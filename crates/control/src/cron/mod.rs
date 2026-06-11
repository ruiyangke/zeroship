//! In-process cron tasks for the control plane.
//!
//! Mirrors `auth::cron`: each task is a `loop { tick; sleep }` future detached
//! on the compio runtime via [`spawn_all`], living for the lifetime of the
//! process.
//!
//! Tasks:
//!   - [`audit_retention`] — the sanctioned deleter for the append-only
//!     `zeroship.app_audit` / `zeroship.authz_decisions` tables (P12). Peer of
//!     `auth::cron::audit_retention` (which sweeps `zeroship.audit_events`);
//!     shares the same `zeroship.audit_retention` GUC.
//!   - [`orphaned_app_reaper`] — purges apps left owner-less by the ISS-12
//!     account-erase reaper (auth), tearing down their DB rows + blobs + Hydra
//!     client. Excludes `system = true` apps (the platform console). See ISS-12b.

pub mod audit_retention;
pub mod orphaned_app_reaper;

use std::sync::Arc;

use crate::AppState;

/// Spawn every control-plane cron task onto the compio runtime.
///
/// Detached: tasks live for the lifetime of the process. The caller keeps the
/// `Arc<AppState>` alive for the lifetime of the server, so each cron's per-tick
/// connections / blob-store handles stay live.
pub fn spawn_all(state: Arc<AppState>, retention_months: u32, retention_check_secs: u64) {
    // Audit-retention sweep — needs only the registry (cheap clone of the
    // db-url handle inside `AppState`).
    let registry = Arc::new(state.registry.clone());
    compio::runtime::spawn(async move {
        audit_retention::run(registry, retention_months, retention_check_secs).await;
    })
    .detach();

    // Orphaned-app reaper — needs the full `AppState` (registry + blob VFS +
    // per-app Hydra client) to run the shared `api::purge_app` teardown.
    let reaper_state = Arc::clone(&state);
    compio::runtime::spawn(async move {
        orphaned_app_reaper::run(reaper_state, orphaned_app_reaper::DEFAULT_CHECK_SECS).await;
    })
    .detach();
}
