//! In-process cron tasks. Spawned at boot via [`spawn_all`].
//!
//! Each task is a `loop { tick; sleep }` future detached on the compio
//! runtime. Tasks observe their schedule from
//! [`AuthConfig`](crate::config::AuthConfig) (so an operator can shorten
//! `--cron-tick-secs` for staging environments) and coordinate via
//! `zeroship.cron_state` rows when they need durable "last-ran" tracking.
//!
//! P6-U1 ships `jwk_rotation`; P6-U2 adds `audit_retention`;
//! `token_sweep` drops expired one-shot token rows after their grace window.

pub mod account_reaper;
pub mod audit_retention;
pub mod jwk_rotation;
pub mod token_sweep;

use std::sync::Arc;

use crate::config::AuthConfig;
use crate::hydra_client::HydraAdmin;

/// Spawn every cron task onto the compio runtime.
///
/// Detached: tasks live for the lifetime of the process. Callers are
/// responsible for keeping the `Arc<Client>` they pass in alive (in
/// practice, `server::run` holds the matching `Arc` until shutdown).
pub fn spawn_all(
    admin: HydraAdmin,
    db: Arc<compio_postgres::Client>,
    cfg: Arc<AuthConfig>,
) {
    let db_jwk = db.clone();
    let cfg_jwk = cfg.clone();
    compio::runtime::spawn(async move {
        jwk_rotation::run(admin, db_jwk, cfg_jwk).await;
    })
    .detach();

    // `audit_retention` opens its OWN dedicated connection per tick (the
    // sweep flips the append-only tamper trigger off via a transaction-local
    // GUC, which must never share a socket with other traffic), so it takes
    // only the config — not the shared `Arc<Client>`.
    let cfg_audit = cfg;
    compio::runtime::spawn(async move {
        audit_retention::run(cfg_audit).await;
    })
    .detach();

    let db_token_sweep = db.clone();
    compio::runtime::spawn(async move {
        token_sweep::run(db_token_sweep).await;
    })
    .detach();

    // ISS-12: erase accounts whose deletion grace window has elapsed.
    let db_reaper = db;
    compio::runtime::spawn(async move {
        account_reaper::run(db_reaper).await;
    })
    .detach();
}
