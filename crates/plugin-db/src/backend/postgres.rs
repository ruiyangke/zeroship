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

use std::rc::Rc;

use crate::diff::LiveSchema;
use crate::error::DbError;

use super::{
    Backend, IndexBuilder, LockManager, NamespaceManager, PgLockManager, PgSqlExecutor,
    SchemaIntrospect, SqlExecutor,
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
}

impl std::fmt::Debug for PostgresBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresBackend").finish()
    }
}

impl PostgresBackend {
    /// Build a backend handle around an already-initialised pool.
    pub fn new(pool: Rc<compio_postgres::Pool>, url: String) -> Self {
        Self { pool, url }
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
        let create_schema = crate::query::build_create_schema(app_id);
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
        Backend, IndexBuilder, LockManager, NamespaceManager, PgLockManager, PgSqlExecutor,
        RegisterBackend, SchemaIntrospect, SqlExecutor,
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
