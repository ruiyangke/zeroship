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
//!    concurrent snapshots per chunk via `futures::future::join_all`
//!    — chunks themselves run serially so peak parallel work stays
//!    bounded at `N` regardless of batch size.
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

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use crate::db::{Database, SandboxRow, SandboxStatus};
use crate::snapshot_handler::{self, snap_stage_dir, SnapshotHandlerError, SourceVmOps};
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

/// `SANDBOX_SNAPSHOT_PER_WORKER_CONCURRENCY`. Per-chunk cap on
/// concurrent snapshot operations. The sweep loop runs each chunk's
/// rows through `futures::future::join_all` (single compio task, no
/// spawn — `IdleSnapshotter::snapshot_one` futures are intentionally
/// !Send under compio) and processes chunks back-to-back, so peak
/// parallel snapshot work is bounded at this number. The actual
/// snapshot wall (~2.1 s + L2 upload) means a default of 2 saturates
/// the design budget without contention at the 5-min sweep cadence.
pub const DEFAULT_PER_WORKER_CONCURRENCY: usize = 2;

/// `SANDBOX_TRANSIENT_STATE_TIMEOUT_SECS`. § 6.1 default 120 s.
pub const DEFAULT_TRANSIENT_TIMEOUT_SECS: i64 = 120;

/// Cadence for the transient-state takeover sweep. Hard-coded 30s
/// per § 6.1 (operators can edit this constant if real workloads
/// need a tighter loop; not env-driven because the threshold is
/// what matters for correctness, not the poll cadence).
pub const TRANSIENT_TAKEOVER_POLL_SECS: u64 = 30;

/// C-7-LT-PR2: cadence for the wake_jobs GC sweep. 60 s per
/// `docs/proposals/c7-lt-async-wake.md` § 5 ("every 60 s the
/// controller deletes wake_jobs rows where finished_at IS NOT NULL
/// AND now() - finished_at > T_KEEP"). The sweep is cheap (a single
/// indexed DELETE) so the cadence floor is set by responsiveness for
/// clients polling a wake_id around the T_KEEP boundary, not by
/// query cost.
pub(crate) const WAKE_JOBS_GC_POLL_SECS: u64 = 60;

/// C-7-LT-PR2: legacy hard-coded retention. The runtime value now
/// lives on [`crate::config::WakeLifecycleConfig::wake_jobs_gc_retention_secs`]
/// (env `SANDBOX_WAKE_JOBS_GC_RETENTION_SECS`); this constant is the
/// fallback default + a single source of truth for the proposal-
/// documented value (5 min per § 2.cleanup, matching the standard
/// async-operation cleanup story — S3 multipart, GCP LRO).
///
/// R16-S5: the controller reads
/// `state.wake_lifecycle.wake_jobs_gc_retention_secs` at every
/// sweep tick, not this constant — operators can shorten the value
/// for dev / test or lengthen it for high-latency clients (the
/// minimum is 1 s, enforced by `WakeLifecycleConfig::from_env`).
pub(crate) const WAKE_JOBS_T_KEEP: Duration = Duration::from_secs(300);

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
        // C1-FOLLOWUP (concurrency-r9): the recovery CAS must fence
        // on the CRASHED controller's `host_id` (the value the sweep
        // query just observed), not on `self.host_id()`. Using
        // `update_sandbox_status` here was a bug: that path fences
        // `host_id = self.host_id()`, which by definition never
        // matches a row owned by a different (crashed) controller,
        // so §6.1 found candidates but never claimed any of them.
        //
        // `claim_orphan_transient_for_recovery` inverts the fence:
        // CAS predicate = `(host_id = row.host_id, generation =
        // row.generation, lessee_updated_at still stale)` and on
        // success atomically transfers ownership to `self.host_id()`,
        // bumps generation, flips status to the recovery target, and
        // clears `lessee_updated_at`.
        match db
            .claim_orphan_transient_for_recovery(
                sandbox_uuid,
                target,
                row.generation,
                &row.host_id,
                threshold_secs,
            )
            .await
        {
            Ok(_) => {
                recovered += 1;
                tracing::info!(
                    sandbox_id = %row.sandbox_id,
                    from = row.status.as_str(),
                    to = target.as_str(),
                    crashed_host_id = %row.host_id,
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

/// Spawn the transient-state takeover loop on a dedicated OS thread with
/// its own compio runtime. Lives for the process lifetime, observes
/// `state.shutdown_requested()` between iterations.
///
/// R16-I1: same C-6 wedge fingerprint as the other periodic tasks — the
/// CAS-takeover path inside `run_transient_takeover_once` issues pg
/// UPDATEs that can stall under contention, and on the shared ntex worker
/// runtime that would starve sibling wake/probe tasks. Running on its
/// own private compio runtime decouples this loop from the worker.
pub fn spawn_transient_state_takeover(state: Arc<AppState>) {
    if state.database.is_none() {
        // pg disabled — nothing to sweep.
        return;
    }
    crate::detach::detach_isolated("snap-transient", move || async move {
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
    });
}

// ────────────────────────────────────────────────────────────────────
// C-7-LT-PR2: wake_jobs GC sweep
// ────────────────────────────────────────────────────────────────────

/// Run a single iteration of the wake_jobs GC sweep. Calls
/// `Database::gc_expired_wake_jobs(retention)` where `retention` is
/// taken from `state.wake_lifecycle.wake_jobs_gc_retention_secs`
/// (env `SANDBOX_WAKE_JOBS_GC_RETENTION_SECS`, default 300 s). Returns
/// the number of rows deleted. Errors are logged-and-continued.
///
/// Public so the pg-gated tests can drive a single pass without
/// spawning the loop.
pub async fn run_wake_jobs_gc_once(state: &Arc<AppState>) -> u64 {
    let Some(db) = state.database.as_ref() else {
        return 0;
    };
    let retention_secs = state.wake_lifecycle.wake_jobs_gc_retention_secs;
    let retention = Duration::from_secs(retention_secs);
    match db.gc_expired_wake_jobs(retention).await {
        Ok(n) => {
            if n > 0 {
                tracing::info!(
                    deleted = n,
                    t_keep_secs = retention_secs,
                    "sandbox wake_jobs GC: deleted terminal rows"
                );
            }
            n
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "sandbox wake_jobs GC: sweep failed (continuing)"
            );
            0
        }
    }
}

/// Spawn the wake_jobs GC loop. Runs on its own dedicated OS thread
/// with a private compio runtime (`detach_isolated`) so the GC's pg
/// DELETE doesn't share runtime time with the ntex worker. Lives for
/// the process lifetime, observes `state.shutdown_requested()`
/// between iterations.
///
/// Skipped when `state.database` is `None` — without pg there are
/// no wake_jobs rows to GC.
pub(crate) fn spawn_wake_jobs_gc(state: Arc<AppState>) {
    if state.database.is_none() {
        return;
    }
    crate::detach::detach_isolated("wake-gc", move || async move {
        let interval = Duration::from_secs(WAKE_JOBS_GC_POLL_SECS);
        tracing::info!(
            interval_secs = WAKE_JOBS_GC_POLL_SECS,
            t_keep_secs = state.wake_lifecycle.wake_jobs_gc_retention_secs,
            "sandbox wake_jobs GC: loop started"
        );
        loop {
            if state.shutdown_requested() {
                tracing::info!("sandbox wake_jobs GC: shutdown");
                break;
            }
            compio::time::sleep(interval).await;
            if state.shutdown_requested() {
                break;
            }
            let _ = run_wake_jobs_gc_once(&state).await;
        }
    });
}

// ────────────────────────────────────────────────────────────────────
// R19-C1: wake_jobs takeover sweep
// ────────────────────────────────────────────────────────────────────

/// R19-C1: cadence for the wake_jobs takeover sweep. 60 s mirrors
/// the GC sweep — same fingerprint (one indexed UPDATE backed by
/// `wake_jobs_lessee_idx`), cheap regardless of fleet size. The
/// per-iteration *threshold* (how stale a row must be to claim) is
/// configurable via `SANDBOX_WAKE_JOBS_TAKEOVER_THRESHOLD_SECS`;
/// this poll cadence is hard-coded since the threshold is what
/// matters for correctness, not the poll cycle.
pub(crate) const WAKE_JOBS_TAKEOVER_POLL_SECS: u64 = 60;

/// Run a single iteration of the wake_jobs takeover sweep. Calls
/// `Database::claim_orphan_wake_for_recovery(threshold)` where
/// `threshold` is taken from
/// `state.wake_lifecycle.takeover_threshold_secs` (env
/// `SANDBOX_WAKE_JOBS_TAKEOVER_THRESHOLD_SECS`, default 60 s).
/// Returns the number of rows claimed. Errors are logged-and-
/// continued (the next tick retries; pg may have blipped).
///
/// Public so the pg-gated tests can drive a single pass without
/// spawning the loop.
///
/// **Why this loop matters.** Without it, a controller crash mid-
/// wake leaves the row in a non-terminal state. The GC sweep skips
/// it (only `ok`/`failed` rows). The GATE-C2 UNIQUE INDEX
/// (migration 0011) then blocks every subsequent wake POST for
/// that sandbox: handler short-circuits with `Replay(stale_row)`
/// pointing at the dead wake_id, client polls forever. This loop
/// reads the `lessee_updated_at` column R17-A1 writes — closing
/// the loop on the wedge concurrency-r19 escalated as R19-C1.
pub async fn run_wake_jobs_takeover_once(state: &Arc<AppState>) -> u64 {
    let Some(db) = state.database.as_ref() else {
        return 0;
    };
    let threshold_secs = state.wake_lifecycle.takeover_threshold_secs;
    let threshold = Duration::from_secs(threshold_secs);
    match db.claim_orphan_wake_for_recovery(threshold).await {
        Ok(n) => {
            if n > 0 {
                tracing::warn!(
                    target: "sandbox::wake::takeover",
                    claimed = n,
                    threshold_secs,
                    closure_ref = "R19-C1",
                    "sandbox wake_jobs takeover: claimed orphan rows \
                     (wake worker aborted mid-wake; rows \
                     transitioned to failed/wake_worker_aborted)"
                );
            } else {
                tracing::debug!(
                    target: "sandbox::wake::takeover",
                    threshold_secs,
                    "sandbox wake_jobs takeover: no orphans"
                );
            }
            n
        }
        Err(e) => {
            tracing::warn!(
                target: "sandbox::wake::takeover",
                error = %e,
                threshold_secs,
                "sandbox wake_jobs takeover: sweep failed (continuing)"
            );
            0
        }
    }
}

/// Spawn the wake_jobs takeover loop. Runs on its own dedicated OS
/// thread with a private compio runtime (`detach_isolated`), same
/// fingerprint as `spawn_wake_jobs_gc` — the takeover's pg UPDATE
/// shares the wake_jobs table with hot inserts from the wake-POST
/// handler, so we keep it off the shared ntex runtime so a stalled
/// pg call cannot starve sibling wake handlers.
///
/// Lives for the process lifetime, observes
/// `state.shutdown_requested()` between iterations. Skipped when
/// `state.database` is `None`.
pub(crate) fn spawn_wake_jobs_takeover(state: Arc<AppState>) {
    if state.database.is_none() {
        return;
    }
    crate::detach::detach_isolated("wake-takeover", move || async move {
        let interval = Duration::from_secs(WAKE_JOBS_TAKEOVER_POLL_SECS);
        tracing::info!(
            target: "sandbox::wake::takeover",
            interval_secs = WAKE_JOBS_TAKEOVER_POLL_SECS,
            threshold_secs = state.wake_lifecycle.takeover_threshold_secs,
            "sandbox wake_jobs takeover: loop started"
        );
        loop {
            if state.shutdown_requested() {
                tracing::info!(
                    target: "sandbox::wake::takeover",
                    "sandbox wake_jobs takeover: shutdown"
                );
                break;
            }
            compio::time::sleep(interval).await;
            if state.shutdown_requested() {
                break;
            }
            let _ = run_wake_jobs_takeover_once(&state).await;
        }
    });
}

// ────────────────────────────────────────────────────────────────────
// Idle-eviction sweep
// ────────────────────────────────────────────────────────────────────

/// Trait the idle sweep delegates to for actually snapshotting a
/// row. Kept abstract so the pg-gated tests can substitute a
/// recording stub without spinning up a fake CH backend.
///
/// Production wires [`ControllerIdleSnapshotter`] which calls
/// `snapshot_handler::snapshot_sandbox` with the controller's
/// real `SnapshotStore` + `ChRemoteClient` + `SourceVmOps`. Async
/// because the underlying pipeline is async — making the sweep
/// loop itself async preserves per-iteration throttling and
/// shutdown observability that a fire-and-forget sync method would
/// lose.
pub trait IdleSnapshotter: Send + Sync {
    fn snapshot_one<'a>(
        &'a self,
        sandbox_id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + 'a>>;
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
    fn snapshot_one<'a>(
        &'a self,
        sandbox_id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + 'a>> {
        Box::pin(async move {
            self.seen
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(sandbox_id);
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                Err("recorder: forced fail".into())
            } else {
                Ok(())
            }
        })
    }
}

/// Production [`IdleSnapshotter`]: bridges the sweep loop to the
/// real `snapshot_handler::snapshot_sandbox` pipeline.
///
/// Holds an `Arc<AppState>` so it can read `backend`, `snapshot_store`,
/// `ch_remote`, `database`, and `config.snapshot_l1_root` at call time.
/// Cheap to clone (just bumps the Arc refcount). The sweep loop holds
/// exactly one `Arc<ControllerIdleSnapshotter>`, plus its own
/// `Arc<AppState>` — no cycle (the snapshotter never references itself).
#[allow(missing_debug_implementations)]
pub struct ControllerIdleSnapshotter {
    state: Arc<AppState>,
}

impl ControllerIdleSnapshotter {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }
}

impl IdleSnapshotter for ControllerIdleSnapshotter {
    fn snapshot_one<'a>(
        &'a self,
        sandbox_id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + 'a>> {
        Box::pin(async move {
            // Mirror admin_handlers::snapshot_sandbox preflight: refuse
            // if the snapshot trio isn't wired. The sweep itself gates
            // on `state.config.snapshot_enabled` and `database.is_some()`
            // before spawning, but the store/ch_remote could still be
            // None in tests that construct AppState directly.
            let (store, ch, db) = match (
                self.state.snapshot_store.as_ref(),
                self.state.ch_remote.as_ref(),
                self.state.database.as_ref(),
            ) {
                (Some(s), Some(c), Some(d)) => (s, c, d),
                _ => return Err("snapshot wiring incomplete (store/ch/db None)".into()),
            };

            // Resolve the source VM identity. Mirrors admin handler
            // pre-CAS lookup so a missing alloc / unreachable Nomad
            // surfaces with the row still `running`.
            let handle = match self.state.backend.lookup_source_vm_ops(sandbox_id).await {
                Ok(h) => h,
                Err(e) => return Err(format!("lookup_source_vm_ops: {e}")),
            };

            let stage_dir = snap_stage_dir(
                &self.state.config.snapshot_l1_root,
                sandbox_id,
            );
            let vm_ops = ResolvedSourceVmOps { handle };

            let outcome = snapshot_handler::snapshot_sandbox(
                db.as_ref(),
                Arc::clone(store),
                Arc::clone(ch),
                &vm_ops,
                sandbox_id,
                stage_dir,
                self.state.config.snapshot_enabled,
            )
            .await;

            match outcome {
                Ok(_) => {
                    // Best-effort source teardown post-snapshot — same
                    // shape as the admin handler. A teardown failure
                    // leaves a runtime-orphan that the next-boot orphan-
                    // prune sweeps; the pg row is already `snapshotted`.
                    if let Err(e) = self
                        .state
                        .backend
                        .teardown_source_for_snapshot(sandbox_id)
                        .await
                    {
                        tracing::warn!(
                            sandbox_id = %sandbox_id,
                            error = %e,
                            "sandbox idle-eviction: source teardown failed \
                             (non-fatal; orphan-prune will reclaim)"
                        );
                    }
                    Ok(())
                }
                Err(SnapshotHandlerError::StateMismatch { current }) => {
                    // Row moved out from under us between idle-query
                    // and CAS — perfectly normal under load. Not an
                    // error worth alarming on.
                    tracing::debug!(
                        sandbox_id = %sandbox_id,
                        current,
                        "sandbox idle-eviction: state moved before CAS (peer raced)"
                    );
                    Ok(())
                }
                Err(e) => Err(format!("snapshot_sandbox: {e}")),
            }
        })
    }
}

/// Local adapter mirroring `admin_handlers::ResolvedSourceVmOps`.
/// Kept private to this module — the admin-handler copy is also
/// private; duplicating it avoids a pub-export churn for a 20-line
/// type. The handle is resolved via the async `lookup_source_vm_ops`
/// before snapshot, so `locate_*` ignore `sandbox_id`.
struct ResolvedSourceVmOps {
    handle: crate::backend::nomad_ch::SourceVmOpsHandle,
}

impl SourceVmOps for ResolvedSourceVmOps {
    fn locate_api_socket(&self, _sandbox_id: Uuid) -> Option<std::path::PathBuf> {
        Some(self.handle.api_socket.clone())
    }
    fn locate_vm_index(&self, _sandbox_id: Uuid) -> Option<i16> {
        i16::try_from(self.handle.vm_index).ok()
    }
    fn teardown_source(&self, _sandbox_id: Uuid) -> Result<(), String> {
        // No-op: the async teardown is invoked from `snapshot_one`
        // after `snapshot_sandbox` returns Ok, mirroring the admin
        // handler. Keeps the snapshot handler's sync interior pure.
        Ok(())
    }
}

/// Run a single iteration of the idle-eviction sweep. Public so the
/// pg-gated test can pin the selection logic. Returns the rows that
/// *were attempted* — rows whose `IdleSnapshotter::snapshot_one`
/// was actually invoked (the call itself may have failed). Rows
/// skipped because of graceful shutdown between chunks, or because
/// their typed-id failed to parse, are NOT included.
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
    let cap = per_iteration_concurrency.max(1);
    let shutdown_flag = || state.shutdown_requested();
    snapshot_rows_chunked(&rows, snapshotter, cap, &shutdown_flag).await
}

/// Snapshot a batch of rows in chunks of `cap`, running the
/// per-row `snapshot_one` calls within each chunk concurrently and
/// the chunks themselves serially. Concurrency is `futures::future::
/// join_all` over a single compio task — no spawn, no Send bound on
/// the per-row futures (T6 left them !Send because compio is
/// single-threaded). The chunks-serial / within-chunk-concurrent
/// shape caps peak parallel snapshot work to `cap` while letting an
/// in-flight L2 upload overlap with another row's `snapshot_sandbox`
/// CPU work — wall-time on a 100-row sweep drops from ~210 s
/// (serial) to ~210/cap s.
///
/// Returns the rows for which `snapshot_one` was actually invoked.
/// Rows skipped because of graceful shutdown between chunks, or
/// because their typed-id failed to parse, are intentionally
/// excluded — `run_idle_eviction_once` propagates this vector to
/// callers as its `attempted` result so log lines and tests reflect
/// what the sweep actually touched (R3-Q1).
///
/// Factored out of `run_idle_eviction_once` so the unit test can
/// pin the actually-concurrent behaviour without needing a
/// `Database` fixture.
async fn snapshot_rows_chunked(
    rows: &[SandboxRow],
    snapshotter: &dyn IdleSnapshotter,
    cap: usize,
    shutdown: &dyn Fn() -> bool,
) -> Vec<SandboxRow> {
    let mut attempted: Vec<SandboxRow> = Vec::with_capacity(rows.len());
    for chunk in rows.chunks(cap) {
        if shutdown() {
            break;
        }
        // Parse typed-ids first; log + drop any that fail. The
        // surviving (row, uuid) pairs feed `join_all` so the actual
        // snapshot work overlaps within the chunk.
        let parsed: Vec<(&SandboxRow, Uuid)> = chunk
            .iter()
            .filter_map(|r| match parse_sbx_uuid(&r.sandbox_id) {
                Ok(u) => Some((r, u)),
                Err(e) => {
                    tracing::warn!(
                        sandbox_id = %r.sandbox_id,
                        error = %e,
                        "sandbox idle-eviction: bad typed-id"
                    );
                    None
                }
            })
            .collect();
        if parsed.is_empty() {
            continue;
        }
        // Record attempted rows *before* awaiting — these are the
        // rows whose `snapshot_one` future is about to be polled.
        // Doing this pre-await means a shutdown observed mid-chunk
        // (between `await` resumes) still leaves the chunk's rows
        // in `attempted`, since `snapshot_one` was already invoked.
        for (row, _) in &parsed {
            attempted.push((*row).clone());
        }
        let results = futures::future::join_all(
            parsed.iter().map(|(_, uuid)| snapshotter.snapshot_one(*uuid)),
        )
        .await;
        for ((row, _), result) in parsed.iter().zip(results) {
            if let Err(e) = result {
                tracing::warn!(
                    sandbox_id = %row.sandbox_id,
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
    // R16-I1 (R14-C1 sibling-C-6): dedicated OS thread + private compio
    // runtime. The idle-eviction sweep's per-iteration work issues
    // `ChRemoteClient` snapshot calls + backend teardowns; each can stall
    // for tens of seconds against an unhealthy agent. On the shared ntex
    // worker runtime that starves sibling wake handlers (the exact C-6
    // fingerprint admin_handlers::teardown_source_for_snapshot already
    // closed at its own site).
    crate::detach::detach_isolated("snap-idle-evict", move || async move {
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
    });
}

// ────────────────────────────────────────────────────────────────────
// T-8b-stress-r2 controller v34: host_dir GC sweep
// ────────────────────────────────────────────────────────────────────
//
// **Why this exists.** T-8b-stress-r2 caught a race between a failing
// CREATE's per-alloc `rm -rf host_dir` (in `CreateGuard::drop`) and a
// concurrent retry-CREATE's `StartTask` running on the same Nomad worker
// for the same `sandbox_id`. The retry's `nomad-driver-ch::StartTask`
// preflight saw `<host_dir>/workspace.img` missing — even though the
// controller's `create_ext4_image_if_missing` had successfully created
// it — because the prior alloc's cleanup unlinked the parent dir mid-
// retry. 48/60 CREATEs hit this; the controller v33 `fsync_dir` +
// `assert_disk_image_present` parity check addressed staging visibility
// (a real issue) but not THIS race.
//
// **Fix shape (sweeper-owned host_dir GC).** Per-alloc cleanup paths —
// `CreateGuard::drop` step 3, `stop_inner` step 5 — now LEAK the
// host_dir. The sweeper here reaps it after:
//   - The sandbox is in a terminal state (`stopped` / `lost` / `orphan`)
//     OR there's no DB row at all for the dir's `<uuid>` (truly
//     orphaned from a prior controller's deleted-state).
//   - There is NO pending `wake_jobs` row for the sandbox (a non-
//     terminal `wake_jobs` row means a restore is in flight and will
//     need `workspace.img` shortly — reaping now would silently break
//     wake).
//   - The directory's mtime is older than `GRACE_SECS` (default 1 hour,
//     env `SANDBOX_HOST_DIR_GC_GRACE_SECS`). Operators retain on-disk
//     artefacts for inspection during the grace window.
//
// **What we DON'T reap.** The per-user home directory tree
// (`<host_state_dir>/users/<user_id>/home.img`) is user-scoped state
// owned by the user lifecycle, not the sandbox lifecycle. The sweep
// iterates `host_state_dir` direct children and SKIPS the literal
// directory named `users` — any other non-UUID name is also skipped
// (defense in depth: operator artefacts under a custom-named subdir
// are never touched).

/// `SANDBOX_HOST_DIR_GC_POLL_SECS`. Cadence between sweeps. 300 s (5 min)
/// matches the idle-eviction sweep's cadence — the work is cheap (one
/// readdir on `host_state_dir` + N pg lookups + best-effort `rm -rf`)
/// and a tighter cadence would barely reduce the time-to-reap given the
/// 1-hour mtime grace. Env tunable; floor 60 s (sub-minute polling
/// would hammer pg's `get_sandbox_row` for no benefit).
pub(crate) const HOST_DIR_GC_POLL_SECS: u64 = 300;

/// Minimum cadence floor — operator can shorten via env but not below
/// this. The floor is set by the cost-of-polling pg vs. the value of
/// faster reaping: a 1-minute cadence reaps within 1 hour 1 min instead
/// of 1 hour 5 min, which is irrelevant.
pub(crate) const HOST_DIR_GC_POLL_FLOOR_SECS: u64 = 60;

/// `SANDBOX_HOST_DIR_GC_GRACE_SECS`. Minimum mtime age before a
/// host_dir is eligible for GC, in seconds. Default 3600 (1 hour).
/// Operators can shorten this for dev / test (env minimum 60 s — see
/// `HOST_DIR_GC_GRACE_FLOOR_SECS`) or lengthen for forensic-friendly
/// production fleets.
pub(crate) const HOST_DIR_GC_GRACE_SECS: u64 = 3600;

/// Minimum grace floor. 60 s is the absolute minimum: short enough for
/// test fixtures to drive the sweeper to completion, long enough that
/// a fresh CREATE's StartTask (typically <10 s) cannot lose the race
/// against a sweep tick that fires immediately after the CREATE's
/// CreateGuard::drop leaves the dir. The mandate's 1-hour default has
/// plenty of headroom over this floor; the floor exists so a careless
/// env value (`=0`) can't disable the safety entirely.
pub(crate) const HOST_DIR_GC_GRACE_FLOOR_SECS: u64 = 60;

/// Classification of a single readdir entry under `host_state_dir`,
/// computed from filesystem-only signals (name + dir-bit + mtime).
/// Pure helper extracted so the unit test suite can drive the
/// non-DB gates without spinning up a `Database`.
///
/// `Skip`   — entry is not a host_dir we own (users subdir, non-UUID
///            name, non-directory, non-UTF8). Caller continues silently.
/// `UnderGrace` — entry is a UUID-named directory but its mtime is
///            younger than `grace_secs`. Caller logs + continues.
/// `Candidate(uuid)` — entry passes all filesystem gates; caller
///            proceeds to DB-eligibility check + reap.
///
/// R25-T4: extracting this lets the test fixture pin every FS gate
/// (the destructive `rm -rf` path branches on the result) without
/// needing a mock `Database`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HostDirEntryDecision {
    Skip,
    UnderGrace { uuid: Uuid, age_secs: u64 },
    Candidate { uuid: Uuid, age_secs: u64 },
}

/// Pure helper: classify a single readdir entry name + metadata
/// against the FS gates. Returns the decision; the caller is
/// responsible for DB lookups + rm -rf on `Candidate`.
///
/// Gates checked here (in order):
///   1. literal name "users" → `Skip` (per-user lifecycle, never reap)
///   2. name does not parse as `Uuid::simple` → `Skip`
///   3. `is_dir == false` → `Skip` (operator artefact / stray file)
///   4. mtime within `grace_secs` of `now_secs` → `UnderGrace`
///   5. all FS gates pass → `Candidate(uuid)`
pub(crate) fn classify_host_dir_entry(
    name: &str,
    is_dir: bool,
    mtime_secs: u64,
    now_secs: u64,
    grace_secs: u64,
) -> HostDirEntryDecision {
    // Gate 1: per-user home tree — never reap.
    if name == "users" {
        return HostDirEntryDecision::Skip;
    }
    // Gate 2: must parse as Uuid::simple (the host_dir layout).
    let uuid = match Uuid::parse_str(name) {
        Ok(u) => u,
        Err(_) => return HostDirEntryDecision::Skip,
    };
    // Gate 3: must be a directory.
    if !is_dir {
        return HostDirEntryDecision::Skip;
    }
    // Gate 4: mtime grace.
    let age_secs = now_secs.saturating_sub(mtime_secs);
    if age_secs < grace_secs {
        return HostDirEntryDecision::UnderGrace { uuid, age_secs };
    }
    HostDirEntryDecision::Candidate { uuid, age_secs }
}

/// Pure helper: eligibility-by-DB-state. Returns `true` iff:
///   - the sandbox row is absent (orphan), OR
///   - the sandbox row is in a terminal state
///     (`stopped` / `lost` / `orphan`)
/// AND there is no pending wake_jobs row.
///
/// R25-T4: extracted so the table-test fixture pins the exact set of
/// terminal states without round-tripping through pg. A future bump
/// of "what counts as terminal" must edit both this fn and the
/// adjacent table-test in lockstep.
pub(crate) fn host_dir_eligible_by_db(
    row: Option<&SandboxRow>,
    has_pending_wake: bool,
) -> bool {
    if has_pending_wake {
        return false;
    }
    match row {
        None => true, // orphan
        Some(r) => matches!(
            r.status,
            SandboxStatus::Stopped | SandboxStatus::Lost | SandboxStatus::Orphan
        ),
    }
}

/// One iteration of the host_dir GC sweep. Public for the pg-gated
/// test suite. Returns `(scanned, reaped)` — `scanned` is the count of
/// candidate `<uuid>`-shaped subdirs the iter found, `reaped` is the
/// count that actually got `rm -rf`'d.
///
/// Per-tick cost: O(N_subdirs × 1 pg lookup × 1 wake_jobs lookup). The
/// pg work is two SELECTs per dir; on a fleet with thousands of
/// long-tail terminal sandboxes the wall is dominated by the rm -rf
/// itself (~hundreds of ms per dir for a populated workspace tree).
/// Operators can shorten the grace if the steady-state size of
/// host_state_dir becomes a problem; today the cost is well within
/// the 5-min sweep budget.
pub async fn run_host_dir_gc_once(
    state: &Arc<AppState>,
    grace_secs: u64,
) -> (u64, u64) {
    let Some(db) = state.database.as_ref() else {
        return (0, 0);
    };

    let host_state_dir = &state.config.nomad_ch.host_state_dir;
    if !host_state_dir.exists() {
        tracing::debug!(
            target: "sandbox::host_dir_gc",
            host_state_dir = %host_state_dir.display(),
            "sandbox host_dir GC: host_state_dir does not exist yet (no sandboxes ever created)"
        );
        return (0, 0);
    }

    let entries = match std::fs::read_dir(host_state_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                target: "sandbox::host_dir_gc",
                error = %e,
                host_state_dir = %host_state_dir.display(),
                "sandbox host_dir GC: readdir failed (continuing)"
            );
            return (0, 0);
        }
    };

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut scanned: u64 = 0;
    let mut reaped: u64 = 0;

    for entry_res in entries {
        if state.shutdown_requested() {
            break;
        }
        let entry = match entry_res {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    target: "sandbox::host_dir_gc",
                    error = %e,
                    "sandbox host_dir GC: readdir entry failed (skipping)"
                );
                continue;
            }
        };
        let name_os = entry.file_name();
        let name = match name_os.to_str() {
            Some(n) => n,
            None => continue, // non-UTF8 name — skip silently
        };

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    target: "sandbox::host_dir_gc",
                    name,
                    error = %e,
                    "sandbox host_dir GC: stat failed (skipping)"
                );
                continue;
            }
        };
        let mtime_secs = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        // FS gates (pure classifier — see `classify_host_dir_entry`):
        // skip the users subdir, non-UUID names, non-directories; bail
        // early on `UnderGrace`. Only `Candidate` proceeds to DB checks.
        let sandbox_uuid = match classify_host_dir_entry(
            name,
            metadata.is_dir(),
            mtime_secs,
            now_secs,
            grace_secs,
        ) {
            HostDirEntryDecision::Skip => continue,
            HostDirEntryDecision::UnderGrace { age_secs, .. } => {
                scanned += 1;
                tracing::debug!(
                    target: "sandbox::host_dir_gc",
                    name,
                    age_secs,
                    grace_secs,
                    "sandbox host_dir GC: dir under grace, skipping"
                );
                continue;
            }
            HostDirEntryDecision::Candidate { uuid, .. } => {
                scanned += 1;
                uuid
            }
        };

        // DB gate 1: sandbox state.
        //
        //   - Row absent → orphan from a deleted sandbox (or pre-v34
        //     leak); reap is safe.
        //   - Row exists + status terminal (stopped/lost/orphan) →
        //     reap is safe (the controller is done with this sandbox).
        //   - Row exists + status non-terminal (running/restoring/etc)
        //     → preserve (a live VM or in-flight restore depends on
        //     workspace.img).
        //   - Row exists + status snapshotted/snapshotted_suspect →
        //     PRESERVE. The workspace.img is durable state needed by
        //     the next wake; reaping would silently break wake.
        let row = match db.get_sandbox_row(sandbox_uuid).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    target: "sandbox::host_dir_gc",
                    name,
                    error = %e,
                    "sandbox host_dir GC: pg query failed (skipping)"
                );
                continue;
            }
        };

        // DB gate 2: no pending wake_jobs row.
        //
        // A non-terminal wake_jobs row means restore_handler.rs is in
        // flight and needs `<host_dir>/restore/` + `workspace.img`. Even
        // if the sandbox row is terminal (e.g. previous boot's stop
        // recorded `stopped` but a wake POST has since arrived and
        // started the wake), reaping now would break the in-flight
        // restore. The sandbox state row transitions terminal → restoring
        // mid-wake (see wake_machine.rs), and the wake_jobs row is the
        // immediate-truth source for in-flight wakes.
        let pending = match db
            .find_pending_wake_for_sandbox(&format!(
                "sbx_{}",
                zeroship_core::typed_id::uuid_to_base62(&sandbox_uuid)
            ))
            .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    target: "sandbox::host_dir_gc",
                    name,
                    error = %e,
                    "sandbox host_dir GC: wake_jobs lookup failed (skipping)"
                );
                continue;
            }
        };

        // Eligibility decided by the pure helper (see
        // `host_dir_eligible_by_db`). Logging stays here so the
        // structured fields still capture the row status.
        if !host_dir_eligible_by_db(row.as_ref(), pending.is_some()) {
            if pending.is_some() {
                tracing::debug!(
                    target: "sandbox::host_dir_gc",
                    name,
                    "sandbox host_dir GC: pending wake_jobs row present, skipping"
                );
            } else {
                tracing::debug!(
                    target: "sandbox::host_dir_gc",
                    name,
                    status = ?row.as_ref().map(|r| r.status.as_str()),
                    "sandbox host_dir GC: sandbox not in terminal state, skipping"
                );
            }
            continue;
        }

        // All gates passed — reap.
        let path = entry.path();
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                reaped += 1;
                tracing::info!(
                    target: "sandbox::host_dir_gc",
                    sandbox_id = %sandbox_uuid,
                    host_dir = %path.display(),
                    sandbox_row_status = ?row.as_ref().map(|r| r.status.as_str()),
                    age_secs = now_secs.saturating_sub(mtime_secs),
                    "sandbox host_dir GC: reaped"
                );
            }
            Err(e) => {
                tracing::warn!(
                    target: "sandbox::host_dir_gc",
                    sandbox_id = %sandbox_uuid,
                    host_dir = %path.display(),
                    error = %e,
                    "sandbox host_dir GC: rm -rf failed (next sweep retries)"
                );
            }
        }
    }

    (scanned, reaped)
}

/// Spawn the host_dir GC loop. Runs on its own dedicated OS thread with
/// a private compio runtime (`detach_isolated`) so the rm -rf work
/// (potentially hundreds of ms per dir for a populated workspace tree)
/// doesn't share runtime time with the ntex worker. Lives for the
/// process lifetime, observes `state.shutdown_requested()` between
/// iterations.
///
/// Skipped when `state.database` is `None` — without pg we can't tell
/// terminal from in-flight sandboxes, and reaping blind would risk
/// breaking a live VM's workspace.img. Single-tenant / no-pg deploys
/// keep the pre-v34 contract (host_dir leaks accumulate until the
/// operator wipes them; matches today's no-DB story for every other
/// pg-only sweep — `wake_jobs_gc`, `transient_state_takeover`, etc.).
pub(crate) fn spawn_host_dir_gc(state: Arc<AppState>) {
    if state.database.is_none() {
        tracing::info!(
            target: "sandbox::host_dir_gc",
            "sandbox host_dir GC: skipped (no database wired — host_dir leaks accumulate; operator must wipe manually)"
        );
        return;
    }
    crate::detach::detach_isolated("host-dir-gc", move || async move {
        let interval_secs = read_u64_env("SANDBOX_HOST_DIR_GC_POLL_SECS", HOST_DIR_GC_POLL_SECS)
            .max(HOST_DIR_GC_POLL_FLOOR_SECS);
        let grace_secs = read_u64_env("SANDBOX_HOST_DIR_GC_GRACE_SECS", HOST_DIR_GC_GRACE_SECS)
            .max(HOST_DIR_GC_GRACE_FLOOR_SECS);
        let interval = Duration::from_secs(interval_secs);
        tracing::info!(
            target: "sandbox::host_dir_gc",
            interval_secs,
            grace_secs,
            host_state_dir = %state.config.nomad_ch.host_state_dir.display(),
            "sandbox host_dir GC: loop started"
        );
        loop {
            if state.shutdown_requested() {
                tracing::info!(
                    target: "sandbox::host_dir_gc",
                    "sandbox host_dir GC: shutdown"
                );
                break;
            }
            compio::time::sleep(interval).await;
            if state.shutdown_requested() {
                break;
            }
            let (scanned, reaped) = run_host_dir_gc_once(&state, grace_secs).await;
            if scanned > 0 || reaped > 0 {
                tracing::debug!(
                    target: "sandbox::host_dir_gc",
                    scanned,
                    reaped,
                    "sandbox host_dir GC: tick"
                );
            }
        }
    });
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    /// Recording snapshotter that sleeps a configurable duration
    /// per call. Used to pin the wall-time of `snapshot_rows_chunked`
    /// so a regression to serial execution shows up as a multiplied
    /// wall-time.
    struct SleepingSnapshotter {
        per_row: Duration,
        max_in_flight: AtomicUsize,
        in_flight: AtomicUsize,
    }
    impl SleepingSnapshotter {
        fn new(per_row: Duration) -> Self {
            Self {
                per_row,
                max_in_flight: AtomicUsize::new(0),
                in_flight: AtomicUsize::new(0),
            }
        }
    }
    impl IdleSnapshotter for SleepingSnapshotter {
        fn snapshot_one<'a>(
            &'a self,
            _sandbox_id: Uuid,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + 'a>> {
            Box::pin(async move {
                let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_in_flight.fetch_max(now, Ordering::SeqCst);
                compio::time::sleep(self.per_row).await;
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            })
        }
    }

    fn row_with_id(id: String) -> SandboxRow {
        SandboxRow {
            sandbox_id: id,
            user_id: "usr_aaaaaaaaaaaaaaaaaaaa".into(),
            project_id: "prj_bbbbbbbbbbbbbbbbbbbb".into(),
            backend: "nomad-ch".into(),
            vm_index: Some(7),
            agent_url: None,
            host_id: "hst_cccccccccccccccccccc".into(),
            generation: 0,
            status: SandboxStatus::Running,
            key_fp: "0123456789abcdef0123456789abcdef".into(),
            created_at_secs: 1_700_000_000,
            started_at_secs: Some(1_700_000_001),
            stopped_at_secs: None,
            last_used_at_secs: 1_700_000_500,
        }
    }

    /// T7 (code-quality-r2): the per-iteration concurrency knob must
    /// actually overlap snapshot work within a chunk. Pre-T7 the
    /// inner loop was sequential under `for r in chunk { .await }`,
    /// so 8 rows × 100 ms slept ~800 ms regardless of cap. With
    /// `futures::future::join_all` driving each chunk, cap=4 over
    /// 8 rows should sleep ~200 ms (two chunks of four, ~100 ms each).
    ///
    /// Asserts (R3-T1): `max_in_flight == cap` — the futures must be
    /// polled concurrently, with all `cap` of them sleeping at once
    /// inside the first chunk. This is the direct semantic check.
    /// A wall-time bound used to be asserted here too (< 500 ms),
    /// but that's flaky on loaded CI runners; we keep elapsed in the
    /// eprintln as a coarse regression hint without making it a
    /// failing assertion.
    #[compio::test]
    async fn sweep_concurrency_is_actually_concurrent() {
        let per_row = Duration::from_millis(100);
        let cap = 4usize;
        let n_rows = 8usize;
        let snapshotter = SleepingSnapshotter::new(per_row);
        let rows: Vec<SandboxRow> = (0..n_rows)
            .map(|_| row_with_id(zeroship_core::typed_id::generate("sbx")))
            .collect();
        let no_shutdown: &dyn Fn() -> bool = &|| false;

        let start = Instant::now();
        snapshot_rows_chunked(&rows, &snapshotter, cap, no_shutdown).await;
        let elapsed = start.elapsed();

        let max_concurrent = snapshotter.max_in_flight.load(Ordering::SeqCst);
        // Soft signal only — wall-time is sensitive to CI load.
        // Serial would sleep n_rows * per_row = 800 ms; concurrent at
        // cap=4 over 8 rows sleeps ~200 ms (two chunks of four). If
        // this diverges wildly from the ~200 ms target, it's a hint
        // worth investigating, but not a test failure.
        eprintln!(
            "sweep_concurrency_is_actually_concurrent: elapsed={elapsed:?} \
             max_in_flight={max_concurrent} (cap={cap}, n_rows={n_rows}, \
             per_row={per_row:?}, serial_floor={:?}, ideal={:?})",
            per_row * n_rows as u32,
            per_row * (n_rows as u32 / cap as u32),
        );
        // The semantic check: futures::join_all must actually overlap
        // within a chunk. Exactly `cap` rows are in the first chunk,
        // each sleeps `per_row` after incrementing `in_flight`; under
        // any sane scheduler all `cap` get to the sleep before any
        // wakes up, so `max_in_flight` must reach exactly `cap`. A
        // serial regression would top out at 1.
        assert_eq!(
            max_concurrent, cap,
            "futures::join_all must overlap within a chunk: \
             max_in_flight={max_concurrent} != cap={cap} \
             (1 = serial regression; <cap = partial overlap)"
        );
    }

    /// R3-Q1: `run_idle_eviction_once` previously computed
    /// `attempted = rows.clone()` *before* the chunk loop, so any
    /// shutdown observed mid-loop silently lied about which rows it
    /// actually touched. The fix populates `attempted` incrementally
    /// inside `snapshot_rows_chunked` for rows whose `snapshot_one`
    /// was actually invoked. This test drives 8 rows with cap=4 and
    /// flips a manual shutdown flag once the first chunk has been
    /// fully recorded by the snapshotter; the second chunk must be
    /// skipped, and `attempted.len()` must equal `cap` — *not* 8.
    #[compio::test]
    async fn idle_sweep_attempted_reflects_partial_shutdown() {
        use std::sync::atomic::AtomicBool;

        /// Snapshotter that records every Uuid it sees and, once it
        /// has seen `trigger_at` calls, flips the shared shutdown
        /// flag. This simulates the production sweep observing a
        /// shutdown between chunks (the flag is checked at the top
        /// of each chunk iteration in `snapshot_rows_chunked`).
        struct ShutdownTriggeringSnapshotter {
            seen: std::sync::Mutex<Vec<Uuid>>,
            shutdown: Arc<AtomicBool>,
            trigger_at: usize,
        }
        impl IdleSnapshotter for ShutdownTriggeringSnapshotter {
            fn snapshot_one<'a>(
                &'a self,
                sandbox_id: Uuid,
            ) -> Pin<Box<dyn Future<Output = Result<(), String>> + 'a>>
            {
                Box::pin(async move {
                    let mut seen = self
                        .seen
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    seen.push(sandbox_id);
                    if seen.len() >= self.trigger_at {
                        self.shutdown.store(true, Ordering::SeqCst);
                    }
                    Ok(())
                })
            }
        }

        let cap = 4usize;
        let n_rows = 8usize;
        let shutdown = Arc::new(AtomicBool::new(false));
        let snapshotter = ShutdownTriggeringSnapshotter {
            seen: std::sync::Mutex::new(Vec::new()),
            shutdown: Arc::clone(&shutdown),
            trigger_at: cap,
        };
        let rows: Vec<SandboxRow> = (0..n_rows)
            .map(|_| row_with_id(zeroship_core::typed_id::generate("sbx")))
            .collect();

        let shutdown_flag_for_closure = Arc::clone(&shutdown);
        let shutdown_fn: &dyn Fn() -> bool =
            &move || shutdown_flag_for_closure.load(Ordering::SeqCst);

        let attempted =
            snapshot_rows_chunked(&rows, &snapshotter, cap, shutdown_fn).await;

        // Only the first chunk's rows had `snapshot_one` invoked.
        // Pre-fix this assertion would fail at the call site in
        // `run_idle_eviction_once` (attempted == rows.clone() == 8);
        // here we pin the chunk-level helper that owns the truth.
        assert_eq!(
            attempted.len(),
            cap,
            "attempted must reflect only the rows actually polled \
             before shutdown took effect; got {} attempted rows \
             (cap={cap}, n_rows={n_rows})",
            attempted.len(),
        );
        let seen = snapshotter.seen.lock().unwrap();
        assert_eq!(
            seen.len(),
            cap,
            "snapshotter should have been called exactly cap times; \
             got seen.len()={} (cap={cap})",
            seen.len(),
        );
        // The recorded rows must be a prefix of the input — order
        // within a chunk is implementation-defined under join_all,
        // but the set must equal the first chunk's rows.
        let first_chunk_ids: std::collections::HashSet<&str> = rows
            .iter()
            .take(cap)
            .map(|r| r.sandbox_id.as_str())
            .collect();
        for row in &attempted {
            assert!(
                first_chunk_ids.contains(row.sandbox_id.as_str()),
                "attempted row {} not in first chunk",
                row.sandbox_id,
            );
        }
    }

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

    // ─── R19-C1 takeover sweep — cadence + threshold pins ─────────
    //
    // The actual sweep behaviour (claim_orphan_wake_for_recovery SQL
    // semantics) is covered by the pg-gated suite in
    // `tests/sandbox_pg_e2e.rs::wake_jobs_crud::claim_orphan_*`. Here
    // we pin the loop constants so a refactor that bumps the cadence
    // or quietly weakens the threshold has to update this file too.

    /// R19-C1: the takeover poll cadence is hard-coded — operators
    /// tune the *threshold* (`SANDBOX_WAKE_JOBS_TAKEOVER_THRESHOLD_SECS`)
    /// not the poll cycle. Pinning the constant catches an
    /// accidental cadence drift that would, e.g., turn a 60 s sweep
    /// into a 600 s sweep and silently keep the wedge open for an
    /// extra 9 minutes per sandbox.
    #[test]
    fn wake_jobs_takeover_cadence_is_60s() {
        assert_eq!(WAKE_JOBS_TAKEOVER_POLL_SECS, 60);
    }

    /// R19-C1: the default takeover threshold is 60 s and the
    /// minimum floor is 30 s. Pinning both means a future bump to
    /// "make the sweep more aggressive" forces this test (and the
    /// concurrency-r19 review correlation) to be updated in
    /// lockstep — preventing a silent value drift below the
    /// in-flight-wake-stage worst case.
    #[test]
    fn wake_lifecycle_takeover_threshold_floor_and_default_pinned() {
        use crate::config::WakeLifecycleConfig;
        assert_eq!(
            WakeLifecycleConfig::DEFAULT_TAKEOVER_THRESHOLD_SECS, 60,
            "R19-C1 default threshold drift"
        );
        assert_eq!(
            WakeLifecycleConfig::MIN_TAKEOVER_THRESHOLD_SECS, 30,
            "R19-C1 minimum threshold drift"
        );
        // Default ≥ minimum is an invariant: `from_env` accepts
        // unset → default, so default below minimum would mean a
        // default-config boot ships a value that the same code path
        // would reject if the operator typed it explicitly.
        assert!(
            WakeLifecycleConfig::DEFAULT_TAKEOVER_THRESHOLD_SECS
                >= WakeLifecycleConfig::MIN_TAKEOVER_THRESHOLD_SECS,
            "default must satisfy the minimum it enforces"
        );
    }

    // ─── R25-T4: host_dir GC eligibility matrix ────────────────────
    //
    // The host_dir GC sweeper at `run_host_dir_gc_once` is destructive
    // (rm -rf <host_state_dir>/<sandbox_id>) and gates the action on
    // six checks:
    //   FS1  name == "users"       → skip (per-user lifecycle)
    //   FS2  name is not a Uuid    → skip
    //   FS3  is_dir == false       → skip
    //   FS4  mtime within grace    → skip (under grace)
    //   DB1  sandbox row state     → reap iff terminal / absent
    //   DB2  pending wake_jobs row → skip if present
    //
    // The bundle commit `e82bffd7` explicitly deferred this matrix to
    // a follow-up. The DB layer (`Database`) is a 4 KLOC concrete
    // struct holding a real pg pool, so we can't easily mock it for
    // lib-tests. Instead the gate logic is split into two pure
    // helpers (`classify_host_dir_entry` for FS gates 1-4 and
    // `host_dir_eligible_by_db` for gates 1-2 on the DB side) and the
    // tests below pin each gate in isolation. The pg-gated tests in
    // `tests/sandbox_pg_e2e.rs` already cover the end-to-end SQL path.

    /// Helper: build a `SandboxRow` in the given status. The other
    /// columns are filler — `host_dir_eligible_by_db` only inspects
    /// `status`.
    fn row_in_status(status: SandboxStatus) -> SandboxRow {
        let mut r = row_with_id("sbx_filler_for_status_check".into());
        r.status = status;
        r
    }

    /// FS1 — the literal "users" subdir is the per-user home tree,
    /// owned by the user lifecycle, not the sandbox lifecycle. The
    /// classifier MUST skip it even when its mtime is older than the
    /// grace window. A misfire here would silently delete every
    /// user's `home.img` on the host.
    #[test]
    fn classify_skips_users_subdir() {
        // "users" with ancient mtime + is_dir + huge grace → still skip.
        let d = classify_host_dir_entry("users", true, 0, 1_000_000_000, 60);
        assert_eq!(d, HostDirEntryDecision::Skip);
    }

    /// FS2 — only `Uuid::simple` names belong to the sandbox host_dir
    /// layout. Anything else (operator artefact, README, stale tag) is
    /// not ours; skip.
    #[test]
    fn classify_skips_non_uuid_names() {
        let d = classify_host_dir_entry("not_a_typed_id", true, 0, 1_000_000_000, 60);
        assert_eq!(d, HostDirEntryDecision::Skip);
        // Empty string, prefixed forms, hex with hyphens — all skip.
        for bad in [
            "",
            "sbx_aaaaaaaaaaaaaaaaaaaa",
            "README.md",
            "00000000-0000-0000-0000-000000000000.bak",
        ] {
            assert_eq!(
                classify_host_dir_entry(bad, true, 0, 1_000_000_000, 60),
                HostDirEntryDecision::Skip,
                "expected Skip for {bad:?}"
            );
        }
    }

    /// FS3 — a regular file or symlink under `host_state_dir` is
    /// operator-planted; skip regardless of name.
    #[test]
    fn classify_skips_non_directory_entries() {
        let uuid = Uuid::now_v7();
        let name = uuid.simple().to_string();
        // Same name + same mtime + same grace, but is_dir=false → Skip.
        let d = classify_host_dir_entry(&name, false, 0, 1_000_000_000, 60);
        assert_eq!(d, HostDirEntryDecision::Skip);
    }

    /// FS4 — a UUID-named directory younger than the grace window is
    /// `UnderGrace`, not `Candidate`. The grace window protects
    /// freshly-created host_dirs from being reaped before the
    /// retry-CREATE's `StartTask` has a chance to land.
    #[test]
    fn classify_respects_mtime_grace() {
        let uuid = Uuid::now_v7();
        let name = uuid.simple().to_string();
        let now = 1_000_000_000u64;
        let grace = 3600u64; // 1 hour
        // mtime = now - 30 min, grace = 1 hour → UnderGrace.
        let mtime = now - 1800;
        let d = classify_host_dir_entry(&name, true, mtime, now, grace);
        assert_eq!(
            d,
            HostDirEntryDecision::UnderGrace {
                uuid,
                age_secs: 1800
            }
        );
    }

    /// FS-pass — UUID-named directory beyond the grace window
    /// surfaces as `Candidate(uuid)`. Caller proceeds to DB checks.
    #[test]
    fn classify_emits_candidate_beyond_grace() {
        let uuid = Uuid::now_v7();
        let name = uuid.simple().to_string();
        let now = 1_000_000_000u64;
        let grace = 600u64; // 10 min (post-R24-A1 default)
        // mtime = now - 1 hour, grace = 10 min → Candidate.
        let mtime = now - 3600;
        let d = classify_host_dir_entry(&name, true, mtime, now, grace);
        assert_eq!(
            d,
            HostDirEntryDecision::Candidate {
                uuid,
                age_secs: 3600
            }
        );
    }

    /// FS4 boundary — `age_secs < grace_secs` skips; `age_secs ==
    /// grace_secs` reaps. The < (strict) inequality is intentional:
    /// at exactly `grace`, the dir has waited its full window.
    #[test]
    fn classify_grace_boundary_is_inclusive() {
        let uuid = Uuid::now_v7();
        let name = uuid.simple().to_string();
        let now = 1_000_000_000u64;
        let grace = 600u64;
        // age == grace → Candidate (boundary inclusive on reap side).
        let mtime = now - grace;
        let d = classify_host_dir_entry(&name, true, mtime, now, grace);
        assert!(
            matches!(d, HostDirEntryDecision::Candidate { .. }),
            "age == grace should be Candidate, got {d:?}"
        );
        // age == grace - 1 → UnderGrace.
        let mtime = now - (grace - 1);
        let d = classify_host_dir_entry(&name, true, mtime, now, grace);
        assert!(
            matches!(d, HostDirEntryDecision::UnderGrace { .. }),
            "age < grace should be UnderGrace, got {d:?}"
        );
    }

    /// FS-clock-skew — if `now < mtime` (clock went backwards or fs
    /// metadata is from the future), `saturating_sub` clamps `age` to
    /// 0 and the dir is treated as under grace. This is the
    /// conservative answer — never reap a dir whose mtime we don't
    /// understand.
    #[test]
    fn classify_clock_skew_is_conservative() {
        let uuid = Uuid::now_v7();
        let name = uuid.simple().to_string();
        let now = 1_000u64;
        let mtime = 2_000u64; // mtime in the future
        let d = classify_host_dir_entry(&name, true, mtime, now, 60);
        // age = saturating_sub(now, mtime) = 0; 0 < 60 → UnderGrace.
        assert_eq!(
            d,
            HostDirEntryDecision::UnderGrace { uuid, age_secs: 0 }
        );
    }

    /// DB1.A — sandbox row absent (orphan from a deleted sandbox or
    /// pre-v34 leak) is eligible. The dir has no live owner, the
    /// grace already elapsed → safe to reap.
    #[test]
    fn db_eligible_when_row_absent() {
        assert!(host_dir_eligible_by_db(None, false));
    }

    /// DB1.B — terminal states (`stopped` / `lost` / `orphan`) are
    /// eligible. The controller is done with the sandbox; the dir is
    /// leaked work product.
    #[test]
    fn db_eligible_for_terminal_states() {
        for st in [
            SandboxStatus::Stopped,
            SandboxStatus::Lost,
            SandboxStatus::Orphan,
        ] {
            let row = row_in_status(st);
            assert!(
                host_dir_eligible_by_db(Some(&row), false),
                "expected eligible for status={:?}",
                st.as_str()
            );
        }
    }

    /// DB1.C — non-terminal states (`running` / `starting` /
    /// `restoring` / `restoring_cold` / `snapshotting`) preserve the
    /// dir. A live or in-flight VM depends on `workspace.img`.
    #[test]
    fn db_skips_non_terminal_states() {
        for st in [
            SandboxStatus::Starting,
            SandboxStatus::Running,
            SandboxStatus::Stopping,
            SandboxStatus::Recreating,
            SandboxStatus::Unreachable,
            SandboxStatus::Snapshotting,
            SandboxStatus::Restoring,
            SandboxStatus::RestoringCold,
            SandboxStatus::SnapshottingAborted,
        ] {
            let row = row_in_status(st);
            assert!(
                !host_dir_eligible_by_db(Some(&row), false),
                "non-terminal status={:?} must NOT be eligible",
                st.as_str()
            );
        }
    }

    /// DB1.D — `snapshotted` / `snapshotted_suspect` are SKIP (NOT
    /// terminal for host_dir-purposes): the `workspace.img` is
    /// durable state needed by the next wake. Reaping would silently
    /// break wake.
    #[test]
    fn db_preserves_snapshotted_states() {
        for st in [SandboxStatus::Snapshotted, SandboxStatus::SnapshottedSuspect] {
            let row = row_in_status(st);
            assert!(
                !host_dir_eligible_by_db(Some(&row), false),
                "snapshotted-family status={:?} must preserve host_dir \
                 (workspace.img is durable wake state)",
                st.as_str()
            );
        }
    }

    /// DB2 — a pending wake_jobs row vetoes the reap, even when the
    /// sandbox row is in a terminal state. The wake_jobs row is the
    /// immediate-truth source for an in-flight wake; the sandbox row
    /// may still read `stopped` if the wake-machine hasn't flipped
    /// it to `restoring` yet.
    #[test]
    fn db_skips_when_pending_wake_present() {
        // Orphan + pending wake → skip.
        assert!(!host_dir_eligible_by_db(None, true));
        // Terminal row + pending wake → skip.
        let stopped = row_in_status(SandboxStatus::Stopped);
        assert!(!host_dir_eligible_by_db(Some(&stopped), true));
    }

    /// Invariant: the floor must never exceed the default — operators
    /// reading the env-tunable should never set a value that
    /// `from_env`'s `.max(floor)` clamps back up to a different
    /// number. (The actual default + floor values are pinned in their
    /// own commit alongside any tuning change; here we just enforce
    /// the relationship between them.)
    #[test]
    fn host_dir_gc_grace_default_at_least_floor() {
        assert!(
            HOST_DIR_GC_GRACE_SECS >= HOST_DIR_GC_GRACE_FLOOR_SECS,
            "default must satisfy the minimum it enforces \
             (default={HOST_DIR_GC_GRACE_SECS}, floor={HOST_DIR_GC_GRACE_FLOOR_SECS})"
        );
    }
}
