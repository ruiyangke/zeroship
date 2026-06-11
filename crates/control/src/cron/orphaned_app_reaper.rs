//! Orphaned-app reaper (ISS-12b): purges apps that the ISS-12 account-erase
//! reaper left behind.
//!
//! When the auth service hard-deletes an erased user (`auth::cron::
//! account_reaper`), the `zeroship.app_members` owner link cascade-deletes — but
//! `zeroship.apps` has NO FK to `zeroship.users`, so the user's apps are left
//! OWNER-LESS. Auth has no call path to control and no access to the blob store,
//! so the orphaned apps row (plus its bundle/blobs in object storage and its
//! per-app Hydra client) is never torn down. This control-side cron is the
//! cleanup: it owns apps + the blob VFS + the per-app Hydra client.
//!
//! Each tick finds apps that are **owner-less AND NOT system AND past a short
//! grace** and purges each via the shared [`crate::api::purge_app`] — the SAME
//! teardown path the `DELETE /apps/{id}` handler uses (VFS delete → atomic DB
//! cascade → best-effort Hydra client delete).
//!
//! ## The platform-console-safety guarantee
//!
//! `bootstrap_console` seeds the platform console into `zeroship.apps` with NO
//! `app_members` owner row — the console is **owner-less by construction**. A
//! naive "delete owner-less apps" sweep would DELETE THE PLATFORM'S OWN CONSOLE.
//! The console seed sets `system = true`; this reaper's detection query excludes
//! `system = true`, so a platform-owned app is never a reap candidate.
//!
//! The grace window (`created_at < NOW() - 5 minutes`) is defensive insurance:
//! `create_app` seeds the owner membership in the SAME transaction as the apps
//! row, so an owner-less app is never a transient state — but the grace costs
//! nothing and guards against any future create path that isn't atomic.

use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use crate::api::{self, PurgeError};
use crate::registry::RegistryError;
use crate::AppState;

/// Default sweep cadence in seconds (1 h) — mirrors `audit_retention` and the
/// auth account reaper. Owner-less apps are rare (only after an account erase),
/// so an hourly tick keeps the cron cheap while bounding orphan lifetime.
pub const DEFAULT_CHECK_SECS: u64 = 3600;

/// Grace window: an owner-less app younger than this is not yet reaped. Owner
/// membership is seeded atomically with the app row, so this is purely
/// defensive insurance against a non-atomic future create path.
const GRACE_INTERVAL: &str = "5 minutes";

/// Outcome of one reaper [`tick`]: how many orphaned apps were found and how
/// many were successfully purged. `found > purged` means one or more per-app
/// purges failed and were skipped (logged, isolated — a single failure never
/// stalls the rest).
#[derive(Debug, Default, Clone, Copy)]
pub struct ReaperReport {
    /// Owner-less, non-system, past-grace apps detected this tick.
    pub found: usize,
    /// Of those, successfully purged (DB row + blobs + Hydra client).
    pub purged: usize,
}

/// Cron entry point. Loops forever; each iteration runs one [`tick`] then sleeps
/// `check_secs`.
///
/// Errors inside a single tick are logged and swallowed so a transient PG hiccup
/// doesn't kill the cron task (mirrors `audit_retention` / the auth reaper).
//
// `compio_postgres::Client` / `AppState` hold `!Send` handles; the lint is
// structural, not actionable.
#[allow(clippy::future_not_send)]
pub async fn run(state: Arc<AppState>, check_secs: u64) {
    tracing::info!(check_secs, "control orphaned_app_reaper cron starting");
    loop {
        match tick(&state).await {
            Ok(report) if report.found > 0 => {
                tracing::info!(
                    found = report.found,
                    purged = report.purged,
                    "control orphaned_app_reaper sweep completed"
                );
            }
            Ok(_) => { /* nothing to do; stay quiet */ }
            Err(e) => {
                tracing::error!(error = %e, "control orphaned_app_reaper tick failed");
            }
        }
        compio::time::sleep(Duration::from_secs(check_secs)).await;
    }
}

/// Run one orphaned-app sweep. Exposed so a live-PG integration test can drive a
/// single tick deterministically without sitting on the cron sleep.
///
/// Detection: an app is a reap candidate iff it is NOT `system`, is past the
/// grace window, and has NO `owner` member. Each candidate is purged via the
/// shared [`api::purge_app`]; per-app failures are isolated (logged, counted as
/// found-not-purged) so one bad app never stalls the rest.
#[allow(clippy::future_not_send)]
pub async fn tick(state: &AppState) -> Result<ReaperReport, RegistryError> {
    let ids = find_orphaned_apps(state).await?;
    let mut report = ReaperReport {
        found: ids.len(),
        purged: 0,
    };
    for id in ids {
        match api::purge_app(state, &id).await {
            Ok(true) => report.purged += 1,
            Ok(false) => {
                // Already gone (raced with an explicit delete) — nothing to do.
                tracing::debug!(app_id = %id, "orphaned_app_reaper: app already gone");
            }
            Err(PurgeError::Vfs(e)) => {
                tracing::error!(
                    app_id = %id,
                    error = %e,
                    "orphaned_app_reaper: VFS delete failed; skipping (retried next tick)"
                );
            }
            Err(PurgeError::Registry(e)) => {
                tracing::error!(
                    app_id = %id,
                    error = %e,
                    "orphaned_app_reaper: DB delete failed; skipping (retried next tick)"
                );
            }
        }
    }
    Ok(report)
}

/// Detection query: owner-less AND NOT system AND past the grace window.
async fn find_orphaned_apps(state: &AppState) -> Result<Vec<Uuid>, RegistryError> {
    let conn = state.registry.conn().await?;
    let rows = conn
        .query(
            "SELECT a.id FROM zeroship.apps a \
             WHERE a.system = false \
               AND a.created_at < NOW() - ($1::text)::interval \
               AND NOT EXISTS ( \
                   SELECT 1 FROM zeroship.app_members m \
                   WHERE m.app_id = a.id AND m.role = 'owner' \
               )",
            &[&GRACE_INTERVAL],
        )
        .await
        .map_err(|e| RegistryError::Database(format!("orphaned-app detection: {e}")))?;
    Ok(rows.iter().map(|r| r.get::<_, Uuid>(0)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_cadence_matches_audit_retention() {
        // Hourly tick, same as audit_retention and the auth account reaper.
        assert_eq!(DEFAULT_CHECK_SECS, 3600);
    }
}
