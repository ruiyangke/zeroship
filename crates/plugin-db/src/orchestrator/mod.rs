//! Cross-cutting orchestrators that coordinate `crate::audit`,
//! `crate::diff`, `crate::query`, and `crate::exec` into multi-step
//! flows the V8 dispatchers expose to JS.
//!
//! Submodules:
//!
//! - [`register_model`] — the four-phase DDL pipeline behind
//!   `db.registerModel(...)`. Owns the advisory-lock guard, the
//!   diff/classify/validate/apply sequence, and the audited
//!   create-index recovery loop.
//! - [`transaction`] — explicit `db.beginTransaction()` lifecycle:
//!   mints the `Transaction` v8_class, issues BEGIN on a dedicated
//!   connection, and stamps `IsolateDbContext::tx_token` (formerly a
//!   `TX_TOKEN` thread-local, folded into `IsolateDbContext` in Stage
//!   8d-R4) so the wrapper's commit/rollback/Drop paths can fence
//!   each other.
//! - [`auto_tx`] — defense-in-depth wrappers (`__zsBeginAutoTx` /
//!   `__zsEndAutoTx`) the runtime installs on `globalThis`. Wraps
//!   `query()` / `mutation()` handlers in a per-kind isolation
//!   envelope; commits or rolls back at the end of the handler.
//!
//! Three submodules (`auto_tx`, `register_model`, `transaction`) are
//! `pub` so the v8_class layer (`v8_classes/*.rs`) can name their
//! dispatch helpers. The session-scoped advisory-lock RAII guard
//! that used to live here as `lock_guard` moved to
//! [`crate::backend::lock_guard`] in P0 PR 6 — it's the canonical
//! return shape for the [`crate::backend::LockManager`] capability,
//! not an orchestrator-internal detail. There is no aggregating
//! re-export — `crate::callbacks` was deleted in Stage 8b once each
//! consumer moved to its canonical import.

pub mod auto_tx;
pub mod drop_namespace;
pub mod register_model;
pub mod transaction;

/// Execute a control statement (`BEGIN`, `SAVEPOINT`, `COMMIT`,
/// `ROLLBACK`, etc.) against the backend-specific pinned tx client.
pub(crate) async fn client_exec_on_tx(
    backend: &crate::backend::BackendHandle,
    client: &crate::context::TxConnection,
    sql: &str,
    params: &[&str],
) -> Result<u64, crate::error::DbError> {
    use crate::backend::SqlExecutor;
    use crate::context::TxConnection;

    match (backend, client) {
        (
            crate::backend::BackendHandle::Postgres(pg),
            TxConnection::Postgres(client),
        ) => pg.client_exec(client, sql, params).await,
        (crate::backend::BackendHandle::Sqlite(sq), TxConnection::Sqlite(client)) => {
            sq.client_exec(client, sql, params).await
        }
        (crate::backend::BackendHandle::Postgres(_), TxConnection::Sqlite(_))
        | (crate::backend::BackendHandle::Sqlite(_), TxConnection::Postgres(_)) => Err(
            crate::error::DbError::internal("db: transaction backend/client mismatch"),
        ),
    }
}

/// Apply the §17.5 per-app PG role to a transaction's dedicated client.
///
/// Issues `SET LOCAL ROLE "<per-app role>"` on `client` so every
/// statement in the surrounding transaction executes under the
/// constrained per-app role rather than the platform login role. `SET
/// LOCAL` auto-reverts at COMMIT / ROLLBACK, so a pooled / dedicated
/// connection can never leak the role to a later use.
///
/// The per-app role is provisioned by `register_model`. The WAL
/// consumer + §17.6 watchdog + §17.7 drop step 3 deliberately do NOT
/// call this — they stay on the platform role (the only connection
/// crossing the per-app trust boundary).
///
/// Shared by [`transaction::exec_begin`] and [`auto_tx::exec_auto_begin`]
/// so the role-application happens at exactly one logical site per tx
/// flavour.
pub(crate) async fn apply_per_app_role(
    client: &compio_postgres::Client,
    app_id: &str,
) -> Result<(), crate::error::DbError> {
    let sql = crate::auth::bootstrap::set_local_role_sql(app_id);
    client
        .execute(&sql, &[])
        .await
        .map_err(|e| {
            let mut err = crate::error::DbError::from_pg(&e);
            crate::error::prefix_message(&mut err, "db: SET LOCAL ROLE (per-app §17.5): ");
            err
        })?;
    Ok(())
}
