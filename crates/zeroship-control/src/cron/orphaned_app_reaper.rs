//! Orphaned-app reaper (ISS-12b): archives apps that the ISS-12 account-erase
//! reaper left behind.
//!
//! When the auth service hard-deletes an erased user (`auth::cron::
//! account_reaper`), their `zeroship.organization_members` rows cascade-delete
//! — but `zeroship.apps` has NO FK to `zeroship.users`, so an app whose
//! organization is left with no owner is OWNER-LESS. Auth has no call path to
//! control and no access to the blob store, so the orphaned app would otherwise
//! keep serving. This control-side cron applies the same archive marker as the
//! creator-facing lifecycle route.
//!
//! WHAT "OWNER-LESS" MEANS, AND WHY THE REAPER IS NARROW. An app reaches its
//! owner through `apps.project_id -> projects.organization_id`, so erasing ONE
//! owner of a shared organization orphans nothing: the remaining owners still
//! answer for every app in it. Only an organization that loses its LAST owner
//! produces reap candidates - a personal organization losing its owner, or a
//! shared one losing its final owner.
//! The erased owner cannot authorize a restore, so these rows and their names
//! remain retained until a future operator-owned database lifecycle handles
//! them. This reaper does not silently substitute privileged teardown.
//!
//! Each tick finds apps that are **owner-less AND NOT system AND past a short
//! grace** and archives it. Billing evidence, manifests, OAuth state, database
//! schema, and database grants remain attached to the retained app row.
//!
//! ## The platform-console-safety guarantee
//!
//! A system-owned app can sit in a project whose organization has no owner —
//! the console is **owner-less by construction**. A naive "delete owner-less
//! apps" sweep would DELETE THE PLATFORM'S OWN CONSOLE. The console seed sets
//! `system = true`; this reaper's detection query excludes `system = true`, so a
//! platform-owned app is never a reap candidate.
//!
//! The grace window (`created_at < NOW() - 5 minutes`) is defensive insurance.
//! `create_app` can no longer write an app whose project does not exist - a
//! LIVE app always names one (`apps_live_app_has_project`: `project_id IS NOT
//! NULL OR deleted_at IS NOT NULL`), against a RESTRICT foreign key - and the
//! zero-config path seats the caller as their personal organization's owner
//! before it creates anything. So an owner-less app is still never a transient
//! state, but the grace costs nothing and guards a future create path that is
//! not atomic.
//!
//! `project_id` LOST ITS NOT NULL when app deletion landed: a deleted app
//! detaches from its project, so the owner lateral finds no project and reports
//! no owner. That does not reach this sweep, and not by luck - deletion implies
//! archive (`apps_deleted_app_is_archived`), and the detection query already
//! requires `archived_at IS NULL`. A deleted app is therefore never a reap
//! candidate, and the `archived_at` predicate is the reason. Do not relax it
//! into a `deleted_at`-blind scan.

use std::sync::Arc;
use std::time::Duration;

use zeroship_core::app_id::AppId;

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
/// many were successfully archived. `found > archived` means one or more per-app
/// transitions failed and were skipped (logged, isolated - a single failure never
/// stalls the rest).
#[derive(Debug, Default, Clone, Copy)]
pub struct ReaperReport {
    /// Owner-less, non-system, past-grace apps detected this tick.
    pub found: usize,
    /// Of those, successfully archived.
    pub archived: usize,
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
                    archived = report.archived,
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
/// grace window, has NO `owner` member, and is not already archived. Per-app
/// failures are isolated so one bad app never stalls the rest.
#[allow(clippy::future_not_send)]
pub async fn tick(state: &AppState) -> Result<ReaperReport, RegistryError> {
    let ids = find_orphaned_apps(state).await?;
    let mut report = ReaperReport {
        found: ids.len(),
        archived: 0,
    };
    for id in ids {
        match state.registry.archive_app(&id).await {
            Ok(Some(_)) => report.archived += 1,
            Ok(None) => {
                // The row disappeared through operator SQL after detection.
                tracing::debug!(app_id = %id.as_str(), "orphaned_app_reaper: app already gone");
            }
            Err(e) => {
                tracing::error!(
                    app_id = %id.as_str(),
                    error = %e,
                    "orphaned_app_reaper: archive failed; skipping (retried next tick)"
                );
            }
        }
    }
    Ok(report)
}

/// Detection query: owner-less AND NOT system AND past the grace window.
///
/// Exposed for the same reason [`tick`] is, and for one more: a test can assert
/// what the sweep SELECTS without asserting how many rows it archived. The
/// count is fleet-wide - every app in the database past the grace window is in
/// it - so "an archived orphan is not selected again" written as `archived == 0`
/// is really "and nothing else in the fleet became reapable meanwhile", which
/// on a database that has been used is false often enough to fail a suite run
/// against a database a previous run left rows in. Membership of THIS list is
/// the property; the count is not.
#[allow(clippy::future_not_send)]
pub async fn find_orphaned_apps(state: &AppState) -> Result<Vec<AppId>, RegistryError> {
    let conn = state.registry.conn().await?;
    let rows = conn
        .query(
            &format!(
                "SELECT a.id FROM zeroship.apps a {lateral} \
                 WHERE a.system = false \
                   AND a.archived_at IS NULL \
                   AND a.created_at < NOW() - ($1::text)::interval \
                   AND app_owner.user_id IS NULL",
                lateral = crate::organizations::app_owner_lateral(),
            ),
            &[&GRACE_INTERVAL],
        )
        .await
        .map_err(|e| RegistryError::Database(format!("orphaned-app detection: {e}")))?;
    rows.iter()
        .map(|r| {
            let raw: String = r.get(0);
            AppId::parse(&raw).map_err(|e| {
                RegistryError::Database(format!("apps.id {raw} is not a canonical app id: {e}"))
            })
        })
        .collect()
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
