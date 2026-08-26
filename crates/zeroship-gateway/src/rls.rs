//! Row-Level-Security tenant-context helpers for the gateway's RLS-table
//! store ops (changeset `0025_roles_rls.sql`).
//!
//! The four tenant tables — `app_secrets`, `gateway_sessions`,
//! `app_session_anchors`, `app_user_identities` — are `FORCE ROW LEVEL
//! SECURITY` with a `tenant_isolation` policy keyed on a per-request GUC:
//!
//!   - `zeroship.tenant_app`    — the app UUID (text form), for `app_id`-keyed
//!     tables (`gateway_sessions`, `app_session_anchors`; also `app_secrets`,
//!     which the gateway never touches).
//!   - `zeroship.tenant_client` — the per-app OAuth `oac_…` `client_id`, for the
//!     `app_client_id`-keyed `app_user_identities`.
//!
//! The gateway connects as the non-bypass `zeroship_gateway` role, so every
//! RLS-table statement it issues is filtered by the policy. Each store op must
//! therefore set the matching GUC **on the same connection, inside a
//! transaction**, before its query. `set_config(name, value, true)` (the
//! `true` = `is_local`) scopes the setting to the surrounding transaction and
//! Postgres auto-reverts it at COMMIT / ROLLBACK — so a pooled connection can
//! never leak the tenant key to the next checkout. (`SET LOCAL <guc> = <v>`
//! cannot take a bind parameter; `set_config(..., $1, true)` is the
//! parameterized equivalent.)
//!
//! Usage pattern in a store fn (operates on a `&mut Client`; a `PooledClient`
//! derefs mutably to it):
//!
//! ```ignore
//! let tx = conn.transaction().await?;          // begin
//! rls::set_tenant_app(&tx, app_id).await?;     // SET LOCAL zeroship.tenant_app
//! let rows = tx.query(SQL, params).await?;     // runs under the tenant GUC
//! tx.commit().await?;                          // GUC auto-reverts here
//! ```
//!
//! Keeping the begin/commit inline in each store fn (rather than behind a
//! closure-taking combinator) sidesteps the higher-ranked-lifetime gymnastics
//! a `for<'t> FnOnce(&'t Transaction)` borrow would require, and matches the
//! plugin-db `SET LOCAL ROLE`-in-transaction precedent.

use compio_postgres::Transaction;
use uuid::Uuid;

use crate::error::{GatewayError, Result};

/// GUC name for the app-UUID tenant key (`app_id`-keyed tables).
pub const GUC_TENANT_APP: &str = "zeroship.tenant_app";
/// GUC name for the per-app OAuth `client_id` tenant key
/// (`app_client_id`-keyed `app_user_identities`).
pub const GUC_TENANT_CLIENT: &str = "zeroship.tenant_client";

/// Bind `zeroship.tenant_app` to `app_id` (text form) for the lifetime of `tx`.
///
/// Transaction-local (`set_config(..., true)`) — auto-reverts on COMMIT /
/// ROLLBACK. The RLS policy casts the GUC back to UUID
/// (`current_setting('zeroship.tenant_app', true)::uuid`); binding the text
/// form keeps the parameter type unambiguous.
///
/// # Errors
/// [`GatewayError::Db`] if the `set_config` statement fails.
pub async fn set_tenant_app(tx: &Transaction<'_>, app_id: Uuid) -> Result<()> {
    tx.execute(
        "SELECT set_config($1, $2, true)",
        &[&GUC_TENANT_APP, &app_id.to_string()],
    )
    .await
    .map_err(|e| GatewayError::Db(format!("rls set tenant_app: {e}")))?;
    Ok(())
}

/// Bind `zeroship.tenant_client` to `app_client_id` (the per-app `oac_…` OAuth
/// `client_id`) for the lifetime of `tx`.
///
/// Transaction-local — see [`set_tenant_app`]. The `app_user_identities` policy
/// compares the GUC as text
/// (`app_client_id = current_setting('zeroship.tenant_client', true)`).
///
/// # Errors
/// [`GatewayError::Db`] if the `set_config` statement fails.
pub async fn set_tenant_client(tx: &Transaction<'_>, app_client_id: &str) -> Result<()> {
    tx.execute(
        "SELECT set_config($1, $2, true)",
        &[&GUC_TENANT_CLIENT, &app_client_id],
    )
    .await
    .map_err(|e| GatewayError::Db(format!("rls set tenant_client: {e}")))?;
    Ok(())
}
