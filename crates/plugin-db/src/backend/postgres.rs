//! `PostgresBackend` — the single concrete impl of [`super::Backend`].
//!
//! Wraps the `compio_postgres::Pool` and the configured URL. Every PG-
//! flavoured call moves here so consumer files (`orchestrator/*`,
//! `audit.rs`, `migrations.rs`) can stay free of `compio_postgres::Client`
//! direct references and go through the trait instead.
//!
//! The methods are intentionally thin — they forward to the existing
//! free functions in `crate::audit` / `crate::diff` / `crate::query`
//! that already do the work. The point of this file is to *name the
//! seam*, not to relocate every line of SQL.

use std::cell::RefCell;
use std::rc::Rc;

use crate::diff::LiveSchema;
use crate::error::DbError;

use super::{
    AuditWriter, Backend, DialectBuilder, IndexBuilder, LockManager, NamespaceManager,
    PgLockManager, PgSqlExecutor, SchemaIntrospect, SqlExecutor, VectorIndex, VectorMetric,
};

/// Single concrete impl of [`Backend`] backed by `compio_postgres`.
///
/// Holds the `Rc<Pool>` for the configured URL. The pool itself is
/// created by [`crate::init_pool_async`] and stashed in the per-isolate
/// context; this wrapper just provides the trait facade.
pub struct PostgresBackend {
    pool: Rc<compio_postgres::Pool>,
    /// Configured URL — used by [`Self::acquire_dedicated_client`] to
    /// open a fresh connection outside the pool (for the
    /// `db.beginTransaction()` and `migrationBegin` paths that need a
    /// connection that survives across pool-return points).
    url: String,
    /// **P4 PR 2** — cached pgvector extension presence probe.
    ///
    /// `None` before the first call to [`VectorIndex::ensure_vector_index`]
    /// or [`VectorIndex::vector_search`]; `Some(true)` / `Some(false)`
    /// after the first `SELECT 1 FROM pg_extension WHERE extname='vector'`
    /// round-trip. The probe is per-backend (so per-isolate, since each
    /// isolate carries its own `PostgresBackend` Rc) and stays cached
    /// for the life of the backend — pgvector is provisioned at admin
    /// time and never disappears mid-process. `RefCell` (not `Mutex`)
    /// because every `PostgresBackend` is owned by a single
    /// compio thread.
    pgvector_available: RefCell<Option<bool>>,
}

impl std::fmt::Debug for PostgresBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresBackend").finish()
    }
}

impl PostgresBackend {
    /// Build a backend handle around an already-initialised pool.
    pub fn new(pool: Rc<compio_postgres::Pool>, url: String) -> Self {
        Self {
            pool,
            url,
            pgvector_available: RefCell::new(None),
        }
    }

    /// Borrow the inner pool. Provided for the few places that still
    /// need the raw `Pool` (e.g. the v8_classes layer's `ensure_pool`
    /// shim until the consumer migration completes).
    pub fn pool(&self) -> &Rc<compio_postgres::Pool> {
        &self.pool
    }

    /// Borrow the configured URL.
    pub fn url(&self) -> &str {
        &self.url
    }
}

// ---------------------------------------------------------------------------
// Capability impls — six blocks after P0 PR 2:
//
//   1. `impl SqlExecutor for PostgresBackend`     — PR 1 (3 methods).
//   2. `impl LockManager for PostgresBackend`     — PR 1 (3 methods).
//   3. `impl NamespaceManager for PostgresBackend` — PR 2 (1 method).
//   4. `impl SchemaIntrospect for PostgresBackend` — PR 2 (2 methods +
//      `type LiveSchema`).
//   5. `impl IndexBuilder for PostgresBackend`     — PR 2 (1 method).
//   6. `impl PgSqlExecutor for PostgresBackend`    — PR 2 (1 method —
//      the PG-only `pool_handle()` accessor that lets free-function
//      audit helpers reach `&Pool` without naming `PostgresBackend`).
//
// `impl Backend for PostgresBackend {}` below is a one-line composition
// marker — every operation lives on the sub-trait impls above.
//
// Audit-row operations: see free fns in `crate::audit`. Open Q1
// resolution per `docs/proposals/p0-implementation-plan.md` §3 Q1 +
// §"PR 2"; consumers reach `&compio_postgres::Pool` through
// [`PgSqlExecutor::pool_handle`].
// ---------------------------------------------------------------------------

impl SqlExecutor for PostgresBackend {
    type Client = compio_postgres::Client;

    async fn acquire_dedicated_client(&self) -> Result<Self::Client, DbError> {
        let (client, connection) = compio_postgres::connect(&self.url, compio_postgres::NoTls)
            .await
            .map_err(|e| DbError::Transient {
                message: format!("db: backend connect failed: {e}"),
            })?;
        compio::runtime::spawn(async move {
            if let Err(e) = connection.run().await {
                tracing::error!(error = ?e, "db: backend connection task error");
            }
        })
        .detach();
        Ok(client)
    }

    async fn pool_exec(&self, sql: &str, params: &[&str]) -> Result<u64, DbError> {
        let rows = self
            .pool
            .query_text_params(sql, params)
            .await
            .map_err(|e| DbError::from_pg(&e))?;
        Ok(rows.len() as u64)
    }

    async fn client_exec(
        &self,
        client: &Self::Client,
        sql: &str,
        params: &[&str],
    ) -> Result<u64, DbError> {
        let rows = client
            .query_text_params(sql, params)
            .await
            .map_err(|e| DbError::from_pg(&e))?;
        Ok(rows.len() as u64)
    }
}

impl LockManager for PostgresBackend {
    async fn acquire_advisory_lock(
        &self,
        client: &Self::Client,
        key1: &str,
        key2: &str,
    ) -> Result<(), DbError> {
        // Two-key advisory lock on `(hashtext(key1)::int4,
        // hashtext(key2)::int4)`. Session-scoped — held until the
        // backend session ends or `release_advisory_lock` runs.
        let sql = "SELECT pg_advisory_lock(hashtext($1)::int4, hashtext($2)::int4)";
        client
            .query_text_params(sql, &[key1, key2])
            .await
            .map_err(|e| {
                let mut err = DbError::from_pg(&e);
                // Decorate the message so operators can see which key
                // failed (the bare SQLSTATE message often doesn't show
                // the hash inputs).
                if let DbError::Internal { message }
                | DbError::Transient { message }
                | DbError::LockContention { message } = &mut err
                {
                    *message = format!("db: pg_advisory_lock({key1}, {key2}) failed: {message}");
                }
                err
            })?;
        Ok(())
    }

    async fn try_acquire_advisory_lock(
        &self,
        client: &Self::Client,
        key1: &str,
        key2: &str,
    ) -> Result<bool, DbError> {
        let sql =
            "SELECT pg_try_advisory_lock(hashtext($1)::int4, hashtext($2)::int4) AS got";
        let rows = client
            .query_text_params(sql, &[key1, key2])
            .await
            .map_err(|e| DbError::from_pg(&e))?;
        let got: bool = rows
            .first()
            .map(|r| r.try_get::<_, bool>("got").unwrap_or(false))
            .unwrap_or(false);
        Ok(got)
    }

    async fn release_advisory_lock(
        &self,
        client: &Self::Client,
        key1: &str,
        key2: &str,
    ) -> Result<(), DbError> {
        let sql = "SELECT pg_advisory_unlock(hashtext($1)::int4, hashtext($2)::int4)";
        client
            .query_text_params(sql, &[key1, key2])
            .await
            .map_err(|e| DbError::from_pg(&e))?;
        Ok(())
    }
}

impl NamespaceManager for PostgresBackend {
    async fn ensure_app_schema(&self, app_id: &str) -> Result<(), DbError> {
        // P1 PR 3: the create-schema SQL now flows through the
        // `DialectBuilder::build_ensure_app_schema` hook instead of
        // the free function `crate::query::build_create_schema`. The
        // SQL text is byte-identical to the previous form
        // (`CREATE SCHEMA IF NOT EXISTS "<app>"`) — the structural
        // change is the routing seam, not the statement. Verified by
        // grep audit (PR-3 commit message): no other callers of
        // `query::build_create_schema` exist, so the free function
        // could be removed in a follow-up; we keep it for now as the
        // dialect's `build_ensure_app_schema` impl delegates to the
        // same quoting primitive.
        let create_schema = self.build_ensure_app_schema(app_id);
        let empty: Vec<&str> = Vec::new();
        self.pool
            .query_text_params(&create_schema, &empty)
            .await
            .map_err(|e| DbError::from_pg(&e))?;
        Ok(())
    }
}

impl SchemaIntrospect for PostgresBackend {
    type LiveSchema = LiveSchema;

    async fn introspect_schema(&self, app_id: &str) -> Result<Self::LiveSchema, DbError> {
        // `read_live_schema` now returns typed `DbError` with SQLSTATE
        // classification preserved — flow through verbatim.
        crate::diff::read_live_schema(&self.pool, app_id).await
    }

    async fn estimate_row_count(&self, app_id: &str, collection: &str) -> Result<i64, DbError> {
        crate::diff::estimate_row_count(&self.pool, app_id, collection).await
    }
}

impl IndexBuilder for PostgresBackend {
    async fn create_index_with_recovery(
        &self,
        app_id: &str,
        collection: &str,
        spec: &crate::query::IndexSpec,
        deploy_id: &str,
        schema_version: i32,
    ) -> Result<(), DbError> {
        create_index_with_recovery_audited(
            &self.pool,
            app_id,
            collection,
            spec,
            deploy_id,
            schema_version,
        )
        .await
    }
}

impl PgSqlExecutor for PostgresBackend {
    fn pool_handle(&self) -> &Rc<compio_postgres::Pool> {
        &self.pool
    }
}

// P1 PR 5: `AuditWriter` capability. The PG impl is a thin wrapper over
// the existing `crate::audit::write_audit_row` free function — same SQL,
// same `RETURNING id` round-trip, same error mapping. The trait method
// discards the returned id because the only PR-5 consumer (the SQLite
// arm's `IndexBuilder::create_index_with_recovery`) writes its audit
// row in terminal state and doesn't need to transition it; PG's own
// `create_index_with_recovery_audited` continues to call the free
// function directly so it can chain `update_audit_status` after, no
// behaviour change on the PG audit path.
impl AuditWriter for PostgresBackend {
    async fn write_audit_row(
        &self,
        app_id: &str,
        row: &crate::audit::AuditRow,
    ) -> Result<(), DbError> {
        crate::audit::write_audit_row(self.pool.as_ref(), app_id, row).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// VectorIndex — P4 PR 2 (pgvector adapter)
// ---------------------------------------------------------------------------
//
// Two methods:
//   * `ensure_vector_index` — `CREATE INDEX CONCURRENTLY ... USING ivfflat`
//      routed through the existing audited CIC retry loop so failures
//      land in `__zeroship_migrations` like any other index build.
//   * `vector_search` — `SELECT *, col <op> $1::vector AS _distance
//      FROM ... ORDER BY col <op> $1::vector LIMIT $2` via `build_vector_search`.
//
// Both probe `pg_extension WHERE extname='vector'` on first call and
// cache the result on `pgvector_available`. Probe absence surfaces as
// `DbError::Configuration { code: "vector_extension_missing", ... }`.
// ---------------------------------------------------------------------------

impl PostgresBackend {
    /// Check (and cache) whether the `vector` extension is installed on
    /// the connected database. The probe runs at most once per backend
    /// instance — pgvector is provisioned at admin time and stays present
    /// for the life of the process.
    ///
    /// Returns `Ok(())` when present; `Err(DbError::Configuration)` with
    /// code `vector_extension_missing` otherwise. Connection failures
    /// during the probe surface as `DbError::Transient` so callers can
    /// distinguish "extension missing" from "database unreachable".
    async fn ensure_pgvector_available(&self) -> Result<(), DbError> {
        // Fast path: cached result.
        if let Some(present) = *self.pgvector_available.borrow() {
            if present {
                return Ok(());
            }
            return Err(DbError::config_hinted(
                "vector_extension_missing",
                "pgvector is not installed on this database",
                "run `CREATE EXTENSION vector;` (Postgres superuser) or \
                 swap the database image to `pgvector/pgvector:pg16` \
                 (see docs/runbooks/docker-compose.md)",
            ));
        }

        let empty: Vec<&str> = Vec::new();
        let rows = self
            .pool
            .query_text_params("SELECT 1 FROM pg_extension WHERE extname='vector'", &empty)
            .await
            .map_err(|e| DbError::from_pg(&e))?;
        let present = !rows.is_empty();
        *self.pgvector_available.borrow_mut() = Some(present);
        if present {
            Ok(())
        } else {
            Err(DbError::config_hinted(
                "vector_extension_missing",
                "pgvector is not installed on this database",
                "run `CREATE EXTENSION vector;` (Postgres superuser) or \
                 swap the database image to `pgvector/pgvector:pg16` \
                 (see docs/runbooks/docker-compose.md)",
            ))
        }
    }
}

impl VectorIndex for PostgresBackend {
    async fn ensure_vector_index(
        &self,
        app_id: &str,
        collection: &str,
        column: &str,
        dims: i32,
        metric: VectorMetric,
    ) -> Result<(), DbError> {
        // Probe first — building an ivfflat index against a database
        // without pgvector would fail with a less actionable SQLSTATE.
        self.ensure_pgvector_available().await?;

        // Opclass per metric — see `VectorMetric` rustdoc for the
        // operator/opclass mapping.
        let opclass = match metric {
            VectorMetric::Cosine => "vector_cosine_ops",
            VectorMetric::L2 => "vector_l2_ops",
            VectorMetric::InnerProduct => "vector_ip_ops",
        };

        // Same naming convention as `crate::query::index_name(collection,
        // &[col], false)` — we route through the existing audited CIC
        // retry loop, so the spec we synthesise must follow the same
        // identifier shape `apply.rs` and the audit log expect.
        let idx_name = crate::query::index_name(collection, &[column], /* unique = */ false);
        let sql = format!(
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS {} ON {}.{} USING ivfflat ({} {}) WITH (lists = 100)",
            self.quote_ident(&idx_name),
            self.quote_ident(app_id),
            self.quote_ident(collection),
            self.quote_ident(column),
            opclass,
        );

        let spec = crate::query::IndexSpec {
            name: idx_name,
            columns: vec![column.to_string()],
            unique: false,
            sql,
            kind: crate::query::IndexKind::Vector { dims, metric },
        };

        // Reuse the audited retry loop — pgvector index builds inherit
        // the same INVALID-on-cancel / data-violation / transient
        // classification machinery as every other CIC on the platform.
        // `deploy_id` is `'p4_vector_index'` because `ensure_vector_index`
        // can be called outside the deploy orchestrator (PR 2 wires it
        // via `register_model::apply`, but the trait surface must stay
        // callable from a standalone migration script too); the audit
        // schema accepts arbitrary deploy ids and the `apply.rs` Pass-2
        // caller overrides this when it routes through the trait.
        create_index_with_recovery_audited(
            &self.pool,
            app_id,
            collection,
            &spec,
            "p4_vector_index",
            0,
        )
        .await
    }

    async fn vector_search(
        &self,
        app_id: &str,
        collection: &str,
        column: &str,
        query: &[f32],
        k: usize,
        metric: VectorMetric,
        filter: &serde_json::Value,
    ) -> Result<Vec<serde_json::Value>, DbError> {
        // Probe so a missing extension surfaces with the same typed
        // error shape `ensure_vector_index` produces — the SDK branches
        // on `e.code === "vector_extension_missing"` regardless of
        // which entry point fired.
        self.ensure_pgvector_available().await?;

        let bq = crate::query::build_vector_search(
            app_id, collection, column, query, k, metric, filter,
        )
        .map_err(DbError::from)?;

        let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
        let rows = self
            .pool
            .query_text_params(&bq.sql, &param_refs)
            .await
            .map_err(|e| DbError::from_pg(&e))?;
        Ok(crate::v8_bridge::rows_to_json_value(&rows))
    }
}

impl PgLockManager for PostgresBackend {
    async fn acquire_pooled_client_for_lock<'p>(
        &'p self,
    ) -> Result<compio_postgres::PooledClient<'p>, DbError> {
        // Mirror the pre-PR-3 inline call site at
        // `register_model/bootstrap.rs:103`: pool.get() with the same
        // operator-facing error message so log lines stay grep-able.
        self.pool.get().await.map_err(|e| DbError::Transient {
            message: format!("db: failed to acquire orchestrator client: {e}"),
        })
    }
}

// ---------------------------------------------------------------------------
// PgDialect — the Postgres flavour of `DialectBuilder`. P1 PR 3 lands
// the trait impl on both backends so `query.rs`'s free-function string
// builders can be retargeted onto a dialect-typed entry point in a
// later PR without re-shaping their call sites.
//
// The hooks are pure functions of their inputs (ZST has no state). We
// `impl DialectBuilder for PostgresBackend` directly — there is no
// reason to carry a `PgDialect` field on the backend struct because
// the ZST has nothing to store. The `PgDialect` type is kept around
// only as the documentation anchor; consumers reach the impl through
// `&PostgresBackend`.
// ---------------------------------------------------------------------------

/// Postgres-flavoured dialect. Zero-sized — every method is pure.
///
/// Not instantiated by production code today; the matching trait
/// behaviour lives on `impl DialectBuilder for PostgresBackend` below.
/// Kept as a documentation anchor + so the test module can name the
/// ZST when asserting per-hook output without holding a `Pool`.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct PgDialect;

impl DialectBuilder for PgDialect {
    /// Double-quote with embedded-quote escape. Matches the existing
    /// `crate::query::quote_ident` helper byte-for-byte.
    fn quote_ident(&self, name: &str) -> String {
        format!("\"{}\"", name.replace('"', "\"\""))
    }

    /// `CREATE SCHEMA IF NOT EXISTS "<app_id>"` — the canonical PG
    /// shape. Byte-identical to the pre-PR-3
    /// `crate::query::build_create_schema(app_id)` output, so the
    /// `NamespaceManager::ensure_app_schema` impl can swap without
    /// changing the on-wire SQL.
    fn build_ensure_app_schema(&self, app_id: &str) -> String {
        format!("CREATE SCHEMA IF NOT EXISTS {}", self.quote_ident(app_id))
    }

    /// Return the `IndexSpec::sql` field verbatim. The spec is built
    /// by `crate::query::build_create_indexes` against the PG dialect
    /// already (`CREATE [UNIQUE] INDEX CONCURRENTLY …`); the `online`
    /// flag has no separate consumer at PR 3. PR 5 may reshape this
    /// when SQLite's `IndexBuilder` lands.
    fn build_create_index(
        &self,
        spec: &crate::query::IndexSpec,
        _online: bool,
    ) -> String {
        spec.sql.clone()
    }

    /// PG mapping for the P1 type vocabulary. Each branch is a single
    /// `&'static str` — matches the column-type names PG accepts in a
    /// `CREATE TABLE` DDL.
    fn map_zs_type(&self, zs_type: &str, _opts: &serde_json::Value) -> String {
        match zs_type {
            "text" => "TEXT",
            "bigint" | "int8" => "BIGINT",
            "integer" | "int4" | "int" => "INTEGER",
            "double" => "DOUBLE PRECISION",
            "real" => "REAL",
            "bytes" | "blob" => "BYTEA",
            "numeric" | "decimal" => "NUMERIC",
            "boolean" | "bool" => "BOOLEAN",
            "timestamp" => "TIMESTAMP",
            "timestamptz" => "TIMESTAMPTZ",
            "json" => "JSON",
            "jsonb" => "JSONB",
            other => {
                tracing::debug!(
                    zs_type = other,
                    "PgDialect::map_zs_type: unknown type — defaulting to TEXT"
                );
                "TEXT"
            }
        }
        .to_string()
    }

    /// PG's "now" function. PG also accepts `CURRENT_TIMESTAMP`, but
    /// `NOW()` is the idiomatic form used elsewhere in the codebase.
    fn now_fn(&self) -> &'static str {
        "NOW()"
    }

    // `last_insert_rowid_sql` defaults to `None` on the trait — PG
    // routes through `RETURNING id` instead. No override needed.
}

/// Direct `DialectBuilder` impl on `PostgresBackend` so consumers can
/// hold an `&PostgresBackend` and reach the dialect without naming a
/// separate field. The bodies delegate to the `PgDialect` ZST; rustc
/// inlines the value away because every method is `&self`.
impl DialectBuilder for PostgresBackend {
    fn quote_ident(&self, name: &str) -> String {
        PgDialect.quote_ident(name)
    }

    fn build_ensure_app_schema(&self, app_id: &str) -> String {
        PgDialect.build_ensure_app_schema(app_id)
    }

    fn build_create_index(
        &self,
        spec: &crate::query::IndexSpec,
        online: bool,
    ) -> String {
        PgDialect.build_create_index(spec, online)
    }

    fn map_zs_type(&self, zs_type: &str, opts: &serde_json::Value) -> String {
        PgDialect.map_zs_type(zs_type, opts)
    }

    fn now_fn(&self) -> &'static str {
        PgDialect.now_fn()
    }

    fn last_insert_rowid_sql(&self) -> Option<&'static str> {
        PgDialect.last_insert_rowid_sql()
    }
}

// `Backend` is a pure composition marker after P0 PR 2 — every method
// lives on a sub-trait impl above. Audit-row operations: see free fns
// in `crate::audit`. Open Q1 resolution per p0-implementation-plan.md.
impl Backend for PostgresBackend {}

// ---------------------------------------------------------------------------
// create_index_with_recovery_audited — moved from
// orchestrator::register_model::apply (Stage 8e-R2).
//
// SQLSTATE-driven retry loop for `CREATE INDEX CONCURRENTLY`. Postgres
// CIC can land an INVALID index (a partial build that has to be
// dropped + retried) or fail outright on a UNIQUE conflict. Every
// retry, INVALID detection, and terminal failure writes an
// `index_retry` row to `__zeroship_migrations` so operators can see
// what the cold-start orchestrator did (proposal A3).
//
// Postgres-shaped on purpose — `SqlState` matching is the cleanest
// way to classify the recovery branches, and other backends that
// support online index builds would supply their own equivalent
// behind the same `Backend::create_index_with_recovery` signature.
// ---------------------------------------------------------------------------

async fn create_index_with_recovery_audited(
    pool: &compio_postgres::Pool,
    app_id: &str,
    collection: &str,
    spec: &crate::query::IndexSpec,
    deploy_id: &str,
    schema_version: i32,
) -> Result<(), DbError> {
    use crate::v8_bridge::fmt_db_err;
    use compio_postgres::error::SqlState;

    const MAX_RETRIES: u32 = 3;
    let empty: Vec<&str> = Vec::new();
    let qualified_idx = format!("\"{}\".\"{}\"", app_id, spec.name);
    let drop_idx_sql = format!(
        "DROP INDEX CONCURRENTLY IF EXISTS \"{}\".\"{}\"",
        app_id, spec.name
    );

    // Helper: wrap a JSON envelope `Value` into a `DbError::SchemaRefused`
    // tagged `cic_failed`. `serde_json::to_string` is the single source of
    // truth for escaping — newlines, tabs, or unicode control chars in a
    // Postgres error message stay inside the envelope's `message` field as
    // properly-escaped JSON, and the SDK's `JSON.parse` never sees
    // malformed input (hand-rolled `.replace('"', "\\\"")` did not cover
    // those cases).
    let refuse = |value: serde_json::Value| -> DbError {
        // `to_string` of a `Value` is infallible in practice (the input is
        // already a tree of JSON-representable nodes); the fallback below
        // keeps the function total without resorting to `unwrap`.
        let envelope_json = serde_json::to_string(&value).unwrap_or_else(|_| {
            String::from("{\"code\":\"cic_failed\",\"reason\":\"envelope serialisation failed\"}")
        });
        DbError::SchemaRefused {
            code: "cic_failed",
            envelope_json,
        }
    };

    let log_retry = |reason: &'static str,
                     attempt: u32,
                     sqlstate: Option<String>,
                     error: Option<String>| {
        crate::audit::AuditRow {
            collection: collection.to_string(),
            phase: crate::audit::Phase::Ddl,
            change_class: if spec.unique {
                crate::audit::ChangeClass::Compatible
            } else {
                crate::audit::ChangeClass::Additive
            },
            change_kind: "index_retry".to_string(),
            details: serde_json::json!({
                "reason": reason,
                "attempt": attempt,
                "index_name": spec.name,
                "columns": spec.columns,
                "unique": spec.unique,
                "sqlstate": sqlstate,
                "error": error,
            }),
            ddl_sql: Some(spec.sql.clone()),
            status: crate::audit::InitialStatus::Running,
            deploy_id: deploy_id.to_string(),
            schema_version,
            actor: crate::audit::ActorKind::Auto,
        }
    };

    for attempt in 0..=MAX_RETRIES {
        let create_res = pool.query_text_params(&spec.sql, &empty).await;

        match create_res {
            Ok(_) => {
                let check_sql = format!(
                    "SELECT indisvalid FROM pg_index WHERE indexrelid = '{}'::regclass",
                    qualified_idx.replace('\'', "''")
                );
                // Postgres errors flow through the typed `From<pg::Error>`
                // impl so SQLSTATE classification (UniqueViolation,
                // Transient, …) lives in one place — `DbError::from_pg`.
                let rows = pool.query_text_params(&check_sql, &empty).await?;
                let valid = rows
                    .first()
                    .map(|r| r.try_get::<_, bool>("indisvalid").unwrap_or(false))
                    .unwrap_or(false);
                if valid {
                    return Ok(());
                }

                // INVALID index — audit the retry, drop, and loop.
                let row = log_retry("invalid_index_landed", attempt, None, None);
                if let Ok(id) = crate::audit::write_audit_row(pool, app_id, &row).await {
                    if let Err(audit_err) = crate::audit::update_audit_status(
                        pool,
                        app_id,
                        id,
                        crate::audit::TerminalStatus::Failed,
                        Some("index landed INVALID"),
                    )
                    .await
                    {
                        tracing::warn!(
                            app_id = %app_id,
                            audit_id = id,
                            transition = "Failed/invalid_index",
                            attempt,
                            audit_err = %audit_err,
                            "update_audit_status failed; row stays in 'running' until reset",
                        );
                    }
                }
                let _ = pool.query_text_params(&drop_idx_sql, &empty).await;
                if attempt == MAX_RETRIES {
                    return Err(refuse(serde_json::json!({
                        "code": "validation_refused",
                        "change_kind": "index_retry",
                        "collection": collection,
                        "index": spec.name,
                        "reason": format!(
                            "index repeatedly landed INVALID after {} retries",
                            MAX_RETRIES
                        ),
                    })));
                }
            }
            Err(e) => {
                let code = e.code().cloned();
                let fatal = matches!(
                    code.as_ref(),
                    Some(c) if c == &SqlState::UNIQUE_VIOLATION
                        || c == &SqlState::NOT_NULL_VIOLATION
                        || c == &SqlState::FOREIGN_KEY_VIOLATION
                        || c == &SqlState::CHECK_VIOLATION
                );
                if fatal {
                    let code_str = code.as_ref().map(|c| c.code()).unwrap_or("23xxx");
                    let constraint_kind = if spec.unique { "unique" } else { "index" };
                    let row = log_retry(
                        "data_violation",
                        attempt,
                        Some(code_str.to_string()),
                        Some(fmt_db_err(&e)),
                    );
                    if let Ok(id) = crate::audit::write_audit_row(pool, app_id, &row).await {
                        if let Err(audit_err) = crate::audit::update_audit_status(
                            pool,
                            app_id,
                            id,
                            crate::audit::TerminalStatus::Failed,
                            Some("data violates constraint"),
                        )
                        .await
                        {
                            tracing::warn!(
                                app_id = %app_id,
                                audit_id = id,
                                transition = "Failed/data_violation",
                                sqlstate = code_str,
                                audit_err = %audit_err,
                                "update_audit_status failed; row stays in 'running' until reset",
                            );
                        }
                    }
                    let _ = pool.query_text_params(&drop_idx_sql, &empty).await;
                    return Err(refuse(serde_json::json!({
                        "code": "unique_violation",
                        "sqlstate": code_str,
                        "collection": collection,
                        "constraint": constraint_kind,
                        "index": spec.name,
                        "columns": spec.columns,
                        "message": fmt_db_err(&e),
                    })));
                }

                let transient = matches!(
                    code.as_ref(),
                    Some(c) if c == &SqlState::T_R_DEADLOCK_DETECTED
                        || c == &SqlState::DISK_FULL
                        || c == &SqlState::OUT_OF_MEMORY
                );

                let row = log_retry(
                    if transient {
                        "transient_retry"
                    } else {
                        "non_transient_failure"
                    },
                    attempt,
                    code.as_ref().map(|c| c.code().to_string()),
                    Some(fmt_db_err(&e)),
                );
                if let Ok(id) = crate::audit::write_audit_row(pool, app_id, &row).await {
                    if let Err(audit_err) = crate::audit::update_audit_status(
                        pool,
                        app_id,
                        id,
                        crate::audit::TerminalStatus::Failed,
                        Some("index build failed"),
                    )
                    .await
                    {
                        tracing::warn!(
                            app_id = %app_id,
                            audit_id = id,
                            transition = "Failed/index_build",
                            attempt,
                            transient,
                            audit_err = %audit_err,
                            "update_audit_status failed; row stays in 'running' until reset",
                        );
                    }
                }

                let _ = pool.query_text_params(&drop_idx_sql, &empty).await;
                if !transient || attempt == MAX_RETRIES {
                    return Err(refuse(serde_json::json!({
                        "code": "validation_refused",
                        "change_kind": "index_retry",
                        "collection": collection,
                        "index": spec.name,
                        "sqlstate": code
                            .as_ref()
                            .map(|c| c.code())
                            .unwrap_or("unknown"),
                        "attempts": attempt + 1,
                        "message": fmt_db_err(&e),
                    })));
                }
            }
        }
    }

    // Loop exited without a terminal `return` — the retry budget is
    // exhausted yet the last iteration produced neither a success nor a
    // classified failure. That's an invariant breach, not user input;
    // surface it as a configuration-class error so the operator log can
    // tell it apart from a validation refusal.
    Err(DbError::Configuration {
        code: "cic_configuration",
        message: format!(
            "db: create index '{}' exhausted retry budget without a terminal result",
            spec.name
        ),
        hint: None,
    })
}

#[cfg(test)]
mod tests {
    //! Unit tests for [`PostgresBackend`].
    //!
    //! ## What this layer can — and cannot — test in isolation
    //!
    //! `PostgresBackend` is, by design, a thin facade: every method in
    //! its per-capability impls (`SqlExecutor` / `LockManager` /
    //! `NamespaceManager` / `SchemaIntrospect` / `IndexBuilder`) either
    //! calls the `Rc<Pool>` directly or forwards into [`crate::audit`] /
    //! [`crate::diff`] / [`crate::query`] free functions. After P0 PR 2
    //! `impl Backend for PostgresBackend` is a one-line composition
    //! marker — every method body lives on a sub-trait impl. The only
    //! non-async logic in this file is:
    //!
    //! * [`PostgresBackend::new`] — captures the `pool` + `url` fields.
    //! * [`PostgresBackend::pool`] / [`PostgresBackend::url`] — getters.
    //! * The [`std::fmt::Debug`] impl — deliberately opaque
    //!   ("PostgresBackend").
    //!
    //! Even those need an `Rc<compio_postgres::Pool>` to construct, and
    //! `Pool::connect` requires a live Postgres listener. There is no
    //! stub / no-IO constructor. The async methods need both a Pool
    //! AND a real `Client`; they're exercised by `tests/integration.rs`.
    //!
    //! That leaves *compile-time* tests as the highest-signal coverage
    //! we can add in `--lib`:
    //!
    //! 1. `PostgresBackend: Backend` — proves the trait impl is wired
    //!    up so any future bound change to `Backend` (adding a method,
    //!    tightening a lifetime, swapping an associated type) fails
    //!    compilation here, not at a distant call site.
    //! 2. Associated-type identities — pin `Client = compio_postgres::Client`
    //!    and `LiveSchema = crate::diff::LiveSchema` so a refactor that
    //!    accidentally swaps either is caught here.
    //! 3. The `Backend: 'static` bound on the trait — re-asserted at
    //!    the impl site.
    //!
    //! These are runtime no-ops (the bodies never execute) — they exist
    //! so `cargo build -p zeroship-plugin-db --tests` fails fast on a
    //! seam break.

    use super::*;
    use crate::backend::{
        AuditWriter, Backend, DialectBuilder, IndexBuilder, LockManager, NamespaceManager,
        PgLockManager, PgSqlExecutor, RegisterBackend, SchemaIntrospect, SqlExecutor,
    };

    /// Compile-time: `PostgresBackend` must satisfy the `Backend` trait
    /// (after P0 PR 2 — a pure composition marker over five sub-traits).
    /// The function is never called; the bound is checked at type-check
    /// time.
    fn assert_postgres_backend_impls_backend() {
        fn assert_impl<T: Backend>() {}
        assert_impl::<PostgresBackend>();
    }

    /// Compile-time: each carved capability trait is impl'd directly on
    /// `PostgresBackend` after P0 PR 2 (not just visible through the
    /// `Backend` super-bound). A regression that pulls one back onto
    /// the omnibus trait or detaches the impl block fails here at
    /// build time.
    fn assert_postgres_backend_impls_sub_traits() {
        fn impls_sql_executor<T: SqlExecutor<Client = compio_postgres::Client>>() {}
        fn impls_lock_manager<T: LockManager<Client = compio_postgres::Client>>() {}
        fn impls_namespace_manager<T: NamespaceManager>() {}
        fn impls_schema_introspect<T: SchemaIntrospect<LiveSchema = crate::diff::LiveSchema>>() {}
        fn impls_index_builder<T: IndexBuilder<Client = compio_postgres::Client>>() {}
        fn impls_pg_sql_executor<T: PgSqlExecutor>() {}
        fn impls_pg_lock_manager<T: PgLockManager>() {}
        fn impls_register_backend<T: RegisterBackend>() {}
        impls_sql_executor::<PostgresBackend>();
        impls_lock_manager::<PostgresBackend>();
        impls_namespace_manager::<PostgresBackend>();
        impls_schema_introspect::<PostgresBackend>();
        impls_index_builder::<PostgresBackend>();
        impls_pg_sql_executor::<PostgresBackend>();
        impls_pg_lock_manager::<PostgresBackend>();
        impls_register_backend::<PostgresBackend>();

        // P1 PR 3: `DialectBuilder` impl lands directly on the backend
        // (not on the `Backend` super-trait — the trait composition
        // stays unchanged). The bound here pins the impl so a future
        // refactor that detaches the impl block fails at type-check.
        fn impls_dialect_builder<T: DialectBuilder>() {}
        impls_dialect_builder::<PostgresBackend>();

        // P1 PR 5: `AuditWriter` impl wraps the free-function audit
        // write path. The bound pins the trait wire so a future
        // refactor that detaches the impl block fails at type-check
        // here, not at a distant `IndexBuilder` consumer site.
        fn impls_audit_writer<T: AuditWriter>() {}
        impls_audit_writer::<PostgresBackend>();
    }

    // ---------------------------------------------------------------------
    // P1 PR 3: PgDialect hook unit tests. ZST has no I/O — each test
    // is a string-compare against the expected SQL fragment.
    // ---------------------------------------------------------------------

    #[test]
    fn pg_dialect_quote_ident_doubles_embedded_quote() {
        let d = PgDialect;
        assert_eq!(d.quote_ident("plain"), "\"plain\"");
        assert_eq!(d.quote_ident("with\"quote"), "\"with\"\"quote\"");
    }

    #[test]
    fn pg_dialect_build_ensure_app_schema_matches_legacy_helper() {
        let d = PgDialect;
        // The dialect output MUST equal the legacy
        // `crate::query::build_create_schema` output byte-for-byte —
        // PR-3 rewires `NamespaceManager::ensure_app_schema` through
        // the dialect, and any divergence here changes the wire SQL.
        let legacy = crate::query::build_create_schema("app_demo");
        let dialect = d.build_ensure_app_schema("app_demo");
        assert_eq!(legacy, dialect, "dialect SQL must match legacy helper");
        assert_eq!(dialect, "CREATE SCHEMA IF NOT EXISTS \"app_demo\"");
    }

    #[test]
    fn pg_dialect_map_zs_type_covers_p1_vocabulary() {
        let d = PgDialect;
        let no_opts = serde_json::json!({});
        assert_eq!(d.map_zs_type("text", &no_opts), "TEXT");
        assert_eq!(d.map_zs_type("bigint", &no_opts), "BIGINT");
        assert_eq!(d.map_zs_type("int8", &no_opts), "BIGINT");
        assert_eq!(d.map_zs_type("integer", &no_opts), "INTEGER");
        assert_eq!(d.map_zs_type("int4", &no_opts), "INTEGER");
        assert_eq!(d.map_zs_type("double", &no_opts), "DOUBLE PRECISION");
        assert_eq!(d.map_zs_type("real", &no_opts), "REAL");
        assert_eq!(d.map_zs_type("bytes", &no_opts), "BYTEA");
        assert_eq!(d.map_zs_type("blob", &no_opts), "BYTEA");
        assert_eq!(d.map_zs_type("numeric", &no_opts), "NUMERIC");
        assert_eq!(d.map_zs_type("decimal", &no_opts), "NUMERIC");
        assert_eq!(d.map_zs_type("boolean", &no_opts), "BOOLEAN");
        assert_eq!(d.map_zs_type("timestamp", &no_opts), "TIMESTAMP");
        assert_eq!(d.map_zs_type("timestamptz", &no_opts), "TIMESTAMPTZ");
        assert_eq!(d.map_zs_type("json", &no_opts), "JSON");
        assert_eq!(d.map_zs_type("jsonb", &no_opts), "JSONB");
        // Unknown types fall through to TEXT.
        assert_eq!(d.map_zs_type("nonsense_type", &no_opts), "TEXT");
    }

    #[test]
    fn pg_dialect_now_fn_and_last_insert_rowid() {
        let d = PgDialect;
        assert_eq!(d.now_fn(), "NOW()");
        // PG routes through `RETURNING id` for last-inserted rowid —
        // the trait default of `None` is the correct PG shape.
        assert_eq!(d.last_insert_rowid_sql(), None);
    }

    /// Compile-time: the associated types must remain wired to the
    /// concrete `compio_postgres` / `crate::diff` types. Swapping
    /// either accidentally would silently change the `B::Client` /
    /// `B::LiveSchema` shape every consumer sees. `LiveSchema` is owned
    /// by [`SchemaIntrospect`] after P0 PR 2 — the `Backend` super-bound
    /// `SchemaIntrospect<LiveSchema = LiveSchema>` re-anchors it so
    /// `Backend<LiveSchema = …>` still resolves here.
    fn assert_postgres_backend_assoc_types() {
        fn same_client<T: Backend<Client = compio_postgres::Client>>() {}
        fn same_live_schema<T: Backend<LiveSchema = crate::diff::LiveSchema>>() {}
        same_client::<PostgresBackend>();
        same_live_schema::<PostgresBackend>();
    }

    /// Compile-time: the `Backend: 'static` bound carries through to
    /// the impl. The per-isolate context relies on this to park
    /// `Rc<PostgresBackend>` in a thread-local without explicit lifetime
    /// gymnastics.
    fn assert_postgres_backend_is_static() {
        fn assert_static<T: 'static>() {}
        assert_static::<PostgresBackend>();
    }

    // Runtime side: the only Pool-free observation we can make is on
    // the `Debug` impl shape. It must remain opaque ("PostgresBackend")
    // so accidentally adding a field that exposes the URL or pool
    // internals via #[derive(Debug)] would be caught here.

    #[test]
    fn debug_impl_is_opaque_source_check() {
        // We can't construct a real `PostgresBackend` without a Pool
        // (Pool::connect needs a live Postgres listener). Instead we
        // verify the *source* of the Debug impl: it must render a
        // bare struct name with no fields, so the URL (which may
        // carry credentials) is never printed.
        //
        // Regression guard: if someone switches to `#[derive(Debug)]`,
        // the rendered string would include
        // `pool: Rc { ... }, url: "postgres://..."` and break the
        // assertion below.
        let src = include_str!("postgres.rs");
        let debug_block = src
            .split("impl std::fmt::Debug for PostgresBackend")
            .nth(1)
            .expect("Debug impl present");
        // First `}` that closes the impl block (the impl body has only
        // one inner `fn fmt` whose own braces match).
        let body_end = debug_block
            .find("\n}\n")
            .expect("Debug impl block has a closing brace");
        let body = &debug_block[..body_end];
        assert!(
            body.contains("debug_struct(\"PostgresBackend\")"),
            "Debug impl must use a `debug_struct(\"PostgresBackend\")` builder"
        );
        assert!(
            body.contains(".finish()"),
            "Debug impl must close with `.finish()` (no fields)"
        );
        assert!(
            !body.contains(".field("),
            "Debug impl must NOT expose internal fields — `url` may contain secrets"
        );
    }

    #[test]
    fn compile_time_trait_assertions_link() {
        // Calling the asserter functions ensures rustc keeps them
        // alive and the `unused` lints don't fire. The compile-time
        // checks happen at type-check time on the function body
        // regardless of whether we call them, but the explicit
        // `_ = ...` documents intent and silences `dead_code`.
        let _ = assert_postgres_backend_impls_backend as fn();
        let _ = assert_postgres_backend_impls_sub_traits as fn();
        let _ = assert_postgres_backend_assoc_types as fn();
        let _ = assert_postgres_backend_is_static as fn();
    }
}
