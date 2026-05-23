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
use crate::snapshot_handler::{
    self, snap_stage_dir, ChRemoteClient, SnapshotHandlerError, SourceVmOps,
};
use crate::snapshot_store::SnapshotStore;
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
                store.as_ref() as &dyn SnapshotStore,
                ch.as_ref() as &dyn ChRemoteClient,
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
}
