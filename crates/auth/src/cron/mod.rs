//! In-process cron tasks. Spawned at boot via [`spawn_all`].
//!
//! Each task is a `loop { tick; sleep }` future detached on the compio
//! runtime. Audit retention reads its schedule from
//! [`AuthConfig`](crate::config::AuthConfig); the other tasks use fixed
//! schedules appropriate to their retention windows.
//!
//! The active tasks retain audit logs, drop expired one-shot token rows after
//! their grace window, terminally retire elapsed OIDC signing keys, and erase
//! accounts whose deletion grace window elapsed.

pub mod account_reaper;
pub mod audit_retention;
pub mod signing_key_retention;
pub mod token_sweep;

use std::sync::Arc;

use crate::config::AuthConfig;

/// Spawn every cron task onto the compio runtime.
///
/// Detached: tasks live for the lifetime of the process. Callers are
/// responsible for keeping the `Arc<Client>` they pass in alive (in
/// practice, `server::run` holds the matching `Arc` until shutdown).
pub fn spawn_all(
    db: Arc<compio_postgres::Client>,
    cfg: Arc<AuthConfig>,
    refresh_pool: crate::oidc::refresh::RefreshSessionPool,
) {
    // `audit_retention` opens its OWN dedicated connection per tick (the
    // sweep flips the append-only tamper trigger off via a transaction-local
    // GUC, which must never share a socket with other traffic), so it takes
    // only the config — not the shared `Arc<Client>`.
    let cfg_audit = cfg.clone();
    compio::runtime::spawn(async move {
        audit_retention::run(cfg_audit).await;
    })
    .detach();

    let db_signing_key_retention = db.clone();
    compio::runtime::spawn(async move {
        signing_key_retention::run(db_signing_key_retention).await;
    })
    .detach();

    let db_token_sweep = db.clone();
    let refresh_pool_token_sweep = refresh_pool;
    compio::runtime::spawn(async move {
        token_sweep::run(db_token_sweep, refresh_pool_token_sweep).await;
    })
    .detach();

    // ISS-12: erase accounts whose deletion grace window has elapsed.
    let db_reaper = db;
    compio::runtime::spawn(async move {
        account_reaper::run(db_reaper).await;
    })
    .detach();
}
