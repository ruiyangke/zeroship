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
//! a separate `PooledClient` carrying the advisory lock. The two are
//! split so `RegisterContext` can be passed by value without dragging
//! the pool's borrow lifetime through the type.

use compio_postgres::PooledClient;
use serde_json::Value;

use crate::backend::{Backend, PostgresBackend};
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

/// Advisory-lock key namespacing — matches the Postgres
/// `pg_advisory_lock(hashtext('zs_reg:' || $1)::int4,
/// hashtext('register_model')::int4)` pair the pre-Stage-8e code
/// emitted inline. `pub(crate)` so `apply()` uses the same key when
/// releasing.
pub(crate) fn lock_key(app_id: &str) -> String {
    format!("zs_reg:{app_id}")
}

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
pub(crate) async fn bootstrap<'p>(
    backend: &'p PostgresBackend,
    app_id: &str,
    collection: &str,
    schema: &Value,
    indexes: &Value,
    deploy_id: &str,
) -> Result<(RegisterContext, PooledClient<'p>), DbError> {
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
    // transaction). Released when `apply` drops the client at the end
    // of pass 1.
    let lock_client = backend.pool().get().await.map_err(|e| DbError::Transient {
        message: format!("db: failed to acquire orchestrator client: {e}"),
    })?;
    let key = lock_key(app_id);
    backend
        .acquire_advisory_lock(&lock_client, &key, LOCK_TAG)
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
    // blocks until lock_client is dropped.

    // -------------------------------------------------------------------
    // Schema + audit table.
    //
    // Run the post-acquire body inside an async block so we can capture
    // its Result and ALWAYS release the advisory lock on error before
    // dropping `lock_client`. The operations below run against the pool
    // (NOT `lock_client`), so an early `?` propagation would drop the
    // pooled lock_client straight back into the pool with the
    // session-scoped advisory lock STILL HELD — every subsequent caller
    // that pulled the same connection would block on
    // `pg_advisory_lock(...)` (cross-app stall).
    //
    // Mirror apply.rs Pass-1's pattern: capture into `inner`, then on
    // error path issue `pg_advisory_unlock` via the lock_client and
    // drop it BEFORE propagating; success path returns `lock_client`
    // untouched so the lock stays held across to `apply()`.
    // -------------------------------------------------------------------
    let inner: Result<RegisterContext, DbError> = async {
        backend
            .ensure_app_schema(app_id)
            .await
            .map_err(|e| match e {
                DbError::Internal { message } => DbError::Internal {
                    message: format!("db: create schema failed: {message}"),
                },
                other => other,
            })?;

        backend.ensure_audit_table(app_id).await?;

        let schema_version = backend.next_schema_version(app_id).await?;

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
    .await;

    match inner {
        Ok(ctx) => Ok((ctx, lock_client)),
        Err(e) => {
            // Release the advisory lock BEFORE returning the pooled
            // connection. Best-effort: the unlock SQL may itself error
            // if the connection was already torn down, but the more
            // important guarantee is that we don't leak a held lock
            // back into the pool.
            let unlock_sql =
                "SELECT pg_advisory_unlock(hashtext($1)::int4, hashtext($2)::int4)";
            let _ = lock_client
                .query_text_params(unlock_sql, &[key.as_str(), LOCK_TAG])
                .await;
            drop(lock_client);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit-test coverage for `bootstrap()`'s error-path unlock is
    //! intentionally deferred to the integration suite.
    //!
    //! Why: `bootstrap()` is `pub(crate)` and is parameterised over a
    //! concrete `&PostgresBackend` (NOT the `Backend` trait), so an
    //! in-crate unit test would still need a live `Pool` and `PgClient`
    //! to drive `acquire_advisory_lock` and the `ensure_*` calls — at
    //! that point the test IS an integration test (Postgres on port
    //! 5434, gated by `require_pg()`).
    //!
    //! Coverage lives in `crates/plugin-db/tests/integration.rs`:
    //!
    //!  - The cross-app stall symptom is observed end-to-end by the
    //!    parallel orchestrator harness (search the file for
    //!    `parallel_register_model` / `pg_advisory_lock` waiters).
    //!  - The two prior advisory-lock leak fixes (commits b4e533e2,
    //!    37a0ef76) shipped without `#[cfg(test)]` unit tests for the
    //!    same reason — the lock state is observable only against a
    //!    real Postgres session.
    //!
    //! If a future refactor lifts `bootstrap()` onto the `Backend`
    //! trait (so it can take `&impl Backend`), this module is the
    //! place to add a fake-backend test that injects an `ensure_app_schema`
    //! failure and asserts `pg_advisory_unlock` was issued on the same
    //! `lock_client` before `Drop`.

    #[test]
    fn lock_key_formats_with_zs_reg_prefix() {
        // Cheap, runtime-free invariant check — `lock_key` is the
        // value `apply()`'s unlock and the error-path unlock share.
        // If this format ever drifts from
        // `pg_advisory_lock(hashtext('zs_reg:<app>'), hashtext('register_model'))`,
        // the unlock SQL above stops releasing what `acquire_advisory_lock`
        // took.
        assert_eq!(super::lock_key("app_abc"), "zs_reg:app_abc");
        assert_eq!(super::LOCK_TAG, "register_model");
    }
}
