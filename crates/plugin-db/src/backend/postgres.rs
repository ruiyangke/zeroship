//! `PostgresBackend` — the single concrete impl of [`super::Backend`].
//!
//! Wraps the `compio_postgres::Pool` and the configured URL. Every PG-
//! flavoured call moves here so consumer files (`register_model/*`,
//! `transaction/*`, `audit.rs`, `migrations.rs`) can stay free of
//! `compio_postgres::Client` direct references and go through the trait
//! instead.
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
    AuditWriter, DialectBuilder, FullTextIndex, GeoPoint, IndexBuilder, LockManager,
    NamespaceManager, PgLockManager, PgSqlExecutor, SchemaIntrospect, SpatialIndex, SqlExecutor,
    VectorIndex, VectorMetric,
};
#[cfg(any(test, feature = "test-helpers"))]
use super::Backend;

/// Single concrete impl of [`Backend`] backed by `compio_postgres`.
///
/// Holds the `Rc<Pool>` for the configured URL. The pool itself is
/// created by [`crate::init_pool_async`] and stashed in the per-isolate
/// context; this wrapper just provides the trait facade.
pub struct PostgresBackend {
    pool: Rc<compio_postgres::Pool>,
    /// Configured URL — used by [`Self::acquire_dedicated_client`] to
    /// open a fresh connection outside the pool (for the
    /// `db.transaction(fn)` and `migrationBegin` paths that need a
    /// connection that survives across pool-return points).
    url: String,
    /// **P5 PR 2** — per-backend column-key cache. Lazily resolves
    /// `(app_id, key_id) → AeadKey` via the
    /// `__zeroship_admin.get_column_key` SECURITY DEFINER getter; falls
    /// back to `ZEROSHIP_COLUMN_KEY_<KEYID>` env vars when the getter
    /// returns NULL (pre-migration / dev parity). Single-threaded
    /// (`RefCell` inside `KeyStore`) since every `PostgresBackend` is
    /// owned by a single compio thread.
    key_store: crate::encryption::KeyStore,
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
    /// **P4 PR 3** — cached PostGIS extension presence probe.
    ///
    /// Same shape and lifetime semantics as [`Self::pgvector_available`]:
    /// `None` until the first `SpatialIndex::ensure_spatial_index` or
    /// `SpatialIndex::spatial_near` call probes `pg_extension WHERE
    /// extname='postgis'`; `Some(true)` / `Some(false)` after. Cached
    /// for the life of the backend (PostGIS is provisioned at admin
    /// time and stays present). Absence surfaces as
    /// `DbError::Configuration { code: "postgis_extension_missing", … }`
    /// from both entry points so the SDK can branch on `.code`.
    postgis_available: RefCell<Option<bool>>,
}

impl std::fmt::Debug for PostgresBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresBackend").finish()
    }
}

impl PostgresBackend {
    /// Build a backend handle around an already-initialised pool.
    pub fn new(pool: Rc<compio_postgres::Pool>, url: String) -> Self {
        // **P5 PR 2** — wire the column-key store. We clone the `Rc<Pool>`
        // into `KeySource::PgAdminTable` so the `KeyStore`'s
        // `resolve(...)` method can call the SECURITY DEFINER getter
        // without re-reaching into `PostgresBackend`. The pool clone is
        // cheap (Rc inc), and the cache is invalidated naturally on
        // backend drop.
        let key_store = crate::encryption::KeyStore::new(
            crate::encryption::KeySource::PgAdminTable(pool.clone()),
        );
        Self {
            pool,
            url,
            pgvector_available: RefCell::new(None),
            postgis_available: RefCell::new(None),
            key_store,
        }
    }

    /// Borrow the inner pool. Provided for the few places that still
    /// need the raw `Pool` (e.g. the v8_classes layer's `ensure_pool`
    /// shim until the consumer migration completes).
    pub fn pool(&self) -> &Rc<compio_postgres::Pool> {
        &self.pool
    }

    /// Borrow the configured URL.
    #[cfg(feature = "test-helpers")]
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

    async fn pool_exec_ddl(&self, sql: &str) -> Result<(), DbError> {
        // Multi-statement DDL (CREATE TABLE + implicit system-field
        // CREATE INDEXes + `COMMENT ON COLUMN` mask sentinels) must use
        // the simple query protocol — `query_text_params` (extended
        // protocol) rejects it with `cannot insert multiple commands
        // into a prepared statement`. `batch_execute` issues a single
        // `Query` message and runs the `;`-separated statements in one
        // implicit transaction.
        let client = self.pool.get().await.map_err(|e| DbError::from_pg(&e))?;
        client
            .batch_execute(sql)
            .await
            .map_err(|e| DbError::from_pg(&e))
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
        // **Schema-authority P1** — `read_live_schema` now lives in the leaf
        // crate `zeroship-schema` and returns `SchemaError` (it cannot name
        // `DbError`). `From<SchemaError> for DbError` re-creates the exact
        // `coded_sql("diff: …", e)` shape, so the SQLSTATE classification +
        // operator-facing message are preserved verbatim.
        crate::diff::read_live_schema(&self.pool, app_id)
            .await
            .map_err(DbError::from)
    }

    async fn estimate_row_count(&self, app_id: &str, collection: &str) -> Result<i64, DbError> {
        crate::diff::estimate_row_count(&self.pool, app_id, collection)
            .await
            .map_err(DbError::from)
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
    async fn ensure_audit_table(&self, app_id: &str) -> Result<(), DbError> {
        crate::audit::ensure_audit_table_exists(self.pool.as_ref(), app_id).await
    }

    async fn next_schema_version(&self, app_id: &str) -> Result<i32, DbError> {
        crate::audit::next_schema_version(self.pool.as_ref(), app_id).await
    }

    async fn write_audit_row_returning_id(
        &self,
        app_id: &str,
        row: &crate::audit::AuditRow,
    ) -> Result<i64, DbError> {
        crate::audit::write_audit_row(self.pool.as_ref(), app_id, row).await
    }

    async fn update_audit_status(
        &self,
        app_id: &str,
        id: i64,
        new_status: crate::audit::TerminalStatus,
        error: Option<&str>,
    ) -> Result<bool, DbError> {
        crate::audit::update_audit_status(self.pool.as_ref(), app_id, id, new_status, error).await
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

        let schema_hint = crate::context::with(|c| c.schema_for(app_id, collection));
        let bq = crate::query::build_vector_search(
            app_id,
            collection,
            column,
            query,
            k,
            metric,
            filter,
            schema_hint.as_ref(),
        )
        .map_err(DbError::from)?;

        let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
        let rows =
            crate::exec::query_postgres_pool_with_autocommit_role(&self.pool, app_id, &bq.sql, &param_refs)
                .await?;
        Ok(crate::v8_bridge::rows_to_json_value(&rows))
    }
}

// ---------------------------------------------------------------------------
// FullTextIndex — P4 PR 3 (tsvector + GIN + tsvector_update_trigger)
// ---------------------------------------------------------------------------
//
// Two methods:
//   * `ensure_fts_index` — four idempotent statements:
//       1. ADD COLUMN IF NOT EXISTS "__fts" tsvector
//       2. backfill the column for rows where it's NULL
//       3. CREATE INDEX CONCURRENTLY IF NOT EXISTS "..."__fts_idx
//          USING GIN ("__fts")
//       4. CREATE TRIGGER "..."__fts_trg BEFORE INSERT OR UPDATE OF
//          <cols> EXECUTE FUNCTION tsvector_update_trigger(...)
//   * `fts_search` — `WHERE __fts @@ plainto_tsquery($1) ORDER BY
//     ts_rank(__fts, plainto_tsquery($1)) DESC LIMIT $2`.
//
// No extension probe: tsvector + GIN + plainto_tsquery +
// tsvector_update_trigger are all part of core Postgres — they ship in
// every supported PG image, including the `postgres:16` default. No
// `CREATE EXTENSION` needed.
// ---------------------------------------------------------------------------

impl FullTextIndex for PostgresBackend {
    async fn ensure_fts_index(
        &self,
        app_id: &str,
        collection: &str,
        columns: &[String],
        language: &str,
    ) -> Result<(), DbError> {
        if columns.is_empty() {
            // Nothing to index. Refuse loudly so a SDK bug emitting an
            // empty FTS index spec gets surfaced rather than silently
            // becoming a no-op (which would later present as "search
            // returns nothing" without any logged cause).
            return Err(DbError::Configuration {
                code: "fts_no_columns",
                message: "db: ensure_fts_index requires at least one source column"
                    .to_string(),
                hint: Some(
                    "mark at least one t.string() field with `.fts()` in the schema"
                        .to_string(),
                ),
            });
        }

        // Whitelist the language token against the same character set we
        // allow in identifiers — splicing it into a SQL literal is safe
        // because plainto_tsquery accepts it verbatim, but we still
        // reject anything that smells of injection. The SDK already
        // restricts the language token at validate time; this is
        // defense-in-depth.
        if language.is_empty()
            || !language
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(DbError::Configuration {
                code: "fts_invalid_language",
                message: format!(
                    "db: ensure_fts_index language {language:?} must be [A-Za-z0-9_]+"
                ),
                hint: Some("use a tsvector configuration name such as 'english' or 'simple'".to_string()),
            });
        }

        // DB-9: validate every FTS source column with the identifier fence
        // BEFORE it is spliced UNQUOTED into the `tsvector_update_trigger` arg
        // list below. That arg list requires bare column names, so quoting is
        // not an option — validation is the only guard. Today CreateTable
        // validates these columns first, but that is a non-local cross-statement
        // ordering invariant; enforce it locally so a direct or FTS-only
        // re-apply against an unvalidated spec can't inject DDL via a column name.
        validate_fts_columns(columns)?;

        let qschema = self.quote_ident(app_id);
        let qcoll = self.quote_ident(collection);
        let qtable = format!("{qschema}.{qcoll}");
        let idx_name = format!("{collection}__fts_idx");
        let trg_name = format!("{collection}__fts_trg");
        let qidx = self.quote_ident(&idx_name);
        let qtrg = self.quote_ident(&trg_name);
        let qfts_col = self.quote_ident("__fts");

        // For the tsvector backfill + trigger we need both the unquoted
        // form (passed as a positional arg to `tsvector_update_trigger`)
        // and the safely-quoted form (spliced into the UPDATE / column
        // list). The trigger function only accepts column NAMES (not
        // dotted identifiers), so the unquoted form is what PG expects
        // there.
        let qcols_csv: String = columns
            .iter()
            .map(|c| self.quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        let unquoted_cols_csv: String = columns.join(", ");
        let coalesce_concat: String = columns
            .iter()
            .map(|c| format!("coalesce({}, '')", self.quote_ident(c)))
            .collect::<Vec<_>>()
            .join(" || ' ' || ");

        let empty: Vec<&str> = Vec::new();

        // 1. Add the tsvector column. Idempotent via IF NOT EXISTS.
        let add_col_sql =
            format!("ALTER TABLE {qtable} ADD COLUMN IF NOT EXISTS {qfts_col} tsvector");
        self.pool
            .query_text_params(&add_col_sql, &empty)
            .await
            .map_err(|e| DbError::from_pg(&e))?;

        // 2. Backfill rows where the tsvector column is still NULL.
        //    A second registerModel call after schema change re-runs the
        //    backfill on any rows where new source columns nulled the
        //    derived value, but typical case is the post-ADD-COLUMN seed.
        let backfill_sql = format!(
            "UPDATE {qtable} SET {qfts_col} = to_tsvector('pg_catalog.{language}', {coalesce_concat}) WHERE {qfts_col} IS NULL"
        );
        self.pool
            .query_text_params(&backfill_sql, &empty)
            .await
            .map_err(|e| DbError::from_pg(&e))?;

        // 3. CREATE INDEX CONCURRENTLY. We do NOT route through the
        //    audited retry loop here because the GIN over tsvector index
        //    cannot land INVALID the same way ivfflat can (no NULL-key
        //    edge cases). Idempotent via IF NOT EXISTS.
        let cic_sql = format!(
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS {qidx} ON {qtable} USING GIN ({qfts_col})"
        );
        self.pool
            .query_text_params(&cic_sql, &empty)
            .await
            .map_err(|e| DbError::from_pg(&e))?;

        // 4. CREATE TRIGGER. Idempotent via DROP TRIGGER IF EXISTS +
        //    CREATE TRIGGER (PG 14+ supports `CREATE OR REPLACE TRIGGER`
        //    but we stay compatible with PG 13/14 minimum since the
        //    rest of the codebase doesn't pin a higher minimum).
        let drop_trg_sql = format!("DROP TRIGGER IF EXISTS {qtrg} ON {qtable}");
        self.pool
            .query_text_params(&drop_trg_sql, &empty)
            .await
            .map_err(|e| DbError::from_pg(&e))?;
        // tsvector_update_trigger args:
        //   1. target column NAME (unquoted, but the trigger function
        //      tokenises by identifier rules — quoting with the standard
        //      identifier syntax is safe).
        //   2. text-cast config name (we pin pg_catalog.<lang> so the
        //      config resolution is deterministic regardless of search_path).
        //   3..n. source column names (unquoted; the trigger reads the
        //      NEW row by name).
        let create_trg_sql = format!(
            "CREATE TRIGGER {qtrg} BEFORE INSERT OR UPDATE OF {qcols_csv} \
             ON {qtable} FOR EACH ROW EXECUTE FUNCTION \
             tsvector_update_trigger(__fts, 'pg_catalog.{language}', {unquoted_cols_csv})"
        );
        self.pool
            .query_text_params(&create_trg_sql, &empty)
            .await
            .map_err(|e| DbError::from_pg(&e))?;

        Ok(())
    }

    async fn fts_search(
        &self,
        app_id: &str,
        collection: &str,
        query: &str,
        filter: &serde_json::Value,
        limit: Option<usize>,
    ) -> Result<Vec<serde_json::Value>, DbError> {
        let schema_hint = crate::context::with(|c| c.schema_for(app_id, collection));
        let bq = crate::query::build_fts_search(
            app_id,
            collection,
            query,
            filter,
            limit,
            schema_hint.as_ref(),
        )
        .map_err(DbError::from)?;
        let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
        let rows =
            crate::exec::query_postgres_pool_with_autocommit_role(&self.pool, app_id, &bq.sql, &param_refs)
                .await?;
        Ok(crate::v8_bridge::rows_to_json_value(&rows))
    }
}

// ---------------------------------------------------------------------------
// SpatialIndex — P4 PR 3 (PostGIS adapter)
// ---------------------------------------------------------------------------
//
// Two methods:
//   * `ensure_spatial_index` — `CREATE INDEX CONCURRENTLY … USING GIST
//     ("col")`. Routes through the audited CIC retry loop like the
//     vector adapter does — same INVALID-on-cancel / data-violation /
//     transient classification machinery.
//   * `spatial_near` — `WHERE ST_DWithin(col, ST_MakePoint(lng, lat)::
//     geography, radius) ORDER BY ST_Distance(...) LIMIT $4`.
//
// Both probe `pg_extension WHERE extname='postgis'` on first call and
// cache on `postgis_available`. Absence surfaces as
// `DbError::Configuration { code: "postgis_extension_missing", ... }`.
// ---------------------------------------------------------------------------

impl PostgresBackend {
    /// Check (and cache) whether the `postgis` extension is installed on
    /// the connected database. Mirrors [`Self::ensure_pgvector_available`]
    /// — the probe runs at most once per backend; PostGIS is
    /// provisioned at admin time and stays present.
    async fn ensure_postgis_available(&self) -> Result<(), DbError> {
        if let Some(present) = *self.postgis_available.borrow() {
            if present {
                return Ok(());
            }
            return Err(DbError::config_hinted(
                "postgis_extension_missing",
                "PostGIS is not installed on this database",
                "run `CREATE EXTENSION postgis;` (Postgres superuser) or \
                 swap the database image to a PostGIS-bundled variant \
                 (see docs/runbooks/docker-compose.md)",
            ));
        }

        let empty: Vec<&str> = Vec::new();
        let rows = self
            .pool
            .query_text_params(
                "SELECT 1 FROM pg_extension WHERE extname='postgis'",
                &empty,
            )
            .await
            .map_err(|e| DbError::from_pg(&e))?;
        let present = !rows.is_empty();
        *self.postgis_available.borrow_mut() = Some(present);
        if present {
            Ok(())
        } else {
            Err(DbError::config_hinted(
                "postgis_extension_missing",
                "PostGIS is not installed on this database",
                "run `CREATE EXTENSION postgis;` (Postgres superuser) or \
                 swap the database image to a PostGIS-bundled variant \
                 (see docs/runbooks/docker-compose.md)",
            ))
        }
    }
}

impl SpatialIndex for PostgresBackend {
    async fn ensure_spatial_index(
        &self,
        app_id: &str,
        collection: &str,
        column: &str,
    ) -> Result<(), DbError> {
        self.ensure_postgis_available().await?;

        let idx_name = crate::query::index_name(collection, &[column], /* unique = */ false);
        let sql = format!(
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS {} ON {}.{} USING GIST ({})",
            self.quote_ident(&idx_name),
            self.quote_ident(app_id),
            self.quote_ident(collection),
            self.quote_ident(column),
        );

        let spec = crate::query::IndexSpec {
            name: idx_name,
            columns: vec![column.to_string()],
            unique: false,
            sql,
            kind: crate::query::IndexKind::Spatial,
        };

        create_index_with_recovery_audited(
            &self.pool,
            app_id,
            collection,
            &spec,
            "p4_spatial_index",
            0,
        )
        .await
    }

    async fn spatial_near(
        &self,
        app_id: &str,
        collection: &str,
        column: &str,
        point: GeoPoint,
        radius_m: f64,
        filter: &serde_json::Value,
        limit: Option<usize>,
    ) -> Result<Vec<serde_json::Value>, DbError> {
        self.ensure_postgis_available().await?;

        let schema_hint = crate::context::with(|c| c.schema_for(app_id, collection));
        let bq = crate::query::build_spatial_near(
            app_id,
            collection,
            column,
            point,
            radius_m,
            filter,
            limit,
            schema_hint.as_ref(),
        )
        .map_err(DbError::from)?;
        let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
        let rows =
            crate::exec::query_postgres_pool_with_autocommit_role(&self.pool, app_id, &bq.sql, &param_refs)
                .await?;
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
    fn sql_dialect(&self) -> crate::query::SqlDialect {
        crate::query::SqlDialect::Postgres
    }

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
    fn sql_dialect(&self) -> crate::query::SqlDialect {
        PgDialect.sql_dialect()
    }

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
#[cfg(any(test, feature = "test-helpers"))]
impl Backend for PostgresBackend {}

// ---------------------------------------------------------------------------
// create_index_with_recovery_audited — moved from
// register_model::apply (Stage 8e-R2).
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

// ===========================================================================
// P5 — EncryptedColumn (PR 2) + Backup (PR 4) impls on PostgresBackend
// ===========================================================================
//
// Both impls are unconditional on the PG arm:
//   * `EncryptedColumn` (PR 2) — PG-prod key sourcing reads from
//     `__zeroship_admin.column_keys` via a SECURITY DEFINER getter,
//     with an env-var fallback for dev parity.
//   * `Backup` (PR 4) — the PITR placeholder writes to
//     `__zeroship_admin.pitr_targets`. Snapshot / restore themselves don't strictly
//     need the admin schema; `BackendHandle::as_backup_pg` matches the
//     `as_encrypted_column_pg` shape.
//
// The PR-1 stub bodies (returning `Configuration { code: "p5_pr2_stub" }`)
// have been removed now that both impls are real.

// **P5 PR 2** — Real `EncryptedColumn` body. Delegates to the workspace
// `crate::encryption::aead` module (mode-dispatch on encrypt; mode-
// agnostic on decrypt because the wire format carries the nonce). Key
// resolution goes through `self.key_store` which prefers the
// SECURITY DEFINER getter and falls back to env-var sourcing.
impl crate::backend::EncryptedColumn for PostgresBackend {
    type KeyHandle = crate::encryption::aead::AeadKey;

    async fn resolve_key(
        &self,
        app_id: &str,
        key_id: &str,
    ) -> Result<Self::KeyHandle, DbError> {
        self.key_store.resolve(app_id, key_id).await
    }

    fn encrypt(
        &self,
        key: &Self::KeyHandle,
        mode: crate::backend::EncryptionMode,
        plaintext: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, DbError> {
        match mode {
            crate::backend::EncryptionMode::Randomised => {
                crate::encryption::aead::encrypt_randomised(key, plaintext, aad)
            }
            crate::backend::EncryptionMode::Deterministic => {
                crate::encryption::aead::encrypt_deterministic(key, plaintext, aad)
            }
        }
    }

    fn decrypt(
        &self,
        key: &Self::KeyHandle,
        _mode: crate::backend::EncryptionMode,
        ciphertext: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, DbError> {
        // Decrypt is mode-agnostic: the wire format carries the nonce,
        // and AES-GCM verifies the tag regardless of how the nonce was
        // produced on the write side. The caller picks the
        // mode-appropriate AAD (Camp A: row_pk in AAD for Randomised,
        // omitted for Deterministic) — see
        // `crate::crud::encryption_pass`.
        crate::encryption::aead::decrypt(key, ciphertext, aad)
    }
}

// ===========================================================================
// P5 PR 4 — Real `Backup` impl on PostgresBackend
// ===========================================================================
//
// `snapshot` → `pg_dump --schema=<app_id> --format=custom --no-owner
// --no-privileges`, stream the dump to a file URI, hash the bytes
// on the fly. `restore` → drop-and-recreate schema then `pg_restore`.
// `pitr_replay` → records the target row in `__zeroship_admin.pitr_targets`;
// the operator runs the actual recovery via `recovery.conf`.
//
// Notes:
//   1. The PITR placeholder writes to the `__zeroship_admin` schema,
//      provisioned by the auth/bootstrap subtree.
//   2. `Backup` is admin-tier surface — app code never reaches it; the
//      `BackendHandle::as_backup_pg` accessor is the only entry point and
//      mirrors the `as_encrypted_column_pg` shape.
//
// Both `pg_dump` and `pg_restore` need to be on `PATH` in the deployment
// environment. The integration tests `#[ignore]` themselves when the
// binaries aren't reachable so CI doesn't hard-fail on minimal images.
//
// The child process is driven via `std::process::Command` wrapped in
// `compio::runtime::spawn_blocking` — keeps `compio`'s workspace feature
// set unchanged (no `process` feature dep) and matches the shell-out
// pattern used elsewhere in the codebase (e.g. `sandbox-agent/src/exec.rs`).

#[cfg(feature = "test-helpers")]
impl crate::backend::Backup for PostgresBackend {
    async fn snapshot(
        &self,
        app_id: &str,
        dest_uri: &str,
        opts: crate::backend::SnapshotOpts,
    ) -> Result<crate::backend::SnapshotHandle, DbError> {
        backup_pg::snapshot_impl(self, app_id, dest_uri, opts).await
    }

    async fn restore(
        &self,
        app_id: &str,
        snapshot: &crate::backend::SnapshotHandle,
    ) -> Result<(), DbError> {
        backup_pg::restore_impl(self, app_id, snapshot).await
    }

    async fn pitr_replay(
        &self,
        app_id: &str,
        target: crate::backend::PitrTarget,
    ) -> Result<(), DbError> {
        backup_pg::pitr_replay_impl(self, app_id, target).await
    }
}

/// Inner module so the helpers stay grouped and the surrounding file
/// keeps the "thin trait facade + per-capability impl block" shape.
/// `pub(super)` so the trait methods above can call in; everything
/// else stays private.
#[cfg(feature = "test-helpers")]
mod backup_pg {
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::PostgresBackend;
    use crate::backend::{
        BusyPolicy, LockGuard, LockScope, PgLockManager, PitrTarget, SnapshotHandle, SnapshotOpts,
    };
    use crate::error::DbError;
    use crate::register_model::bootstrap::LOCK_TAG as REGISTER_MODEL_LOCK_TAG;

    /// Parse a `file:///abs/path` URI into the underlying filesystem
    /// path. Returns a typed `Configuration` error for unsupported
    /// schemes (e.g. `s3://`) so the SDK can branch on `.code`.
    ///
    /// PR 4 supports only the `file://` scheme — S3 / R2 land alongside
    /// the production BlobStore wire-through in a later PR. The path
    /// is the on-disk address of the dump artifact; SHA-256 hashing
    /// happens on write (snapshot) and re-verification on read
    /// (restore). The handle's `uri` field echoes the caller-supplied
    /// string so dashboards and operators can correlate the snapshot
    /// back to wherever they asked for it to land.
    fn parse_dest_path(dest_uri: &str) -> Result<PathBuf, DbError> {
        if let Some(rest) = dest_uri.strip_prefix("file://") {
            // RFC 8089: `file:///abs/path` — the empty authority leaves
            // `rest` starting with `/`. We accept both `file:///x` and
            // `file://x` here since callers in tests sometimes elide
            // the empty authority.
            Ok(PathBuf::from(rest))
        } else if dest_uri.starts_with("s3://") || dest_uri.starts_with("https://") {
            Err(DbError::Configuration {
                code: "backup_dest_uri_unsupported",
                message: format!(
                    "snapshot destination URI {dest_uri:?} uses an unsupported scheme; \
                     PR 4 ships `file://` only — S3/HTTPS land alongside the production \
                     BlobStore wire-through in a later PR"
                ),
                hint: Some(
                    "use `file:///abs/path/to/snapshot.dump` in P5 PR 4; \
                     S3/R2 destinations are deferred"
                        .to_string(),
                ),
            })
        } else {
            // Treat anything else as a bare filesystem path so the
            // operator can pass either form. `file://` is the documented
            // shape per `SnapshotHandle::uri` rustdoc.
            Ok(PathBuf::from(dest_uri))
        }
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Stream `path` through SHA-256 and return the 32-byte digest.
    /// Reads in 64 KiB chunks; runs entirely on the compio thread (no
    /// `spawn_blocking`) — the snapshot path is operator-driven and not
    /// hot enough to warrant offloading. The std-fs blocking reads
    /// dominate only for multi-GiB snapshots, at which point the whole
    /// op is already gated by pg_dump latency.
    fn sha256_file(path: &Path) -> Result<[u8; 32], std::io::Error> {
        use sha2::Digest;
        let mut file = std::fs::File::open(path)?;
        let mut hasher = sha2::Sha256::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hasher.finalize().into())
    }

    /// Classify a `pg_dump` / `pg_restore` failure by stderr substring
    /// match. The set of patterns is intentionally narrow — only the
    /// shapes the SDK needs to branch on get a coded variant; the rest
    /// pass through as `Internal` with the raw stderr in the message
    /// so the operator can read it from the log.
    fn classify_pg_tool_failure(
        op: &'static str,
        stderr: &str,
        if_busy: BusyPolicy,
    ) -> DbError {
        // `pg_dump: error: connection to server ... failed: …` —
        // transient infra issue. When the caller asked for Retry we
        // mark it retryable; otherwise propagate the same code so the
        // SDK can branch on `.code === "backup_busy"`.
        if stderr.contains("connection to server")
            || stderr.contains("connection to database failed")
            || stderr.contains("could not connect to server")
        {
            return DbError::Coded {
                code: "backup_busy".to_string(),
                message: format!(
                    "{op} could not reach Postgres: {}",
                    stderr.trim().lines().next().unwrap_or(stderr.trim())
                ),
                hint: match if_busy {
                    BusyPolicy::Retry => Some(
                        "transient — retry after the database accepts connections again"
                            .to_string(),
                    ),
                    BusyPolicy::Abort => None,
                },
            };
        }
        // `pg_dump: error: relation "<app>.<table>" does not exist` /
        // schema not found.
        if stderr.contains("does not exist")
            || stderr.contains("no matching schemas were found")
        {
            return DbError::Configuration {
                code: "snapshot_app_unknown",
                message: format!(
                    "{op}: source app schema is missing — {}",
                    stderr.trim().lines().next().unwrap_or(stderr.trim())
                ),
                hint: Some(
                    "verify the app_id matches a schema that exists on this database"
                        .to_string(),
                ),
            };
        }
        // Everything else: Internal with the raw stderr so the
        // operator can debug. We keep the message bounded so a verbose
        // `pg_restore --verbose` dump doesn't flood the JS console.
        let mut truncated = stderr.trim().to_string();
        if truncated.len() > 4096 {
            truncated.truncate(4096);
            truncated.push_str("\n…[truncated]");
        }
        DbError::Internal {
            message: format!("{op} failed: {truncated}"),
        }
    }

    /// Run a `pg_dump` / `pg_restore` subprocess on a blocking worker
    /// and return its output. The connection URL is passed via the
    /// `--dbname=` long arg so it doesn't show up in `ps` output
    /// (modern pg tools mask the password component, but we still
    /// prefer the explicit form).
    async fn run_pg_tool(
        binary: &'static str,
        args: Vec<String>,
    ) -> Result<std::process::Output, std::io::Error> {
        compio::runtime::spawn_blocking(move || {
            std::process::Command::new(binary).args(&args).output()
        })
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("{binary}: spawn_blocking task panicked"),
            )
        })?
    }

    pub(super) async fn snapshot_impl(
        backend: &PostgresBackend,
        app_id: &str,
        dest_uri: &str,
        opts: SnapshotOpts,
    ) -> Result<SnapshotHandle, DbError> {
        // Pre-flight: hold the per-app `register_model` lock for the
        // duration of pg_dump so no concurrent deploy mutates the
        // schema while we capture it. Contention surfaces as
        // `migration_in_progress` regardless of `opts.if_busy` — the
        // SDK branches on `.code` and the caller chooses to retry.
        let lock_client = backend.acquire_pooled_client_for_lock().await?;
        let scope = LockScope::GlobalApp {
            app_id: app_id.to_string(),
            name: REGISTER_MODEL_LOCK_TAG.to_string(),
        };
        let guard = match LockGuard::acquire(backend, lock_client, &scope).await {
            Ok(g) => g,
            Err(DbError::LockContention { message }) => {
                return Err(DbError::Coded {
                    code: "migration_in_progress".to_string(),
                    message: format!(
                        "snapshot: another deploy / migration is in progress for app {app_id:?}: \
                         {message}"
                    ),
                    hint: Some(
                        "retry the snapshot once the in-flight register_model / migration \
                         completes"
                            .to_string(),
                    ),
                });
            }
            Err(other) => return Err(other),
        };

        // Parse destination + ensure parent dir exists.
        let dest_path = parse_dest_path(dest_uri)?;
        if let Some(parent) = dest_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| DbError::Internal {
                    message: format!(
                        "snapshot: create parent dir {parent:?} failed: {e}"
                    ),
                })?;
            }
        }

        // pg_dump invocation. `--no-owner --no-privileges` makes the
        // dump restore-portable across operator roles; `--format=custom`
        // is the input format `pg_restore` consumes.
        let url = backend.url().to_string();
        let app_id_owned = app_id.to_string();
        let dest_path_str = dest_path.to_string_lossy().into_owned();
        let args = vec![
            format!("--dbname={url}"),
            format!("--schema={app_id_owned}"),
            "--format=custom".to_string(),
            "--no-owner".to_string(),
            "--no-privileges".to_string(),
            format!("--file={dest_path_str}"),
        ];

        let output = match run_pg_tool("pg_dump", args).await {
            Ok(o) => o,
            Err(e) => {
                // Best-effort lock release on Err. Drop logs a leak
                // notice on failure; the session-scoped lock auto-
                // releases when the pool recycles the connection.
                let _ = guard.release().await;
                return Err(DbError::Configuration {
                    code: "pg_dump_unavailable",
                    message: format!("pg_dump spawn failed: {e}"),
                    hint: Some(
                        "ensure `pg_dump` is on PATH in the deployment environment \
                         (matches the server major version)"
                            .to_string(),
                    ),
                });
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let _ = guard.release().await;
            // Cleanup partial file so a re-run doesn't see stale bytes.
            let _ = std::fs::remove_file(&dest_path);
            return Err(classify_pg_tool_failure("pg_dump", &stderr, opts.if_busy));
        }

        // Compute the content hash over the persisted file.
        let content_hash = match sha256_file(&dest_path) {
            Ok(h) => h,
            Err(e) => {
                let _ = guard.release().await;
                let _ = std::fs::remove_file(&dest_path);
                return Err(DbError::Internal {
                    message: format!(
                        "snapshot: SHA-256 of {dest_path_str:?} failed: {e}"
                    ),
                });
            }
        };

        // Release the lock now that the dump is committed to disk.
        // From this point onwards concurrent deploys can proceed; the
        // returned handle is enough for a later restore to verify
        // integrity independent of the live schema.
        let _ = guard.release().await;

        Ok(SnapshotHandle {
            uri: dest_uri.to_string(),
            content_hash,
            created_at_ms: now_ms(),
        })
    }

    pub(super) async fn restore_impl(
        backend: &PostgresBackend,
        app_id: &str,
        snapshot: &SnapshotHandle,
    ) -> Result<(), DbError> {
        // Hold the per-app register_model lock for the whole restore.
        // This prevents concurrent deploys from racing the
        // drop-and-recreate sequence — without it, a register_model
        // running mid-restore would see an empty schema and CREATE
        // its own tables on top of whatever pg_restore is rebuilding.
        let lock_client = backend.acquire_pooled_client_for_lock().await?;
        let scope = LockScope::GlobalApp {
            app_id: app_id.to_string(),
            name: REGISTER_MODEL_LOCK_TAG.to_string(),
        };
        let guard = match LockGuard::acquire(backend, lock_client, &scope).await {
            Ok(g) => g,
            Err(DbError::LockContention { message }) => {
                return Err(DbError::Coded {
                    code: "migration_in_progress".to_string(),
                    message: format!(
                        "restore: another deploy / migration is in progress for app {app_id:?}: \
                         {message}"
                    ),
                    hint: Some(
                        "retry the restore once the in-flight register_model / migration \
                         completes"
                            .to_string(),
                    ),
                });
            }
            Err(other) => return Err(other),
        };

        // Resolve the on-disk path; PR 4 supports file:// only.
        let src_path = match parse_dest_path(&snapshot.uri) {
            Ok(p) => p,
            Err(e) => {
                let _ = guard.release().await;
                return Err(e);
            }
        };

        // Verify the content hash matches what `snapshot` recorded.
        // A mismatch means either the file was truncated/corrupted in
        // transit or the operator pointed us at the wrong dump. Refuse
        // before touching the live schema.
        match sha256_file(&src_path) {
            Ok(observed) if observed == snapshot.content_hash => { /* ok */ }
            Ok(_) => {
                let _ = guard.release().await;
                return Err(DbError::Coded {
                    code: "snapshot_hash_mismatch".to_string(),
                    message: format!(
                        "restore: SHA-256 of {:?} does not match the SnapshotHandle's \
                         recorded hash — snapshot is corrupt or this is the wrong file",
                        src_path
                    ),
                    hint: Some(
                        "re-fetch the snapshot from the original source; do NOT run \
                         restore against a dump whose hash has drifted"
                            .to_string(),
                    ),
                });
            }
            Err(e) => {
                let _ = guard.release().await;
                return Err(DbError::Internal {
                    message: format!(
                        "restore: SHA-256 of {src_path:?} failed: {e}"
                    ),
                });
            }
        }

        // Drop the live schema so pg_restore can rebuild it from
        // the dump's TOC. This is the load-bearing safety step's
        // PR-4 simplification: the full `swap_schema_atomic` SECURITY
        // DEFINER function lands in P6a hardening. The simple
        // sequence is destructive — if pg_restore fails after the
        // DROP, the schema is gone and the operator has to re-
        // restore. The lock above keeps concurrent deploys out of
        // the window; the snapshot hash above keeps wrong dumps out.
        //
        // We DROP but do NOT pre-CREATE the schema: pg_dump's custom
        // format emits its own `CREATE SCHEMA "<app_id>"` statement
        // in the TOC, and pre-creating would trip pg_restore with
        // `schema … already exists`. The CASCADE drop kills the
        // schema's tables / indexes / sequences; pg_restore rebuilds
        // the full graph (schema + objects).
        //
        // Run via the pool (not the locked client) so a SQL error
        // doesn't drop the lock. We use raw quoted identifiers; app
        // ids reaching this surface are platform-controlled (typed_id
        // entity prefixes), not user input.
        let drop_sql = format!(r#"DROP SCHEMA IF EXISTS "{app_id}" CASCADE"#);
        let empty: Vec<&str> = Vec::new();
        if let Err(e) = backend
            .pool()
            .query_text_params(&drop_sql, &empty)
            .await
        {
            let _ = guard.release().await;
            return Err(DbError::Internal {
                message: format!("restore: DROP SCHEMA failed: {}", DbError::from_pg(&e)),
            });
        }

        // pg_restore. The dump's TOC includes a `CREATE SCHEMA` so
        // we don't pre-create. `--no-owner --no-privileges` matches
        // the dump-side flags so role-rewriting doesn't trip.
        let url = backend.url().to_string();
        let src_path_str = src_path.to_string_lossy().into_owned();
        let args = vec![
            format!("--dbname={url}"),
            "--no-owner".to_string(),
            "--no-privileges".to_string(),
            "--exit-on-error".to_string(),
            src_path_str.clone(),
        ];

        let output = match run_pg_tool("pg_restore", args).await {
            Ok(o) => o,
            Err(e) => {
                let _ = guard.release().await;
                return Err(DbError::Configuration {
                    code: "pg_restore_unavailable",
                    message: format!("pg_restore spawn failed: {e}"),
                    hint: Some(
                        "ensure `pg_restore` is on PATH in the deployment environment \
                         (matches the server major version)"
                            .to_string(),
                    ),
                });
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let _ = guard.release().await;
            // The schema is empty at this point — the operator needs
            // to know that the partial restore left no data.
            let base = classify_pg_tool_failure(
                "pg_restore",
                &stderr,
                BusyPolicy::Abort,
            );
            return Err(match base {
                DbError::Internal { message } => DbError::Coded {
                    code: "restore_failed".to_string(),
                    message: format!(
                        "{message}\n\
                         NOTE: schema {app_id:?} is EMPTY after partial restore — \
                         operator must re-restore or re-run register_model to \
                         reconstruct state"
                    ),
                    hint: Some(
                        "P6a hardening lands the swap_schema_atomic SECURITY DEFINER \
                         function that makes the restore non-destructive on failure"
                            .to_string(),
                    ),
                },
                other => other,
            });
        }

        let _ = guard.release().await;
        Ok(())
    }

    pub(super) async fn pitr_replay_impl(
        backend: &PostgresBackend,
        app_id: &str,
        target: PitrTarget,
    ) -> Result<(), DbError> {
        // PR 4 ships the API surface only. PITR replay requires
        // PG-server-level configuration (recovery.conf,
        // archive_command); the platform can't initiate WAL replay
        // from a client connection. We record the target so the
        // operator dashboard / maintenance cron can surface
        // "PITR recovery pending for <app_id>"; the operator runs
        // the actual recovery out-of-band.
        let target_str = match &target {
            PitrTarget::Lsn(s) => format!("LSN:{s}"),
            PitrTarget::TimeMillis(ms) => format!("TIME_MS:{ms}"),
        };
        let sql = "INSERT INTO __zeroship_admin.pitr_targets \
                   (app_id, target, recorded_at) \
                   VALUES ($1, $2, NOW()) \
                   ON CONFLICT (app_id) DO UPDATE \
                     SET target = EXCLUDED.target, \
                         recorded_at = EXCLUDED.recorded_at";
        backend
            .pool()
            .query_text_params(sql, &[app_id, target_str.as_str()])
            .await
            .map_err(|e| DbError::from_pg(&e))?;
        Ok(())
    }
}

/// DB-9: validate FTS source column names with the shared identifier fence
/// before they are spliced unquoted into the `tsvector_update_trigger` DDL.
fn validate_fts_columns(columns: &[String]) -> Result<(), DbError> {
    for col in columns {
        crate::query::validate_field_name(col).map_err(DbError::from)?;
    }
    Ok(())
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
    fn validate_fts_columns_rejects_unsafe_names_db9() {
        // Normal FTS source columns pass.
        assert!(super::validate_fts_columns(&["title".into(), "body".into()]).is_ok());
        // A column name that would break out of the unquoted trigger arg list
        // (or carry a null byte) is rejected before any DDL is built.
        assert!(super::validate_fts_columns(&["body); DROP TABLE x; --".into()]).is_err());
        assert!(super::validate_fts_columns(&["a\u{0}b".into()]).is_err());
    }

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
