//! In-process cron tasks. Spawned at boot via [`spawn_all`].
//!
//! Each task is a `loop { tick; sleep }` future detached on the compio
//! runtime. Tasks observe their schedule from
//! [`AuthConfig`](crate::config::AuthConfig) (so an operator can shorten
//! `--cron-tick-secs` for staging environments) and coordinate via
//! `auth.cron_state` rows when they need durable "last-ran" tracking.
//!
//! P6-U1 ships `jwk_rotation`. P6-U2 will add `audit_retention` here.

pub mod jwk_rotation;

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
    compio::runtime::spawn(async move {
        jwk_rotation::run(admin, db, cfg).await;
    })
    .detach();

    // P6-U2 adds audit_retention here.
}
