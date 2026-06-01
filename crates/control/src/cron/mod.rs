//! In-process cron tasks for the control plane.
//!
//! Mirrors `auth::cron`: each task is a `loop { tick; sleep }` future detached
//! on the compio runtime via [`spawn_all`], living for the lifetime of the
//! process.
//!
//! Today the only task is [`audit_retention`], the sanctioned deleter for the
//! append-only `zeroship.app_audit` / `zeroship.authz_decisions` tables (P12).
//! It is the control-side peer of `auth::cron::audit_retention` (which sweeps
//! `zeroship.audit_events`) and shares the same `zeroship.audit_retention` GUC.

pub mod audit_retention;

use std::sync::Arc;

use crate::registry::Registry;

/// Spawn every control-plane cron task onto the compio runtime.
///
/// Detached: tasks live for the lifetime of the process. The caller keeps the
/// `Arc<Registry>` alive for the lifetime of the server (`AppState` holds the
/// matching handle), so the cron's per-tick connections always have a live URL.
pub fn spawn_all(registry: Arc<Registry>, retention_months: u32, check_secs: u64) {
    compio::runtime::spawn(async move {
        audit_retention::run(registry, retention_months, check_secs).await;
    })
    .detach();
}
