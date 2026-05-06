//! Background sweep tasks for the snapshot/restore lifecycle.
//!
//! Source-of-truth: docs/proposals/sandbox-snapshot-restore.md
//! § 6.1 (transient-state lease takeover) and § 7 (idle eviction).
//!
//! Two long-running tasks, each takes `Arc<AppState>` and runs an
//! event loop until `state.shutdown_requested()`:
//!
//! 1. [`spawn_idle_eviction_sweep`] — every
//!    `SANDBOX_IDLE_SNAPSHOT_SWEEP_SECS` (default 300) reads
//!    `db.idle_eligible_sandboxes` and drives each row through
//!    `snapshot_handler::snapshot_sandbox`. Throttled to
//!    `SANDBOX_SNAPSHOT_PER_WORKER_CONCURRENCY` (default 2)
//!    concurrent snapshots per sweep iteration — v1 keeps it simple
//!    by running serially in batches of N, no semaphore.
//!
//! 2. [`spawn_transient_state_takeover`] — every 30s reads
//!    `db.transient_state_lease_expired_sandboxes` and CASes each
//!    row to its recovery state per § 9.2:
//!      `snapshotting → snapshotting_aborted`
//!      `restoring   → snapshotted`
//!      `restoring_cold → snapshotted_suspect`
//!
//! Both tasks are spawned from `AppState::from_config` after the
//! existing heartbeat / takeover spawns. The feature flag
//! (`state.config.snapshot_enabled`) gates the *idle-eviction* sweep
//! (no transient writers when off, so nothing to eviction-sweep).
//! The transient-state sweep keeps running regardless of the flag —
//! defense-in-depth so a feature-flipped-on-then-off deploy still
//! recovers any in-flight transients left behind.

use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use crate::db::{Database, SandboxRow, SandboxStatus};
use crate::AppState;

/// `SANDBOX_IDLE_SNAPSHOT_SWEEP_SECS`. How often the idle-eviction
/// loop wakes up. Default 300 (5 min). Tunable but a 5-min minimum
/// is a reasonable floor: we don't want this loop hammering pg
/// every few seconds for an O(opted-in cohort) query.
pub const DEFAULT_IDLE_SWEEP_SECS: u64 = 300;

/// `SANDBOX_IDLE_SNAPSHOT_SECS`. Idle threshold — rows with
/// `last_used_at < now() - this` are eviction-eligible. Default 1800
/// (30 min). `0` disables the sweep entirely (defense-in-depth dev
/// mode).
pub const DEFAULT_IDLE_THRESHOLD_SECS: i64 = 1800;

/// `SANDBOX_SNAPSHOT_PER_WORKER_CONCURRENCY`. Per-iteration cap on
/// concurrent snapshot operations. v1 runs them in serial batches
/// of this size — compio doesn't ship a `Semaphore` we need, and
/// the actual snapshot wall (~2.1s + L2 upload) means batches of 2
/// at 5-min cadence saturate the design budget without contention.
pub const DEFAULT_PER_WORKER_CONCURRENCY: usize = 2;

/// `SANDBOX_TRANSIENT_STATE_TIMEOUT_SECS`. § 6.1 default 120 s.
pub const DEFAULT_TRANSIENT_TIMEOUT_SECS: i64 = 120;

/// Cadence for the transient-state takeover sweep. Hard-coded 30s
/// per § 6.1 (operators can edit this constant if real workloads
/// need a tighter loop; not env-driven because the threshold is
/// what matters for correctness, not the poll cadence).
pub const TRANSIENT_TAKEOVER_POLL_SECS: u64 = 30;

/// Idle-eviction sweep candidates per pg call. Bounds the per-sweep
/// pg work; if more rows are eligible they'll be picked up next
/// iteration.
pub const IDLE_BATCH_LIMIT: i64 = 100;

fn read_u64_env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

fn read_i64_env(name: &str, default: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(default)
}

fn read_usize_env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(default)
}

// ────────────────────────────────────────────────────────────────────
// Transient-state takeover sweep
// ────────────────────────────────────────────────────────────────────

/// Pure helper: target recovery state per § 9.2 for an abandoned
/// transient. Exposed so the unit + pg-gated tests can pin the
/// table without duplicating the match.
pub fn recovery_target(status: SandboxStatus) -> Option<SandboxStatus> {
    match status {
        SandboxStatus::Snapshotting => Some(SandboxStatus::SnapshottingAborted),
        SandboxStatus::Restoring => Some(SandboxStatus::Snapshotted),
        SandboxStatus::RestoringCold => Some(SandboxStatus::SnapshottedSuspect),
        _ => None,
    }
}

/// Run a single iteration of the transient-state takeover sweep.
/// Public so the pg-gated tests can drive a single pass without
/// spawning the loop. Callers should hold `Arc<AppState>` so the
/// `Database` and shutdown flag are reachable.
///
/// Returns `(seen, recovered)` — `seen` is the rows the query
/// produced; `recovered` is the rows whose CAS succeeded. The
/// difference is normal: a peer may have already taken the row over
/// (CAS-loss), or the row may have moved out of the transient state
/// between the query and the CAS.
pub async fn run_transient_takeover_once(
    state: &Arc<AppState>,
    threshold_secs: i64,
) -> (usize, usize) {
    let Some(db) = state.database.as_ref() else {
        return (0, 0);
    };
    let rows = match db
        .transient_state_lease_expired_sandboxes(threshold_secs)
        .await
    {
        Ok(rs) => rs,
        Err(e) => {
            tracing::warn!(error = %e, "sandbox transient-takeover: query failed");
            return (0, 0);
        }
    };
    let seen = rows.len();
    let mut recovered = 0usize;
    for row in rows {
        if state.shutdown_requested() {
            break;
        }
        let Some(target) = recovery_target(row.status) else {
            // Defensive: query already filtered to the three
            // transient states, but a race could surface a row in a
            // non-transient state (the CAS would miss anyway).
            continue;
        };
        let sandbox_uuid = match parse_sbx_uuid(&row.sandbox_id) {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(
                    sandbox_id = %row.sandbox_id,
                    error = %e,
                    "sandbox transient-takeover: bad typed-id"
                );
                continue;
            }
        };
        match db
            .update_sandbox_status(sandbox_uuid, target, row.generation, None)
            .await
        {
            Ok(_) => {
                recovered += 1;
                tracing::info!(
                    sandbox_id = %row.sandbox_id,
                    from = row.status.as_str(),
                    to = target.as_str(),
                    "sandbox transient-takeover: recovered abandoned transient"
                );
            }
            Err(crate::db::DatabaseError::CasLost { .. }) => {
                tracing::info!(
                    sandbox_id = %row.sandbox_id,
                    "sandbox transient-takeover: CAS lost (peer took over)"
                );
            }
            Err(e) => {
                tracing::warn!(
                    sandbox_id = %row.sandbox_id,
                    error = %e,
                    "sandbox transient-takeover: CAS failed"
                );
            }
        }
    }
    (seen, recovered)
}

/// Spawn the transient-state takeover loop on the compio runtime.
/// Detached; lives for the process lifetime, observes
/// `state.shutdown_requested()` between iterations.
pub fn spawn_transient_state_takeover(state: Arc<AppState>) {
    if state.database.is_none() {
        // pg disabled — nothing to sweep.
        return;
    }
    compio::runtime::spawn(async move {
        let interval = Duration::from_secs(TRANSIENT_TAKEOVER_POLL_SECS);
        let threshold = read_i64_env(
            "SANDBOX_TRANSIENT_STATE_TIMEOUT_SECS",
            DEFAULT_TRANSIENT_TIMEOUT_SECS,
        );
        tracing::info!(
            interval_secs = TRANSIENT_TAKEOVER_POLL_SECS,
            threshold_secs = threshold,
            "sandbox transient-takeover: loop started"
        );
        loop {
            if state.shutdown_requested() {
                tracing::info!("sandbox transient-takeover: shutdown");
                break;
            }
            compio::time::sleep(interval).await;
            if state.shutdown_requested() {
                break;
            }
            let (seen, recovered) = run_transient_takeover_once(&state, threshold).await;
            if seen > 0 {
                tracing::debug!(seen, recovered, "sandbox transient-takeover: tick");
            }
        }
    })
    .detach();
}

// ────────────────────────────────────────────────────────────────────
// Idle-eviction sweep
// ────────────────────────────────────────────────────────────────────

/// Trait the idle sweep delegates to for actually snapshotting a
/// row. Kept abstract so the pg-gated tests can substitute a
/// recording stub without spinning up a fake CH backend.
///
/// Production wires a closure that calls
/// `snapshot_handler::snapshot_sandbox` with the controller's
/// real `SnapshotStore` + `ChRemoteClient` + `SourceVmOps`.
pub trait IdleSnapshotter: Send + Sync {
    fn snapshot_one(&self, sandbox_id: Uuid) -> Result<(), String>;
}

/// Simplest fixture: a `Mutex<Vec<Uuid>>` that records the rows the
/// sweep tried to snapshot. Used by the pg-gated test.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct RecordingIdleSnapshotter {
    pub seen: std::sync::Mutex<Vec<Uuid>>,
    pub fail: std::sync::atomic::AtomicBool,
}
impl IdleSnapshotter for RecordingIdleSnapshotter {
    fn snapshot_one(&self, sandbox_id: Uuid) -> Result<(), String> {
        self.seen
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(sandbox_id);
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            Err("recorder: forced fail".into())
        } else {
            Ok(())
        }
    }
}

/// Run a single iteration of the idle-eviction sweep. Public so the
/// pg-gated test can pin the selection logic. Returns the count of
/// rows that *were attempted* (not necessarily snapshotted —
/// `IdleSnapshotter::snapshot_one` may have failed).
pub async fn run_idle_eviction_once(
    state: &Arc<AppState>,
    snapshotter: &dyn IdleSnapshotter,
    threshold_secs: i64,
    per_iteration_concurrency: usize,
) -> Vec<SandboxRow> {
    let Some(db) = state.database.as_ref() else {
        return Vec::new();
    };
    if !state.config.snapshot_enabled {
        return Vec::new();
    }
    if threshold_secs <= 0 {
        return Vec::new();
    }
    let rows = match db.idle_eligible_sandboxes(threshold_secs, IDLE_BATCH_LIMIT).await {
        Ok(rs) => rs,
        Err(e) => {
            tracing::warn!(error = %e, "sandbox idle-eviction: query failed");
            return Vec::new();
        }
    };
    if rows.is_empty() {
        return Vec::new();
    }
    // v1 throttling: snapshot at most `per_iteration_concurrency` at
    // a time. Compio doesn't ship a Semaphore; we just chunk the
    // rows and process each chunk sequentially. The wall-time
    // estimate is ~2.1s/snapshot, so a chunk of 2 takes ~4 s; idle-
    // sweep cadence is 5 min so this fits comfortably.
    let cap = per_iteration_concurrency.max(1);
    let attempted: Vec<SandboxRow> = rows.clone();
    for chunk in rows.chunks(cap) {
        if state.shutdown_requested() {
            break;
        }
        for r in chunk {
            let sid = match parse_sbx_uuid(&r.sandbox_id) {
                Ok(u) => u,
                Err(e) => {
                    tracing::warn!(
                        sandbox_id = %r.sandbox_id,
                        error = %e,
                        "sandbox idle-eviction: bad typed-id"
                    );
                    continue;
                }
            };
            if let Err(e) = snapshotter.snapshot_one(sid) {
                tracing::warn!(
                    sandbox_id = %r.sandbox_id,
                    error = %e,
                    "sandbox idle-eviction: snapshot_one failed (next tick retries)"
                );
            }
        }
    }
    attempted
}

/// Spawn the idle-eviction loop on the compio runtime. Disabled if
/// `state.config.snapshot_enabled = false` or if
/// `SANDBOX_IDLE_SNAPSHOT_SECS = 0`.
///
/// `snapshotter` is owned: the loop holds it for its lifetime. The
/// production caller wraps the snapshot-handler call site in an
/// `Arc<dyn IdleSnapshotter>` shim; the test wires a recording
/// fixture.
pub fn spawn_idle_eviction_sweep(
    state: Arc<AppState>,
    snapshotter: Arc<dyn IdleSnapshotter>,
) {
    if !state.config.snapshot_enabled {
        tracing::info!("sandbox idle-eviction: skipped (snapshot_enabled=false)");
        return;
    }
    if state.database.is_none() {
        return;
    }
    compio::runtime::spawn(async move {
        let sweep_secs = read_u64_env(
            "SANDBOX_IDLE_SNAPSHOT_SWEEP_SECS",
            DEFAULT_IDLE_SWEEP_SECS,
        )
        .max(1);
        let threshold = read_i64_env(
            "SANDBOX_IDLE_SNAPSHOT_SECS",
            DEFAULT_IDLE_THRESHOLD_SECS,
        );
        let concurrency = read_usize_env(
            "SANDBOX_SNAPSHOT_PER_WORKER_CONCURRENCY",
            DEFAULT_PER_WORKER_CONCURRENCY,
        )
        .max(1);
        if threshold <= 0 {
            tracing::info!(
                "sandbox idle-eviction: disabled (SANDBOX_IDLE_SNAPSHOT_SECS={threshold})"
            );
            return;
        }
        let interval = Duration::from_secs(sweep_secs);
        tracing::info!(
            sweep_secs,
            threshold_secs = threshold,
            concurrency,
            "sandbox idle-eviction: loop started"
        );
        loop {
            if state.shutdown_requested() {
                tracing::info!("sandbox idle-eviction: shutdown");
                break;
            }
            compio::time::sleep(interval).await;
            if state.shutdown_requested() {
                break;
            }
            let attempted =
                run_idle_eviction_once(&state, snapshotter.as_ref(), threshold, concurrency)
                    .await;
            if !attempted.is_empty() {
                tracing::debug!(
                    attempted = attempted.len(),
                    "sandbox idle-eviction: tick"
                );
            }
        }
    })
    .detach();
}

// ────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────

/// Parse the typed-id back to a `Uuid`. Wraps the typed_id helper
/// in a uniform error type for the sweep code paths.
fn parse_sbx_uuid(typed: &str) -> Result<Uuid, String> {
    zeroship_core::typed_id::parse_with_prefix(typed, "sbx")
        .map_err(|e| e.to_string())
}

// Silence unused-import warning when the trait isn't otherwise used.
#[allow(dead_code)]
fn _db_anchor(_: &Database) {}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn recovery_target_pins_proposal_table() {
        // § 9.2 lifecycle table; pinned so a refactor must update
        // both the proposal and this test in lockstep.
        assert_eq!(
            recovery_target(SandboxStatus::Snapshotting),
            Some(SandboxStatus::SnapshottingAborted)
        );
        assert_eq!(
            recovery_target(SandboxStatus::Restoring),
            Some(SandboxStatus::Snapshotted)
        );
        assert_eq!(
            recovery_target(SandboxStatus::RestoringCold),
            Some(SandboxStatus::SnapshottedSuspect)
        );
        // Non-transient states yield None.
        assert_eq!(recovery_target(SandboxStatus::Running), None);
        assert_eq!(recovery_target(SandboxStatus::Snapshotted), None);
    }
}
