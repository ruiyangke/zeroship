//! Stage 1 — Bootstrap.
//!
//! Idempotent setup that has to happen before any diff/apply work:
//!
//! 1. Acquire the session-scoped advisory lock on `(app_id,
//!    'register_model')` via a dedicated pool client. The lock survives
//!    the non-transactional `CREATE INDEX CONCURRENTLY` phase because
//!    [`apply`](super::apply) drops the client (releasing the lock)
//!    between pass 1 and pass 2.
//! 2. `CREATE SCHEMA IF NOT EXISTS` for the app.
//! 3. `CREATE TABLE IF NOT EXISTS __zeroship_migrations` (delegated to
//!    [`crate::backend::Backend::ensure_audit_table`]).
//! 4. Compute `schema_version` from `MAX(schema_version) + 1` over the
//!    audited DDL history.
//! 5. Expand declared inline indexes + named indexes into a single
//!    `Vec<IndexSpec>` the plan stage will diff against the live
//!    snapshot.
//!
//! Returns a [`RegisterContext`] the later stages thread through, plus
//! a separate [`LockGuard`] carrying the advisory lock. The two are
//! split so `RegisterContext` can be passed by value without dragging
//! the pool's borrow lifetime through the type.

use serde_json::Value;

use crate::backend::{LockGuard, LockScope, RegisterBackend};
use crate::error::DbError;
use crate::query;

/// Threaded context produced by `bootstrap`. Pure value type — does
/// not own any borrow-lifetimed handle. The advisory-lock client is
/// returned separately by [`bootstrap`].
pub(crate) struct RegisterContext {
    /// App identifier — schema name, audit-row key.
    pub app_id: String,
    /// Deploy identifier — audit-row grouping key. `'cold_start'` for
    /// pre-deploy bootstrap.
    pub deploy_id: String,
    /// `schema_version` snapshot captured AFTER `ensure_audit_table`
    /// runs, so each DDL row gets a monotonic version. Set once at
    /// bootstrap and shared across every audit write in this run.
    pub schema_version: i32,
    /// Strictness from `schema._meta.strictness` (defaults to `strict`).
    /// Read once at bootstrap so the validate stage can branch without
    /// re-walking the JSON.
    pub strictness: String,
    /// Expanded index list — declared inline + named — passed into
    /// `compute_diff` so the plan correctly identifies which need
    /// `CREATE INDEX CONCURRENTLY`.
    pub declared_indexes: Vec<query::IndexSpec>,
}

/// Stage tag for the per-app register-model advisory lock. Used as
/// the `name` field of the canonical
/// [`LockScope::GlobalApp`](crate::backend::LockScope::GlobalApp)
/// — `LockScope::to_keys` then derives the underlying
/// `(format!("{app_id}:register_model"), "register_model")` pair the
/// PG impl hashes through `hashtext()` (§7.2 / §10.5).
pub(crate) const LOCK_TAG: &str = "register_model";

/// Run stage 1.
///
/// Order is load-bearing: the schema must exist before
/// `ensure_audit_table` runs, the audit table must exist before
/// `next_schema_version` reads from it, the lock must be held before
/// any diff/apply work to serialise concurrent deploys per-app.
///
/// The advisory-lock client is acquired via the pool's `get()` (a
/// `PooledClient`) rather than a fresh `connect()`. The pool already
/// runs the connection task on whatever compio runtime the warm-up
/// happened on; reusing that task avoids the test-harness pattern
/// where each `dispatch_zs` spins a fresh runtime and would orphan a
/// freshly-spawned connection.
///
/// **P0 PR 3**: bound narrowed from `&PostgresBackend` to
/// [`RegisterBackend`] — the composition marker over the six
/// capability traits this stage actually uses. The
/// [`crate::backend::PgLockManager::acquire_pooled_client_for_lock`]
/// method closes the `backend.pool().get()` escape hatch while
/// preserving the borrow-lifetime `'p` that threads through
/// [`LockGuard`] (Open Q5 resolution; see
/// `docs/proposals/p0-implementation-plan.md` §"PR 3" + §3 Q5 and
/// `docs/proposals/db-system-design.md` §7).
///
/// **P0 PR 6**: the lock site is classified as
/// `LockScope::GlobalApp { app_id, name: "register_model" }` — the
/// canonical §10.5 shape. `LockScope::to_keys` then yields
/// `(format!("{app_id}:register_model"), "register_model")`, which
/// the PG impl's `hashtext()` SQL consumes verbatim.
pub(crate) async fn bootstrap<'p, B: RegisterBackend>(
    backend: &'p B,
    app_id: &str,
    collection: &str,
    schema: &Value,
    indexes: &Value,
    deploy_id: &str,
) -> Result<(RegisterContext, LockGuard<'p>), DbError> {
    // Strictness — proposal A2 line 122. Read from schema._meta.strictness
    // if present; default is 'strict'.
    let strictness = schema
        .get("_meta")
        .and_then(|m| m.get("strictness"))
        .and_then(Value::as_str)
        .unwrap_or("strict")
        .to_string();

    // -------------------------------------------------------------------
    // Concurrent-deploy serialisation: proposal A2 line 202.
    //
    // Two-key advisory lock keyed on (app_id, register_model). Held at
    // session scope on a dedicated pool client so the lock survives
    // the CREATE INDEX CONCURRENTLY phases (which can't run in a
    // transaction). Released when `apply` calls [`LockGuard::release`]
    // at the end of pass 1.
    //
    // P0 PR 3: closes the `backend.pool().get()` escape hatch — the
    // PG-only `PgLockManager::acquire_pooled_client_for_lock` returns
    // the same `PooledClient<'p>` shape so `'p` still threads through
    // `LockGuard<'p>` exactly as before. The error mapping
    // (`DbError::Transient` with the same operator-facing prefix)
    // lives in the PG impl now. Open Q5 resolution.
    //
    // P0 PR 6: typed [`LockScope::GlobalApp`] classifies this site as
    // cluster-wide (visible to every worker pointed at the same DB);
    // `LockScope::to_keys` derives the `(key1, key2)` pair the
    // underlying `LockManager` primitive consumes.
    let lock_client = backend.acquire_pooled_client_for_lock().await?;
    let scope = LockScope::GlobalApp {
        app_id: app_id.to_string(),
        name: LOCK_TAG.to_string(),
    };
    let guard = LockGuard::acquire(backend, lock_client, scope)
        .await
        .map_err(|e| match e {
            // Preserve the operator-facing prefix when the lock attempt
            // produced a pg error; other DbError variants flow through
            // verbatim so `lock_not_available` / `transient` reach JS
            // with their canonical `.code`.
            DbError::Internal { message } => DbError::Internal {
                message: format!("db: pg_advisory_lock failed: {message}"),
            },
            other => other,
        })?;
    // From here on, any other orchestrator call against the same app_id
    // blocks until the guard is released or the held client is dropped.

    // -------------------------------------------------------------------
    // Schema + audit table.
    //
    // These calls go through the pool (not the locked client), so a
    // failure does not affect the lock state — but it MUST still
    // release the lock before propagating, otherwise the guard's
    // Drop logs an error and the held client returns to the pool
    // with the session-scoped lock alive. `release_on_err` centralises
    // that pattern.
    // -------------------------------------------------------------------
    let ctx = match build_ctx(backend, app_id, collection, schema, indexes, deploy_id, strictness)
        .await
    {
        Ok(ctx) => ctx,
        Err(e) => {
            let _ = guard.release().await;
            return Err(e);
        }
    };

    Ok((ctx, guard))
}

/// Inner half of [`bootstrap`] — runs everything that can fail AFTER
/// the lock has been acquired so the outer function can release on
/// `Err` in one place.
///
/// **P0 PR 3**: bound narrowed from `&PostgresBackend` to
/// [`RegisterBackend`] in lock-step with `bootstrap`. See the
/// outer function's rustdoc for the rationale.
#[allow(clippy::too_many_arguments)]
async fn build_ctx<B: RegisterBackend>(
    backend: &B,
    app_id: &str,
    collection: &str,
    schema: &Value,
    indexes: &Value,
    deploy_id: &str,
    strictness: String,
) -> Result<RegisterContext, DbError> {
    backend
        .ensure_app_schema(app_id)
        .await
        .map_err(|e| match e {
            DbError::Internal { message } => DbError::Internal {
                message: format!("db: create schema failed: {message}"),
            },
            other => other,
        })?;

    // P0 PR 2: audit-table helpers live as free fns in `crate::audit`;
    // reach the pool through the `PgSqlExecutor::pool_handle` accessor.
    // Open Q1 resolution per `docs/proposals/p0-implementation-plan.md`
    // §3 Q1 + §"PR 2".
    let pool = backend.pool_handle().as_ref();
    crate::audit::ensure_audit_table_exists(pool, app_id).await?;

    let schema_version = crate::audit::next_schema_version(pool, app_id).await?;

    let mut declared_indexes =
        query::build_create_indexes(app_id, collection, schema).map_err(DbError::from)?;
    let named_indexes =
        query::build_named_indexes(app_id, collection, indexes).map_err(DbError::from)?;
    declared_indexes.extend(named_indexes);

    Ok(RegisterContext {
        app_id: app_id.to_string(),
        deploy_id: deploy_id.to_string(),
        schema_version,
        strictness,
        declared_indexes,
    })
}
