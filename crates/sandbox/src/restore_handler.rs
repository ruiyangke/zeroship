//! `RestoreHandler` — orchestrates `snapshotted → restoring → running`
//! per § 5 / § 6.1 / § 8 of the snapshot-restore proposal.
//!
//! Source-of-truth: docs/proposals/sandbox-snapshot-restore.md
//!
//! ## Flow
//!
//! 1. Read row; refuse anything but `Snapshotted` / `SnapshottedSuspect`.
//! 2. CAS `snapshotted → restoring` (generation-fenced).
//! 3. Allocate `vm_index`: try the row's recorded `snapshot_vm_index`
//!    first per § 5.0/§ 5.1 (sticky); fall back to a fresh `alloc()`.
//!    Cluster-exhaustion → 503 `vm_index_unavailable`.
//! 4. `SnapshotStore::get(sandbox_id, alloc_dir, expected_sha256)`.
//!    `ChecksumMismatch` → CAS `restoring → snapshotted_suspect`,
//!    return 500 `snapshot_corrupt`.
//! 5. Rewrite `config.json` per § 5.1: substitute `payload.cmdline`
//!    IP arg, `disks[].path`, `net[].tap`, `net[].mac`, `fs[].socket`,
//!    `serial.file`. Disks + IP are documented no-ops in v1 but the
//!    code paths exist for the v2 cross-cluster work.
//! 6. Submit a Nomad job (or equivalent backend op) for the new alloc
//!    with `ZSBX_RESTORE_FROM=<alloc_dir>` env so the wrapper script's
//!    PR 3f branch knows to invoke `cloud-hypervisor --restore`.
//! 7. Wait for `/livez` to 200.
//! 8. CAS `restoring → running`; clear snapshot metadata.
//! 9. On any mid-flight failure: CAS `restoring → snapshotted`
//!    (rollback; the artifact is preserved).
//!
//! ## Why a separate module
//!
//! Same rationale as `snapshot_handler.rs`: keep the snapshot/restore
//! orchestration out of `nomad_ch.rs` (already 3.7k lines) and pin
//! the ch-spawn + livez integration behind a trait so the v1 unit
//! tests can drive the flow with a mock backend.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::config::NomadCHConfig;
use crate::db::{Database, DatabaseError, SandboxStatus};
use crate::snapshot_store::SnapshotStore;

/// Outcome shape returned by [`restore_sandbox`].
#[derive(Debug)]
pub struct RestoreOutcome {
    pub sandbox_id: Uuid,
    pub vm_index: i16,
    /// Final pg generation post-CAS to `running`.
    pub generation: i64,
}

/// Why the restore couldn't proceed. Maps to § 10.0 wire envelope:
///
/// - `StateMismatch`        → 409 `state_mismatch`
/// - `FeatureDisabled`      → 501 `feature_disabled`
/// - `VmIndexUnavailable`   → 503 `vm_index_unavailable` (no Retry-After under async-wake contract; clients poll `GET /wake/{id}`)
/// - `SnapshotCorrupt`      → 500 `snapshot_corrupt` (CAS to suspect)
/// - `Backend` / `Database` → 500 (controller-internal)
/// - `NotFound`             → 404
#[derive(Debug, thiserror::Error)]
pub enum RestoreHandlerError {
    #[error("snapshot feature disabled (SANDBOX_SNAPSHOT_ENABLED=false)")]
    FeatureDisabled,

    #[error("state mismatch: expected `snapshotted` or `snapshotted_suspect`, current `{current}`")]
    StateMismatch { current: &'static str },

    #[error("sandbox not found: {0}")]
    NotFound(String),

    #[error("vm_index unavailable (cluster exhausted at vm_index={requested})")]
    VmIndexUnavailable { requested: i16 },

    #[error("snapshot corrupt (sha256 mismatch); row marked snapshotted_suspect")]
    SnapshotCorrupt,

    #[error("snapshot store: {0}")]
    Store(#[from] crate::snapshot_store::SnapshotError),

    #[error("backend: {0}")]
    Backend(String),

    #[error("config.json rewrite: {0}")]
    ConfigRewrite(String),

    #[error("database: {0}")]
    Database(#[from] DatabaseError),

    #[error("internal: {0}")]
    Internal(String),
}

/// C-4 fix (T-8b-smoke-r5 cluster review, 2026-05-25): bounded retry
/// policy used by [`restore_sandbox`] when the source vm_index is
/// momentarily held by an in-flight source-teardown.
///
/// The snapshot endpoint returns 200 **as soon as the artifact is on
/// disk** and detaches `teardown_source_for_snapshot`. That detached
/// task keeps `vm_index` reserved until the host-fence clears
/// (`host_fence_timeout_secs`, default 30–120 s) + the Nomad job is
/// purged (~30 s). A wake arriving in the millisecond range after the
/// snapshot response races that detached teardown and the v1 sticky
/// allocator (`reserve_vm_index(snap.vm_index)`) rejects.
///
/// Fix shape: **caller-side bounded retry**. The reserve is
/// idempotent and cheap (a single mutex `.reserve()` call against
/// `VmIndexAllocator`), so polling until the teardown releases the
/// slot is correct *and* preserves the host-fence invariant (we do
/// not race the fence — we wait for it).
///
/// Why caller-side retry rather than:
/// - (a) snapshot blocks until teardown done — would push snapshot p50
///   from ~6 s to ~50–90 s. Bad SLO.
/// - (b) cross-slot fallback (drop sticky) — requires plumbing an
///   `alloc()` path through the trait + breaks the v1 § 5.0/§ 5.1
///   sticky contract callers depend on (cache warmth + `snap.vm_index`
///   as a wire authority).
/// - (c) decouple vm_index release from host-fence — host_fence is
///   the *primary* FM-F defense against handing a live IP to a new
///   tenant; releasing the slot before the fence clears reopens that
///   race for any concurrent CREATE.
///
/// Default budget: **25 attempts × 2 s interval = 48 s total**
/// (24 sleeps between 25 attempts — the first attempt does not sleep,
/// so wall-time = `(max_attempts - 1) * interval`, not
/// `max_attempts * interval`; R14-Q4 doc off-by-one fix).
///
/// **C-7 fix (T-8b-smoke-r8 cluster review)**: the original v1 default
/// was 60×2s=118s wall-time (60 attempts have 59 sleeps), sized to
/// envelope the worst observed teardown wall-time (host_fence ~60 s
/// + Nomad purge ~30 s ≈ 90 s). That budget exceeded the stress
/// client's 60 s deadline. When the client disconnected at 60 s, ntex
/// dropped the wake handler future mid-`compio::time::sleep.await`,
/// leaving no success/exhausted log — a silent failure with the row
/// wedged at `restoring`.
///
/// **R14-A6 (architecture-r14)**: production backends should derive
/// the policy from `cfg.host_fence_timeout_secs` via
/// [`VmIndexRetryPolicy::from_host_fence_timeout`] rather than rely on
/// the hard-coded default — so a future fence-config bump (or a
/// per-cluster override) scales the wake budget automatically. The
/// `Default` impl remains at the C-7 constants (25×2s=48s) as the
/// test contract anchor and the unit-test fallback for backends
/// without a `cfg` handle (e.g. `StubRestoreBackend` in unit tests).
///
/// Trade-off: under sustained `host_fence` races where the source
/// slot does not vacate within 50 s, wake now surfaces a clean 503
/// `vm_index_unavailable` (with the exhausted-budget warn log)
/// instead of silently hanging until the client times out. An
/// observable failure mode is strictly better than a silent stall.
/// The long-term fix is async response with polling (return 202 +
/// status URL, client polls until ready) — out of scope for the
/// cluster-smoke unblock.
#[derive(Debug, Clone, Copy)]
pub struct VmIndexRetryPolicy {
    /// Maximum number of reserve attempts before giving up and
    /// returning `VmIndexUnavailable`. 1 = no retries (first attempt
    /// is decisive).
    pub max_attempts: u32,
    /// Wall time slept between attempts. Fixed (not exponential) —
    /// the slot frees on a roughly-deterministic ~90 s timeline; the
    /// added jitter of exponential backoff would mostly miss the
    /// release window.
    pub interval: Duration,
}

impl Default for VmIndexRetryPolicy {
    fn default() -> Self {
        // C-7 fix: 25 attempts × 2 s = 48 s wall-time budget
        // (24 sleeps; the first attempt fires immediately). Keeps
        // ≥10 s headroom under the 60 s ntex/stress-client deadline
        // so the exhausted-budget log fires before the client
        // disconnect cancels the future. Preserved as the test
        // contract anchor (see `c7_retry_budget_default_is_under_client_deadline`)
        // and the fallback for backends without a `cfg` handle.
        // Production backends should call
        // [`Self::from_host_fence_timeout`] instead — see R14-A6.
        Self { max_attempts: 25, interval: Duration::from_secs(2) }
    }
}

impl VmIndexRetryPolicy {
    /// Derive the vm-index wake-retry budget from `host_fence_timeout_secs`
    /// so a future fence-config bump scales the budget automatically (R14-A6).
    ///
    /// In `Sync` mode: `budget = MIN(2×fence − 10, 50)` s — the tighter of
    /// the fence-derived and ntex-client-deadline ceilings (C-8a/C-8b).
    /// In `Async` mode: `budget = 2×fence + 10` s — the deadline cap is
    /// dropped because the loop runs on `detach_isolated` with no client
    /// cancellation (C-7-LT-1).
    ///
    /// See `docs/decisions/2026-05-25-vm-index-retry-policy.md` for the full
    /// C-8/C-8a/C-8b/C-7-LT-1 tuning history and the smoke-r13 retrospective
    /// explaining why the 2× factor is a conservative safety margin, not a
    /// compositional model.
    pub fn from_host_fence_timeout(
        host_fence_timeout_secs: u64,
        wake_mode: crate::config::WakeResponseMode,
    ) -> Self {
        const HEADROOM_SECS: u64 = 10;
        const CLIENT_DEADLINE_SECS: u64 = 60;
        const INTERVAL_SECS: u64 = 2;
        const MIN_ATTEMPTS: u32 = 1;

        // C-8b: empirical source-teardown wall-time is ~2× the
        // host_fence_timeout — `wait_for_agent_silent` requires 2
        // consecutive no-reply polls AND the Nomad job purge tail
        // appends a second fence-shaped wait. The fence-derived
        // ceiling must envelope this combined pipeline, not just the
        // fence component. Smoke-r10 measured 60.164 s teardown at
        // fence=30 s (deferred C-8b).
        let teardown_estimate = host_fence_timeout_secs.saturating_mul(2);

        // C-7-LT-1: branch on response mode. In sync the legacy
        // dual-ceiling MIN protects against ntex client-disconnect
        // cancellation; in async the wake loop runs on
        // `detach_isolated` with no client-side cancellation, so we
        // anchor the budget to `2×fence + HEADROOM` (a safety margin
        // past the measured teardown wall-time).
        let effective_budget = match wake_mode {
            crate::config::WakeResponseMode::Sync => {
                // Dual ceilings:
                //   - fence-derived is the IDEAL upper bound (matches
                //     the observed source-teardown wall-time,
                //     post-C-8b 2× factor).
                //   - deadline-derived is the HARD upper bound
                //     (anything past this is silently dropped when
                //     the ntex client disconnects — the C-7 / C-8a
                //     failure mode).
                // Take the MIN — the tighter of the two always wins.
                let max_budget_from_fence =
                    teardown_estimate.saturating_sub(HEADROOM_SECS);
                let max_budget_from_deadline =
                    CLIENT_DEADLINE_SECS.saturating_sub(HEADROOM_SECS);
                max_budget_from_fence.min(max_budget_from_deadline)
            }
            crate::config::WakeResponseMode::Async => {
                // No ntex client deadline binds the server-side state
                // machine — it runs on `detach_isolated` and surfaces
                // terminal status through `wake_jobs` polling. Budget
                // = empirical teardown wall-time + safety margin.
                // Smoke-r12: 60.166 s teardown at fence=30 → 70 s
                // budget envelopes with ~10 s slack.
                teardown_estimate.saturating_add(HEADROOM_SECS)
            }
        };

        let attempts_from_budget = (effective_budget / INTERVAL_SECS).saturating_add(1);
        let max_attempts = u32::try_from(attempts_from_budget)
            .unwrap_or(u32::MAX)
            .max(MIN_ATTEMPTS);
        Self {
            max_attempts,
            interval: Duration::from_secs(INTERVAL_SECS),
        }
    }
}

/// Pieces of the production backend the restore handler touches.
/// Kept behind a trait so the v1 unit tests can inject a stub
/// without dragging the entire `NomadCHBackend` into scope. The
/// real implementation in `nomad_ch.rs` (or its sibling) wires
/// `submit_nomad_job` + `wait_for_agent_livez`.
pub trait RestoreBackend: Send + Sync {
    /// Reserve `vm_index` (the source slot) on this worker. Returns
    /// `Ok(())` on success; `Err(_)` if the slot is already held.
    /// C-4 fix: callers (see [`restore_sandbox`]) now retry on Err
    /// per [`Self::vm_index_retry_policy`] before surfacing as 503,
    /// so the source-teardown's ~90 s vm_index hold no longer races
    /// the wake-immediately-after-snapshot client request.
    fn reserve_vm_index(&self, vm_index: i16) -> Result<(), String>;

    /// Release a previously-reserved vm_index. Idempotent.
    fn release_vm_index(&self, vm_index: i16);

    /// Worker-local alloc-dir path for a sandbox restore. Stable
    /// across restore attempts (so `ZSBX_RESTORE_FROM` is
    /// deterministic). `<host_state_dir>/<sandbox-id>/restore/`.
    fn restore_alloc_dir(&self, sandbox_id: Uuid) -> PathBuf;

    /// Submit a Nomad (or equivalent) job for the restored alloc.
    /// `ZSBX_RESTORE_FROM=<alloc_dir>` must be set in the spawned
    /// task's env so the wrapper's restore branch fires (PR 3f).
    /// Synchronous return on success means the job was *enqueued*;
    /// readiness is signalled by [`Self::wait_for_livez`].
    fn submit_restore_job(
        &self,
        sandbox_id: Uuid,
        vm_index: i16,
        alloc_dir: &Path,
        user_id: &str,
    ) -> Result<(), String>;

    /// Block until the restored VM's agent serves `/livez 200`. Best
    /// implemented over `compio::time::sleep` polling; v1 ships the
    /// existing `wait_for_agent_livez` helper from `nomad_ch.rs`.
    fn wait_for_livez(
        &self,
        sandbox_id: Uuid,
        vm_index: i16,
    ) -> Result<(), String>;

    /// Best-effort teardown after restore failure: kill the alloc,
    /// release vm_index. Mirrors `SourceVmOps::teardown_source`.
    fn teardown_restore(&self, sandbox_id: Uuid, vm_index: i16);

    /// B19 fix (cluster smoke 2026-05-23 r4): install the restored
    /// sandbox into the backend's in-memory state map after
    /// `wait_for_livez` succeeds, so subsequent `exec` / `stop` /
    /// `delete` calls find it. Pre-B19 this step was missing, so
    /// post-wake `exec` returned 500 "sandbox not found in nomad-ch
    /// backend", `stop`/`delete` returned 404, and the vm_index slot
    /// leaked across the controller's uptime.
    ///
    /// `signing_key_bytes` is the 32-byte per-sandbox signing seed
    /// the handler has already unsealed from `Persistence` before
    /// calling here. The agent_url is re-derived inside the impl
    /// (it's a deterministic function of `vm_index` and the
    /// backend's `subnet_second_octet` config — same shape as
    /// `wait_for_livez`).
    ///
    /// **Default impl is a no-op `Ok(())`** so the in-crate
    /// `StubRestoreBackend` (test scaffolding) doesn't need to
    /// implement state-map registration just to keep the existing
    /// pg-gated tests compiling. The real impl on
    /// `RealRestoreBackend` performs the insert through a shared
    /// `Arc<NomadCHBackend>` handle.
    fn register_restored(
        &self,
        _sandbox_id: Uuid,
        _vm_index: i16,
        _signing_key_bytes: [u8; 32],
        _user_id: &str,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Bug #22 fix: re-derive the in-VM agent URL for a given
    /// `vm_index`. Same formula `RealRestoreBackend::wait_for_livez`
    /// + `NomadCHBackend::derive_agent_url` use —
    /// `http://10.<subnet_second_octet>.<100+idx>.2:7777`. Exposed on
    /// the trait so `restore_sandbox` can issue the post-livez
    /// `/_clock_resync` handshake without reaching into backend
    /// internals.
    ///
    /// **No default impl.** R7-S2 fix: B22's initial sketch had a
    /// `"http://127.0.0.1:0"` sentinel default — a bogus URL that
    /// resolves but answers nothing, so any backend that forgot to
    /// override would surface as transport error ("agent down") rather
    /// than the real misconfiguration ("trait method not implemented").
    /// Silent fail-OPEN. Removing the default forces every impl
    /// (including test stubs) to provide a real URL at compile time.
    fn derive_agent_url(&self, vm_index: i16) -> String;

    /// C-4 fix (T-8b-smoke-r5 cluster review): policy the wake path
    /// uses to retry [`Self::reserve_vm_index`] while the source's
    /// detached teardown still holds the slot. Default budget (after
    /// the C-7 fix at T-8b-smoke-r8) is 48 s wall-time (25 attempts ×
    /// 2 s interval, 24 sleeps — the first attempt does not sleep).
    /// Sized to stay strictly below the 60 s ntex/stress-client
    /// deadline so the exhausted-budget log fires before the client
    /// disconnect drops the wake future. Production backends override
    /// via [`VmIndexRetryPolicy::from_host_fence_timeout`] to keep the
    /// budget linked to `cfg.host_fence_timeout_secs` (R14-A6). Test
    /// stubs may override with shorter budgets to keep unit tests
    /// fast. See [`VmIndexRetryPolicy`] for the full trade-off
    /// rationale.
    fn vm_index_retry_policy(&self) -> VmIndexRetryPolicy {
        VmIndexRetryPolicy::default()
    }
}

/// C-4 fix helper: bounded-retry wrapper around
/// [`RestoreBackend::reserve_vm_index`]. Polls the allocator on a
/// [`VmIndexRetryPolicy`] cadence; once the source-teardown releases
/// the slot the next attempt succeeds. Surfaces 503 with the same
/// wire shape as before only after exhausting the budget.
///
/// Extracted from `do_restore_inner` so the retry loop is testable
/// without a Postgres-backed restore flow. The DB-driven happy/sad
/// paths cover the integration; this helper's unit tests pin the
/// loop semantics (succeeds-after-N, exhausts cleanly, single-shot
/// when slot is free).
pub(crate) async fn reserve_vm_index_with_retry(
    backend: &dyn RestoreBackend,
    sandbox_id: Uuid,
    vm_index: i16,
) -> Result<(), RestoreHandlerError> {
    let retry = backend.vm_index_retry_policy();
    let attempts = retry.max_attempts.max(1);
    let mut last_err: Option<String> = None;
    for attempt in 1..=attempts {
        // C-7 fix: per-attempt INFO marker. Smoke-r8 falsified the
        // C-6 runtime-starvation hypothesis: the wake handler was
        // silently canceled by ntex when the stress client's 60 s
        // deadline elapsed, before either the success-after-retry
        // or exhausted-budget branch fired. Emitting before each
        // reserve attempt lets the next smoke see which attempt-N
        // the loop is on when the cancellation lands (or that it
        // never entered the loop at all). Volume is bounded by the
        // policy's `max_attempts` per wake — fine at c=20.
        tracing::info!(
            target: "zeroship_sandbox::restore_handler",
            sandbox_id = %sandbox_id,
            attempt = attempt,
            max_attempts = attempts,
            vm_index = vm_index,
            "restore/wake: reserve_vm_index_with_retry attempt"
        );
        match backend.reserve_vm_index(vm_index) {
            Ok(()) => {
                if attempt > 1 {
                    tracing::info!(
                        sandbox_id = %sandbox_id,
                        vm_index = vm_index,
                        attempts = attempt,
                        "restore/wake: vm_index reserved after retry \
                         (raced source-teardown release)"
                    );
                }
                return Ok(());
            }
            Err(e) => {
                last_err = Some(e);
                if attempt < attempts {
                    compio::time::sleep(retry.interval).await;
                }
            }
        }
    }
    tracing::warn!(
        sandbox_id = %sandbox_id,
        vm_index = vm_index,
        attempts = attempts,
        budget_ms = %(retry.interval.as_millis() as u64
            * u64::from(attempts.saturating_sub(1))),
        last_error = %last_err.as_deref().unwrap_or("<unknown>"),
        "restore/wake: vm_index reserve exhausted retry budget; \
         source-teardown still holding the slot — surfacing 503"
    );
    Err(RestoreHandlerError::VmIndexUnavailable { requested: vm_index })
}

/// Restore a snapshotted sandbox. See module doc for the full flow.
///
/// `persist` is the sealed-record handle the controller built at
/// startup; the wake path calls `persist.unseal(sandbox_id)` after
/// `wait_for_livez` returns Ok to recover the per-sandbox signing
/// key for the state-map insert (B19 fix). `None` makes the
/// post-wake registration a no-op — appropriate for unit tests that
/// drive `restore_sandbox` with a `StubRestoreBackend` (whose
/// default `register_restored` impl is also a no-op).
pub async fn restore_sandbox(
    db: &Database,
    store: Arc<dyn SnapshotStore>,
    backend: Arc<dyn RestoreBackend>,
    persist: Option<&crate::persist::Persistence>,
    sandbox_id: Uuid,
    snapshot_enabled: bool,
) -> Result<RestoreOutcome, RestoreHandlerError> {
    if !snapshot_enabled {
        return Err(RestoreHandlerError::FeatureDisabled);
    }

    // C-6 trace (T-8b-smoke-r6 wake silent-stall investigation,
    // 2026-05-25). The wake handler emitted ZERO log lines between
    // "admin/wake started" (handler entry) and the eventual stall —
    // every step between row read, CAS-to-restoring,
    // reserve_vm_index_with_retry, store.get, submit_restore_job,
    // wait_for_livez, unseal, clock_resync, register_restored, and
    // CAS-to-running was silent. After-the-fact we could not localize
    // which step wedged. These phase-boundary `restore: phase=*` logs
    // let the next cluster smoke pinpoint the stall (whichever phase
    // is the LAST `restore: phase=*` emitted before the 60 s client
    // timeout is the wedge site). Kept at INFO so they show up in the
    // standard controller log without per-restore `RUST_LOG` gymnastics;
    // the volume is one batch of ~14 lines per wake which is fine at
    // c=1 stress and easy to grep at c=20.
    tracing::info!(sandbox_id = %sandbox_id, phase = "entry", "restore: phase");

    // 1. Read row + check state.
    let row = db
        .get_sandbox_row(sandbox_id)
        .await?
        .ok_or_else(|| {
            RestoreHandlerError::NotFound(format!(
                "sbx_{}",
                zeroship_core::typed_id::uuid_to_base62(&sandbox_id)
            ))
        })?;
    tracing::info!(
        sandbox_id = %sandbox_id,
        phase = "row_read_ok",
        status = row.status.as_str(),
        generation = row.generation,
        "restore: phase"
    );
    if !matches!(
        row.status,
        SandboxStatus::Snapshotted | SandboxStatus::SnapshottedSuspect
    ) {
        return Err(RestoreHandlerError::StateMismatch {
            current: row.status.as_str(),
        });
    }
    // SnapshottedSuspect is restorable too — operators may choose to
    // attempt a restore from a suspect artifact (e.g. after manual
    // verification). The artifact's SHA-256 is still verified by
    // `SnapshotStore::get`, so a corrupt suspect artifact cannot be
    // silently accepted. § 8.2 lifecycle table.
    let g0 = row.generation;

    // 2. Look up snapshot metadata (sha256 + vm_index) from pg. The
    //    `SandboxRow` doesn't carry the snapshot_* columns today —
    //    we read them via a focused query against the row at
    //    `snapshotted` state. (Folding them into `SandboxRow` would
    //    ripple through every existing reader; v1 keeps the read
    //    local to this handler.)
    let snap = read_snapshot_row(db, sandbox_id).await?;
    tracing::info!(
        sandbox_id = %sandbox_id,
        phase = "read_snapshot_row_ok",
        vm_index = snap.vm_index,
        "restore: phase"
    );

    // 3. CAS to restoring.
    let g1 = db
        .update_sandbox_status(sandbox_id, SandboxStatus::Restoring, g0, None)
        .await?;
    tracing::info!(
        sandbox_id = %sandbox_id,
        phase = "cas_restoring_ok",
        generation = g1,
        "restore: phase"
    );

    // From here on use a closure + rollback semantics.
    let result = do_restore_inner(
        db,
        Arc::clone(&store),
        Arc::clone(&backend),
        persist,
        sandbox_id,
        &snap,
        g1,
    )
    .await;

    match result {
        Ok((vm_index, g2)) => Ok(RestoreOutcome { sandbox_id, vm_index, generation: g2 }),
        Err(e) => {
            // Rollback path. ChecksumMismatch / SnapshotCorrupt is
            // distinct: row goes to `snapshotted_suspect` instead
            // of bouncing back to `snapshotted` (§ 8.2).
            let target = match &e {
                RestoreHandlerError::SnapshotCorrupt => {
                    SandboxStatus::SnapshottedSuspect
                }
                _ => SandboxStatus::Snapshotted,
            };
            // Best-effort teardown of the partially-spawned alloc.
            //
            // **R10-C2 fix (concurrency-r10 2026-05-25)**: wrap the
            // sync call in `spawn_blocking` so the ntex worker doesn't
            // park for up to 10 s on the ureq DELETE inside
            // `RealRestoreBackend::teardown_restore`. R8-A3-5 wrapped
            // submit_restore_job + wait_for_livez but missed the
            // rollback path. Under c=N concurrent restores all hitting
            // rollback (e.g., GCS hiccup on `store.get`), pre-fix all
            // ntex workers would park simultaneously → 10 s of no
            // progress, surface-level p99 spike. Pattern mirrors
            // `store.get` at lines ~397-411 and `clock_resync_post_restore`
            // at lines ~1530-1616.
            //
            // (b2 choice: wrap at call site, keep callee sync. The
            // `RestoreBackend` trait stays sync so the in-crate
            // `StubRestoreBackend` test scaffolding stays simple; the
            // only async caller is this rollback closure.)
            let backend_for_teardown = Arc::clone(&backend);
            let snap_vm_index = snap.vm_index;
            let sandbox_id_for_teardown = sandbox_id;
            let _ = compio::runtime::spawn_blocking(move || {
                backend_for_teardown
                    .teardown_restore(sandbox_id_for_teardown, snap_vm_index);
            })
            .await;
            if let Err(rb_err) = db
                .update_sandbox_status(sandbox_id, target, g1, None)
                .await
            {
                tracing::error!(
                    sandbox_id = %sandbox_id,
                    rollback_target = target.as_str(),
                    rollback_error = %rb_err,
                    original_error = %e,
                    "restore rollback failed; row may be wedged in `restoring` until lease-takeover sweep"
                );
            }
            Err(e)
        }
    }
}

#[derive(Debug, Clone)]
struct SnapshotRowMeta {
    artifact_path: String,
    sha256: [u8; 32],
    vm_index: i16,
    /// Source sandbox's `user_id` (typed-id form `usr_<base62>`).
    /// Needed by `submit_restore_job` to derive `ZSBX_USER_HOME_IMG`
    /// per the cold-boot env contract (Phase B fix #6).
    user_id: String,
}

/// Focused pg read for the snapshot_* columns. Returns
/// `RestoreHandlerError::Internal` if any required column is NULL —
/// the 0007 CHECK should make that impossible for a `snapshotted` /
/// `snapshotted_suspect` row, but we surface a clear error rather
/// than panic.
async fn read_snapshot_row(
    db: &Database,
    sandbox_id: Uuid,
) -> Result<SnapshotRowMeta, RestoreHandlerError> {
    let pool = db.pool_app().await.map_err(|e| {
        RestoreHandlerError::Internal(format!("pool_app: {e}"))
    })?;
    let client = pool.get().await.map_err(|e| {
        RestoreHandlerError::Internal(format!("pool_app.get: {e}"))
    })?;
    let sandbox_id_typed = format!(
        "sbx_{}",
        zeroship_core::typed_id::uuid_to_base62(&sandbox_id)
    );
    let row = client
        .query_opt(
            "SELECT snapshot_artifact_path, snapshot_sha256, snapshot_vm_index, user_id \
               FROM sandbox.sandboxes \
              WHERE sandbox_id = $1::TEXT AND deleted_at IS NULL",
            &[&sandbox_id_typed],
        )
        .await
        .map_err(|e| RestoreHandlerError::Internal(format!("snap row read: {e}")))?
        .ok_or_else(|| RestoreHandlerError::NotFound(sandbox_id_typed.clone()))?;
    let artifact_path: Option<String> = row.try_get(0).ok();
    let sha_bytes: Option<Vec<u8>> = row.try_get(1).ok();
    let vm_index: Option<i16> = row.try_get(2).ok();
    let user_id: Option<String> = row.try_get(3).ok();
    let (Some(p), Some(s), Some(v), Some(u)) = (artifact_path, sha_bytes, vm_index, user_id) else {
        return Err(RestoreHandlerError::Internal(format!(
            "snapshot row missing required columns for {sandbox_id_typed} \
             (artifact / sha / vm_index / user_id)"
        )));
    };
    if s.len() != 32 {
        return Err(RestoreHandlerError::Internal(format!(
            "snapshot_sha256 length={} expected 32", s.len()
        )));
    }
    let mut sha = [0u8; 32];
    sha.copy_from_slice(&s);
    Ok(SnapshotRowMeta { artifact_path: p, sha256: sha, vm_index: v, user_id: u })
}

async fn do_restore_inner(
    db: &Database,
    store: Arc<dyn SnapshotStore>,
    backend: Arc<dyn RestoreBackend>,
    persist: Option<&crate::persist::Persistence>,
    sandbox_id: Uuid,
    snap: &SnapshotRowMeta,
    expected_generation: i64,
) -> Result<(i16, i64), RestoreHandlerError> {
    // 3 (cont). Reserve vm_index. v1: source slot only. § 5.1
    // "v1 forces vm_index = source vm_index"; cross-worker fallback
    // is documented but not implemented in v1.
    //
    // **C-4 fix** (T-8b-smoke-r5 cluster review, 2026-05-25): the
    // snapshot endpoint detaches `teardown_source_for_snapshot`,
    // which holds the source slot for ~90 s (host_fence + Nomad
    // purge). A wake arriving ms after the snapshot response used to
    // 503 immediately (sticky alloc refused the busy slot). We now
    // retry the reserve on a bounded budget (see
    // [`VmIndexRetryPolicy`]); the slot frees as soon as the
    // detached teardown's `release()` fires. Total budget for
    // production backends is derived from `cfg.host_fence_timeout_secs`
    // via `VmIndexRetryPolicy::from_host_fence_timeout` (R14-A6); the
    // `Default` fallback (used by test stubs without a cfg) is
    // 48 s wall-time (25 × 2 s = 24 sleeps). Was ~118 s pre-C-7
    // (60 × 2 s = 59 sleeps — exceeded the ntex/stress-client 60 s
    // deadline and the wake future was canceled mid-sleep before any
    // exhaustion log fired); exhaustion still surfaces as 503 with
    // the same wire shape.
    tracing::info!(
        sandbox_id = %sandbox_id,
        phase = "pre_reserve_vm_index",
        vm_index = snap.vm_index,
        "restore: phase"
    );
    reserve_vm_index_with_retry(backend.as_ref(), sandbox_id, snap.vm_index).await?;
    tracing::info!(
        sandbox_id = %sandbox_id,
        phase = "post_reserve_vm_index",
        vm_index = snap.vm_index,
        "restore: phase"
    );

    let alloc_dir = backend.restore_alloc_dir(sandbox_id);
    if alloc_dir.exists() {
        std::fs::remove_dir_all(&alloc_dir).map_err(|e| {
            RestoreHandlerError::Internal(format!(
                "remove stale alloc_dir {}: {e}", alloc_dir.display()
            ))
        })?;
    }
    std::fs::create_dir_all(&alloc_dir).map_err(|e| {
        RestoreHandlerError::Internal(format!(
            "create alloc_dir {}: {e}", alloc_dir.display()
        ))
    })?;
    tracing::info!(
        sandbox_id = %sandbox_id,
        phase = "alloc_dir_ready",
        alloc_dir = %alloc_dir.display(),
        "restore: phase"
    );

    // 4. Fetch artifact. ChecksumMismatch / InvalidArtifact → corrupt.
    let sandbox_id_typed = format!(
        "sbx_{}",
        zeroship_core::typed_id::uuid_to_base62(&sandbox_id)
    );
    let _ = &snap.artifact_path; // pg-recorded path; the store may
    // ignore it (the LocalDiskSnapshotStore re-derives from
    // `<root>/<sandbox-id>/`). Surfaced here for tracing/logs.
    // R5-P1b: store.get is sync (sha256 + AEAD decrypt + GCS read on
    // a ~1 GB blob) — run it on the blocking pool so the ntex worker
    // thread can serve other restores while this one's I/O+crypto
    // chews. Pattern mirrors `persist::Persistence::unseal` at
    // `crates/sandbox/src/persist.rs:677-687`. Owned clones of the
    // by-ref args are needed because spawn_blocking requires
    // `'static + Send + FnOnce`.
    tracing::info!(
        sandbox_id = %sandbox_id,
        phase = "pre_store_get",
        "restore: phase"
    );
    let get_result = {
        let store_clone = Arc::clone(&store);
        let sid_clone = sandbox_id_typed.clone();
        let alloc_dir_clone = alloc_dir.clone();
        let sha_clone = snap.sha256;
        compio::runtime::spawn_blocking(move || {
            store_clone.get(&sid_clone, &alloc_dir_clone, &sha_clone)
        })
        .await
        .unwrap_or_else(|p| {
            Err(crate::snapshot_store::SnapshotError::Io(
                std::io::Error::other(format!("spawn_blocking panic: {p:?}")),
            ))
        })
    };
    tracing::info!(
        sandbox_id = %sandbox_id,
        phase = "post_store_get",
        ok = get_result.is_ok(),
        "restore: phase"
    );
    // Bug-#14a diagnostic: surface what's on disk immediately after
    // store.get returns. Prior cluster smokes (2026-05-22) reported
    // the wake-time staging dir was empty despite a successful Ok
    // from store.get. Logging file presence + sizes here gives the
    // next cycle hard evidence whether the controller wrote files
    // that subsequently disappeared, or store.get is mis-claiming
    // success.
    {
        let mut sizes = Vec::with_capacity(3);
        for name in ["config.json", "memory-ranges", "state.json"] {
            let p = alloc_dir.join(name);
            sizes.push(match std::fs::metadata(&p) {
                Ok(m) => format!("{name}={} bytes", m.len()),
                Err(e) => format!("{name}=MISSING ({e})"),
            });
        }
        tracing::info!(
            sandbox_id = %sandbox_id,
            alloc_dir = %alloc_dir.display(),
            stat = %sizes.join(", "),
            "restore: post-store.get staged files"
        );
    }
    if let Err(e) = get_result {
        return Err(match e {
            crate::snapshot_store::SnapshotError::ChecksumMismatch { .. } => {
                tracing::error!(
                    sandbox_id = %sandbox_id,
                    "restore: checksum mismatch — marking snapshotted_suspect"
                );
                RestoreHandlerError::SnapshotCorrupt
            }
            crate::snapshot_store::SnapshotError::InvalidArtifact(s) => {
                // AEAD-auth failure (PR 3c) or magic mismatch is also
                // a "don't trust this artifact" signal. Treat the same
                // way as ChecksumMismatch — suspect + 500.
                tracing::error!(
                    sandbox_id = %sandbox_id,
                    error = %s,
                    "restore: artifact invalid (AEAD auth / magic mismatch)"
                );
                RestoreHandlerError::SnapshotCorrupt
            }
            other => RestoreHandlerError::Store(other),
        });
    }

    // 5. Rewrite config.json per § 5 — controller-side rewrites
    //    ONLY the vm_index-dependent fields (net[].tap, net[].mac).
    //    The path-bearing fields (disks[].path, fs[].socket,
    //    serial.file) are NOT touched here; the wrapper rewrites
    //    them at exec time because only the wrapper knows the
    //    actual NOMAD_TASK_DIR (Nomad assigns the alloc UUID after
    //    job submission). See bug-#8 diagnostic 2026-05-22.
    let config_path = alloc_dir.join("config.json");
    rewrite_config_json(&config_path, snap.vm_index)
        .map_err(RestoreHandlerError::ConfigRewrite)?;
    tracing::info!(
        sandbox_id = %sandbox_id,
        phase = "config_rewritten",
        "restore: phase"
    );

    // 6. Submit the restore job.
    //
    // R8-A3-5 (perf-r8): `submit_restore_job` on `RealRestoreBackend`
    // is internally sync — it POSTs to Nomad over a blocking ureq
    // client, then `std::thread::sleep(250ms)`-polls allocations
    // until the alloc reaches `running` (typically 2-4 s, deadline
    // up to `restore_job_timeout`). Running it on the async caller
    // parked the ntex worker for the entire duration. Per perf-r8
    // this was the single largest remaining wake-path target (wake
    // p50 ~9.2 s, ~5-8 s of which was this call). Hop through
    // `compio::runtime::spawn_blocking` so the worker can serve
    // other RPCs while Nomad churns. Pattern mirrors R7-P1
    // (79428d53) on snapshot's ch.pause + ch.snapshot and R5-P1b
    // (cdd2e677) on store.get.
    tracing::info!(
        sandbox_id = %sandbox_id,
        phase = "pre_submit_restore_job",
        "restore: phase"
    );
    {
        let backend_clone = Arc::clone(&backend);
        let alloc_dir_owned = alloc_dir.clone();
        let user_id_owned = snap.user_id.clone();
        let vm_index = snap.vm_index;
        compio::runtime::spawn_blocking(move || {
            backend_clone.submit_restore_job(
                sandbox_id,
                vm_index,
                &alloc_dir_owned,
                &user_id_owned,
            )
        })
        .await
        .unwrap_or_else(|p| Err(format!("spawn_blocking panic: {p:?}")))
        .map_err(RestoreHandlerError::Backend)?;
    }
    tracing::info!(
        sandbox_id = %sandbox_id,
        phase = "post_submit_restore_job",
        "restore: phase"
    );

    // 7. Wait for /livez.
    //
    // R8-A3-5 (perf-r8): `wait_for_livez` is also sync — it polls
    // the in-VM agent's `/livez` with `std::thread::sleep(250ms)`
    // between attempts until the agent answers 200 or the deadline
    // hits (typically 1-3 s post-`running`). Same blocking-poll
    // shape as `submit_restore_job`; same spawn_blocking treatment.
    tracing::info!(
        sandbox_id = %sandbox_id,
        phase = "pre_wait_for_livez",
        "restore: phase"
    );
    {
        let backend_clone = Arc::clone(&backend);
        let vm_index = snap.vm_index;
        compio::runtime::spawn_blocking(move || {
            backend_clone.wait_for_livez(sandbox_id, vm_index)
        })
        .await
        .unwrap_or_else(|p| Err(format!("spawn_blocking panic: {p:?}")))
        .map_err(RestoreHandlerError::Backend)?;
    }
    tracing::info!(
        sandbox_id = %sandbox_id,
        phase = "post_wait_for_livez",
        "restore: phase"
    );

    // 7b (B19 fix, cluster smoke 2026-05-23 r4). Install the restored
    //    VM into the backend's in-memory state map. Without this step
    //    every post-wake `exec`/`stop`/`delete` returned "sandbox not
    //    found" and the vm_index slot leaked across the controller's
    //    uptime (stop_inner's idempotent-Ok branch fired without
    //    releasing the allocator). Unseal the per-sandbox signing key
    //    from the persistence handle the controller built at startup,
    //    then hand the bytes to the backend's `register_restored`
    //    through the trait. The trait default impl is a no-op so
    //    `StubRestoreBackend`-driven unit tests don't need to
    //    implement state-map registration. A `None` persist or a
    //    `NotFound` sealed record both surface as an Internal error
    //    here — a live restored VM whose signing key is unreachable
    //    cannot be safely registered (subsequent signed-RPC traffic
    //    would fail on every call); fail loudly so the operator sees
    //    the rollback rather than a silent wedge.
    //
    // **Bug #22 fix (cluster smoke 2026-05-23 r6+B22-fixer).** After
    //    unseal, BEFORE register_restored, issue a one-shot signed
    //    `/_clock_resync` to the restored agent. CH `--restore`
    //    brings the VM back with `CLOCK_REALTIME` frozen at the
    //    snapshot-time value, so without this handshake every
    //    subsequent signed RPC fails the agent's strict 5-second
    //    skew check (surface: 401 unauthorized on every post-wake
    //    `/exec`). The resync uses the same Ed25519 signing key the
    //    agent already trusts — same `signing_key_bytes` we are
    //    about to install into the state map — and the agent's
    //    `verify_kind_skew_bypass` path accepts the call without
    //    applying the skew window. After it returns Ok the agent's
    //    wall clock matches the controller's; the state-map insert
    //    + subsequent `/exec` traffic runs under the normal strict
    //    path. We sequence resync BEFORE register_restored so an
    //    `/exec` racing with this code path either (a) precedes the
    //    state-map insert and gets the existing "sandbox not found"
    //    surface, or (b) follows both and runs on a healthy clock.
    //    There is no window where the state map says "ready" but
    //    the clock is still broken.
    if let Some(p) = persist {
        tracing::info!(
            sandbox_id = %sandbox_id,
            phase = "pre_unseal",
            "restore: phase"
        );
        let sealed = p.unseal(sandbox_id).await.map_err(|e| {
            RestoreHandlerError::Internal(format!(
                "post-wake unseal sandbox {sandbox_id}: {e}"
            ))
        })?;
        tracing::info!(
            sandbox_id = %sandbox_id,
            phase = "post_unseal",
            "restore: phase"
        );
        let agent_url = backend.derive_agent_url(snap.vm_index);
        tracing::info!(
            sandbox_id = %sandbox_id,
            phase = "pre_clock_resync",
            agent_url = %agent_url,
            "restore: phase"
        );
        clock_resync_post_restore(
            &agent_url,
            sandbox_id,
            &sealed.signing_key_bytes,
        )
        .await
        .map_err(|e| {
            RestoreHandlerError::Backend(format!(
                "post-wake clock_resync to {agent_url}: {e}"
            ))
        })?;
        tracing::info!(
            sandbox_id = %sandbox_id,
            phase = "post_clock_resync",
            "restore: phase"
        );
        backend
            .register_restored(
                sandbox_id,
                snap.vm_index,
                sealed.signing_key_bytes,
                &snap.user_id,
            )
            .map_err(RestoreHandlerError::Backend)?;
        tracing::info!(
            sandbox_id = %sandbox_id,
            phase = "post_register_restored",
            "restore: phase"
        );
    } else {
        // Test path — `restore_sandbox` was called with persist=None
        // (StubRestoreBackend driven). The trait default impl is a
        // no-op; log so prod misconfiguration doesn't slip past.
        tracing::warn!(
            sandbox_id = %sandbox_id,
            "restore: register_restored skipped — persist=None \
             (expected only in tests; production wiring at \
             AppState::from_config plumbs Some)"
        );
    }

    // 8. CAS restoring → running. We also clear the snapshot
    //    metadata: the artifact is no longer the canonical state
    //    once the VM is live again. (Per § 1: snapshot is
    //    destructive of the source; restore is destructive of the
    //    snapshot. Idle-eviction will produce a fresh snapshot.)
    tracing::info!(
        sandbox_id = %sandbox_id,
        phase = "pre_cas_running",
        "restore: phase"
    );
    let g2 = db
        .update_sandbox_status(sandbox_id, SandboxStatus::Running, expected_generation, None)
        .await?;
    tracing::info!(
        sandbox_id = %sandbox_id,
        phase = "post_cas_running",
        generation = g2,
        "restore: phase"
    );
    if let Err(e) = db.clear_snapshot_metadata(sandbox_id, g2).await {
        // Non-fatal: the row is `running` and serves traffic; the
        // operator's stale snapshot_* columns become a tidy-up
        // chore for the next idle-snapshot cycle. Log loudly.
        tracing::warn!(
            sandbox_id = %sandbox_id,
            error = %e,
            "restore: clear_snapshot_metadata after restoring→running failed (non-fatal)"
        );
    }

    Ok((snap.vm_index, g2))
}

// ────────────────────────────────────────────────────────────────────
// config.json rewrite (§ 5.1)
// ────────────────────────────────────────────────────────────────────

/// MAC derivation rule from `crates/sandbox/scripts/nomad-vm-wrapper.sh`:
/// `printf '12:34:56:78:9b:%02x' "$VM_INDEX"`. v1 vm_index is i16
/// (max 256/worker per § 5.0); we mask to one byte for the format.
pub(crate) fn derive_mac(vm_index: i16) -> String {
    format!("12:34:56:78:9b:{:02x}", (vm_index as u16) & 0xff)
}

/// Tap derivation rule: `zsbx-nm-<vm_index>`. Source-of-truth is
/// the wrapper script; this Rust copy must match.
pub(crate) fn derive_tap(vm_index: i16) -> String {
    format!("zsbx-nm-{vm_index}")
}

/// Rewrite the snapshot's `config.json` in place per § 5.1. v1 only
/// touches `net[].tap` (worker-local) and the alloc-uuid embedded
/// in `fs[].socket` and `serial.file`. IP / cmdline / disks / MAC
/// are all no-op rewrites in v1 (documented in the proposal).
///
/// Any structural surprise (missing key, wrong type) returns an
/// `Err` — the caller maps to `RestoreHandlerError::ConfigRewrite`
/// which lands as `snapshot_corrupt` so the row is marked suspect.
pub(crate) fn rewrite_config_json(
    config_path: &Path,
    vm_index: i16,
) -> Result<(), String> {
    let raw = std::fs::read_to_string(config_path)
        .map_err(|e| format!("read {}: {e}", config_path.display()))?;
    let mut v: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| format!("parse {}: {e}", config_path.display()))?;

    // 5.1 net[].tap + net[].mac: vm_index-dependent. Source and dest
    // may have different vm_indices (cluster-fallback in v2). We
    // recompute from the destination vm_index. This is the ONLY
    // path the controller rewrites — path-bearing fields (disks[].path,
    // fs[].socket, serial.file) require the runtime NOMAD_TASK_DIR
    // which the controller cannot know at job-submit time. The
    // wrapper handles path rewrites at exec time. See bug-#8
    // diagnostic 2026-05-22 / proposal § 5.
    let tap = derive_tap(vm_index);
    let mac = derive_mac(vm_index);
    if let Some(nets) = v.get_mut("net").and_then(|n| n.as_array_mut()) {
        for net in nets.iter_mut() {
            if let Some(obj) = net.as_object_mut() {
                obj.insert("tap".into(), serde_json::Value::String(tap.clone()));
                obj.insert("mac".into(), serde_json::Value::String(mac.clone()));
            }
        }
    }

    // 5.1 payload.cmdline IP rewrite — no-op in v1 (the in-VM kernel
    // doesn't re-DHCP), but include the substitution so a v2
    // refactor lands cleanly. Comment-only for now.
    let _ = (); // placeholder for v2 cmdline rewrite

    let new = serde_json::to_string_pretty(&v)
        .map_err(|e| format!("re-serialize: {e}"))?;
    std::fs::write(config_path, new)
        .map_err(|e| format!("write {}: {e}", config_path.display()))?;
    Ok(())
}

// `rewrite_alloc_path` removed 2026-05-22 (bug #8 fix). The controller
// can't fabricate a meaningful alloc UUID at job-submit time; the
// wrapper rewrites path-bearing fields at exec time using the real
// `NOMAD_TASK_DIR` env var. See `crates/sandbox/scripts/nomad-vm-wrapper.sh`
// restore branch and the diagnostic at the top of this commit.

// ────────────────────────────────────────────────────────────────────
// Test stub `RestoreBackend`.
// ────────────────────────────────────────────────────────────────────

/// Minimal stub for unit tests. Returns programmable success/error
/// from each operation; records calls so tests can assert on order.
#[doc(hidden)]
#[derive(Debug)]
pub struct StubRestoreBackend {
    pub root: PathBuf,
    pub reserved: std::sync::Mutex<Vec<i16>>,
    pub released: std::sync::Mutex<Vec<i16>>,
    pub submit_called: std::sync::atomic::AtomicBool,
    pub livez_called: std::sync::atomic::AtomicBool,
    pub teardown_called: std::sync::atomic::AtomicBool,
    pub fail_reserve: bool,
    pub fail_submit: bool,
    pub fail_livez: bool,
    /// C-4 fix: tunable retry policy. Tests that want to exercise the
    /// retry loop (e.g. "succeeds after N attempts") configure this;
    /// the default keeps `fail_reserve` tests fast by limiting to a
    /// single attempt with zero sleep.
    pub vm_index_retry_policy: VmIndexRetryPolicy,
    /// C-4 fix: when `Some(N)`, the Nth call to `reserve_vm_index`
    /// (1-indexed) flips from Err to Ok. Lets tests simulate the
    /// source-teardown finally releasing the slot after a few
    /// retries.
    pub reserve_succeeds_on_attempt: Option<u32>,
    pub reserve_attempts: std::sync::atomic::AtomicU32,
}

impl StubRestoreBackend {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            reserved: std::sync::Mutex::new(Vec::new()),
            released: std::sync::Mutex::new(Vec::new()),
            submit_called: std::sync::atomic::AtomicBool::new(false),
            livez_called: std::sync::atomic::AtomicBool::new(false),
            teardown_called: std::sync::atomic::AtomicBool::new(false),
            fail_reserve: false,
            fail_submit: false,
            fail_livez: false,
            // Default: single attempt, zero sleep — keeps existing
            // tests (notably `fail_reserve = true`) fast.
            vm_index_retry_policy: VmIndexRetryPolicy {
                max_attempts: 1,
                interval: Duration::from_millis(0),
            },
            reserve_succeeds_on_attempt: None,
            reserve_attempts: std::sync::atomic::AtomicU32::new(0),
        }
    }
}

impl RestoreBackend for StubRestoreBackend {
    fn reserve_vm_index(&self, vm_index: i16) -> Result<(), String> {
        let n = self
            .reserve_attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        // C-4 fix: "succeeds-after-N" override. Lets tests simulate
        // the source-teardown releasing the slot after a few wake
        // retries.
        if let Some(target) = self.reserve_succeeds_on_attempt {
            if n < target {
                return Err(format!(
                    "stub: reserve_vm_index({vm_index}) still held by source-teardown (attempt {n})"
                ));
            }
            self.reserved
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(vm_index);
            return Ok(());
        }
        if self.fail_reserve {
            return Err(format!("stub: reserve_vm_index({vm_index}) cluster-exhausted"));
        }
        self.reserved
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(vm_index);
        Ok(())
    }
    fn vm_index_retry_policy(&self) -> VmIndexRetryPolicy {
        self.vm_index_retry_policy
    }
    fn release_vm_index(&self, vm_index: i16) {
        self.released
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(vm_index);
    }
    fn restore_alloc_dir(&self, sandbox_id: Uuid) -> PathBuf {
        self.root.join(sandbox_id.simple().to_string())
    }
    fn submit_restore_job(
        &self,
        _sandbox_id: Uuid,
        _vm_index: i16,
        _alloc_dir: &Path,
        _user_id: &str,
    ) -> Result<(), String> {
        self.submit_called
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if self.fail_submit {
            return Err("stub: submit_restore_job failed".into());
        }
        Ok(())
    }
    fn wait_for_livez(&self, _sandbox_id: Uuid, _vm_index: i16) -> Result<(), String> {
        self.livez_called
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if self.fail_livez {
            return Err("stub: wait_for_livez failed".into());
        }
        Ok(())
    }
    fn teardown_restore(&self, _sandbox_id: Uuid, vm_index: i16) {
        self.teardown_called
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.release_vm_index(vm_index);
    }
    /// R7-S2: deterministic test-friendly URL on loopback. Encodes
    /// `vm_index` into the port so test assertions can pin the shape
    /// without resolving the host. NOT the old `127.0.0.1:0` sentinel
    /// — that was a fail-OPEN trap (resolves but nothing answers,
    /// indistinguishable from a real backend whose agent is down).
    /// `StubRestoreBackend` is used only on the `persist=None` test
    /// path where `clock_resync_post_restore` is skipped, so this URL
    /// is never dialled in current tests — but providing a real one
    /// keeps the contract honest and gives future stub-driven tests
    /// something they can actually bind to.
    fn derive_agent_url(&self, vm_index: i16) -> String {
        // Port in the IANA ephemeral-style range; offset by vm_index so
        // each stub URL is unique. Bound to 127.0.0.1 so any accidental
        // dial fails loudly with "connection refused" rather than
        // hitting an unrelated host.
        format!("http://127.0.0.1:{}", 17777u16 + (vm_index as u16))
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn derive_mac_matches_wrapper_pattern() {
        assert_eq!(derive_mac(3), "12:34:56:78:9b:03");
        assert_eq!(derive_mac(16), "12:34:56:78:9b:10");
        assert_eq!(derive_mac(255), "12:34:56:78:9b:ff");
    }

    #[test]
    fn derive_tap_matches_wrapper_pattern() {
        assert_eq!(derive_tap(0), "zsbx-nm-0");
        assert_eq!(derive_tap(42), "zsbx-nm-42");
    }

    /// Bug #8 fix (2026-05-22 cluster diagnostic): the controller
    /// rewrites ONLY net[].tap + net[].mac. Path-bearing fields
    /// (disks[].path, fs[].socket, serial.file) are left untouched
    /// because the controller can't know the runtime NOMAD_TASK_DIR
    /// at job-submit time. The wrapper does that rewrite at exec
    /// time with a `sed` against the real env-resolved task dir.
    #[test]
    fn rewrite_config_json_rewrites_only_net_fields() {
        let dir = std::env::temp_dir().join(format!(
            "zsbx-restore-test-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.json");
        // Source-shaped config with paths that must NOT be touched.
        let source_disk = "/opt/nomad/data/alloc/oldalloc/ch/local/rootfs.img";
        let source_sock = "/opt/nomad/data/alloc/oldalloc/ch/local/vfs-keys.sock";
        let source_serial = "/opt/nomad/data/alloc/oldalloc/ch/local/serial.log";
        std::fs::write(
            &cfg,
            format!(
                r#"{{
                    "net":[{{"tap":"zsbx-nm-9","mac":"12:34:56:78:9b:09"}}],
                    "fs":[{{"tag":"keys","socket":"{source_sock}"}}],
                    "disks":[{{"path":"{source_disk}"}}],
                    "serial":{{"mode":"File","file":"{source_serial}"}}
                }}"#
            ),
        )
        .unwrap();
        rewrite_config_json(&cfg, 7).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        // net.tap + net.mac rewritten from vm_index=7
        assert_eq!(v["net"][0]["tap"], "zsbx-nm-7");
        assert_eq!(v["net"][0]["mac"], "12:34:56:78:9b:07");
        // Paths LEFT UNTOUCHED — the wrapper handles them at exec.
        assert_eq!(v["fs"][0]["socket"], source_sock);
        assert_eq!(v["disks"][0]["path"], source_disk);
        assert_eq!(v["serial"]["file"], source_serial);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── C-4 fix (T-8b-smoke-r5 cluster review, 2026-05-25) ─────
    //
    // Pin the wake-vs-source-teardown race fix. The snapshot endpoint
    // returns 200 once the artifact is on disk and detaches a teardown
    // task that holds `vm_index` for ~90 s (host_fence + Nomad purge).
    // Pre-fix wake arrived ms later and immediately surfaced 503; now
    // it retries on a bounded budget. These four tests pin:
    //   - succeeds after N retries when the slot finally frees,
    //   - exhausts the budget cleanly when the slot never frees,
    //   - single-shot when the slot was free from the start (cache-warm
    //     sticky preserved),
    //   - the default policy envelopes the observed teardown profile.

    /// C-4 #1: wake retries until source slot frees.
    /// Stub backend's `reserve_vm_index` returns Err for the first 2
    /// attempts (simulating the detached teardown still holding the
    /// slot) and Ok on the 3rd. The retry loop must succeed.
    #[compio::test]
    async fn c4_wake_retries_until_source_slot_frees() {
        let root = std::env::temp_dir().join(format!(
            "zsbx-c4-retry-frees-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut stub = StubRestoreBackend::new(root.clone());
        stub.vm_index_retry_policy = VmIndexRetryPolicy {
            max_attempts: 5,
            interval: Duration::from_millis(10),
        };
        stub.reserve_succeeds_on_attempt = Some(3);
        let sid = Uuid::now_v7();
        let started = Instant::now();
        let res = reserve_vm_index_with_retry(&stub, sid, 4).await;
        let elapsed = started.elapsed();
        res.expect("wake must succeed within the retry budget");
        // Three attempts, two sleeps of 10 ms → ≥ ~20 ms; ≤ a generous
        // ceiling that absorbs scheduler jitter on busy CI.
        assert!(
            elapsed >= Duration::from_millis(15),
            "expected at least one inter-attempt sleep; elapsed={:?}",
            elapsed
        );
        assert_eq!(
            stub.reserve_attempts
                .load(std::sync::atomic::Ordering::SeqCst),
            3,
            "expected exactly 3 reserve attempts"
        );
        let reserved = stub.reserved.lock().unwrap().clone();
        assert_eq!(reserved, vec![4], "slot must be reserved on success");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// C-4 #2: wake fails cleanly with a 503-shaped error if the slot
    /// never frees within the retry budget. Pins the operator-visible
    /// surface (`VmIndexUnavailable { requested: N }`).
    #[compio::test]
    async fn c4_wake_fails_if_slot_never_frees_within_budget() {
        let root = std::env::temp_dir().join(format!(
            "zsbx-c4-exhaust-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut stub = StubRestoreBackend::new(root.clone());
        stub.fail_reserve = true; // never succeeds
        stub.vm_index_retry_policy = VmIndexRetryPolicy {
            max_attempts: 4,
            interval: Duration::from_millis(5),
        };
        let sid = Uuid::now_v7();
        let res = reserve_vm_index_with_retry(&stub, sid, 9).await;
        let err = res.expect_err("must surface 503 after exhausting budget");
        assert!(
            matches!(err, RestoreHandlerError::VmIndexUnavailable { requested: 9 }),
            "expected VmIndexUnavailable{{requested:9}}, got {err:?}"
        );
        assert_eq!(
            stub.reserve_attempts
                .load(std::sync::atomic::Ordering::SeqCst),
            4,
            "must consume the full attempt budget before surfacing 503"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// C-4 #3: wake takes a single attempt when the source slot is
    /// already free. Pins the sticky-cache-warm fast path — no
    /// unnecessary sleep, no extra reserve calls.
    #[compio::test]
    async fn c4_wake_uses_single_attempt_when_slot_free() {
        let root = std::env::temp_dir().join(format!(
            "zsbx-c4-fastpath-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut stub = StubRestoreBackend::new(root.clone());
        // Generous retry budget — if the slot is free we should NOT
        // use any of it.
        stub.vm_index_retry_policy = VmIndexRetryPolicy {
            max_attempts: 10,
            interval: Duration::from_millis(500),
        };
        let sid = Uuid::now_v7();
        let started = Instant::now();
        reserve_vm_index_with_retry(&stub, sid, 2)
            .await
            .expect("free slot must succeed");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(100),
            "free-slot fast path must not sleep; elapsed={:?}",
            elapsed
        );
        assert_eq!(
            stub.reserve_attempts
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly one reserve attempt on a free slot"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// C-7 #1 (supersedes C-4 #4): the default `VmIndexRetryPolicy`
    /// budget must sit STRICTLY BELOW the 60 s ntex/stress-client
    /// deadline. The original C-4 default (60×2 s = 120 s) exceeded
    /// the client deadline and the wake handler future was dropped
    /// by ntex on client disconnect mid-`compio::time::sleep.await`
    /// — leaving no observable success or exhausted-budget log
    /// (silent wedge at `pre_reserve_vm_index`, T-8b-smoke-r8).
    ///
    /// Concretely: total budget = `(max_attempts - 1) * interval`
    /// must be ≤55 s so the exhausted-budget warn fires with ≥5 s of
    /// headroom before the 60 s deadline closes the connection. The
    /// trade-off is documented on `VmIndexRetryPolicy` and on the
    /// C-7 review entry: if the source slot is held past 50 s by a
    /// host_fence race, wake returns a clean 503 instead of hanging
    /// silently. Observable failure mode > silent timeout.
    ///
    /// If a future refactor needs a longer retry window (e.g., to
    /// re-envelope a slower teardown profile), the correct fix is
    /// async response with polling (C-7-LT) — NOT bumping this
    /// budget back above the client deadline.
    #[test]
    fn c7_retry_budget_default_is_under_client_deadline() {
        const NTEX_CLIENT_DEADLINE_MS: u64 = 60_000;
        const REQUIRED_HEADROOM_MS: u64 = 5_000;
        let p = VmIndexRetryPolicy::default();
        let total_ms = p.interval.as_millis() as u64
            * u64::from(p.max_attempts.saturating_sub(1));
        assert!(
            total_ms + REQUIRED_HEADROOM_MS <= NTEX_CLIENT_DEADLINE_MS,
            "default retry budget must leave ≥{} ms headroom under the \
             {} ms ntex/stress-client deadline; current budget = {} ms \
             (attempts={}, interval={:?}). See C-7 in \
             docs/reviews/sandbox-snapshot-restore-deferred.md.",
            REQUIRED_HEADROOM_MS,
            NTEX_CLIENT_DEADLINE_MS,
            total_ms,
            p.max_attempts,
            p.interval
        );
    }

    /// **R14-A6 (architecture-r14)**: production backends derive
    /// `VmIndexRetryPolicy` from `cfg.host_fence_timeout_secs` via
    /// `from_host_fence_timeout`. For the platform-default fence
    /// (60 s in many deployments), the derived budget must (a) stay
    /// strictly below the 60 s ntex/stress-client deadline and (b)
    /// envelope the host_fence so a wake racing a fence-clear has a
    /// non-trivial chance of catching the release. 60 s fence →
    /// 26×2 s = 50 s wall-time fits both constraints.
    #[test]
    fn r14a6_policy_from_cfg_respects_host_fence_timeout() {
        // 60 s host-fence (the platform default after the cad098e6
        // 30→120 bump backed off to 60 in many configs).
        let p = VmIndexRetryPolicy::from_host_fence_timeout(
            60,
            crate::config::WakeResponseMode::Sync,
        );
        let wall_ms = p.interval.as_millis() as u64
            * u64::from(p.max_attempts.saturating_sub(1));
        assert!(
            wall_ms <= 50_000,
            "60 s fence → derived budget should be ≤50 s; got {} ms \
             (attempts={}, interval={:?})",
            wall_ms,
            p.max_attempts,
            p.interval
        );
        // And not trivially small — must actually exercise the retry
        // loop past the first reserve attempt.
        assert!(
            p.max_attempts > 1,
            "60 s fence → derived policy must allow >1 attempt; got {}",
            p.max_attempts
        );
        // 2 s interval is the C-7 cadence; from_host_fence_timeout
        // anchors to it so the loop semantics match the existing
        // observability + tests.
        assert_eq!(
            p.interval,
            Duration::from_secs(2),
            "from_host_fence_timeout must use the C-7 2 s interval"
        );
    }

    /// **R14-A6 short-timeout case (post-C-8b)**: a 20 s host_fence
    /// (an aggressive per-cluster override) should yield a sensible
    /// non-trivial budget. Post-C-8b the fence-derived ceiling uses
    /// `teardown_estimate = 2 * fence = 40 s`, so the formula is
    /// `(40 - 10) / 2 + 1 = 16` attempts × 2 s = 30 s wall-time
    /// (fence-ceil binds; deadline-ceil 50 s is looser here). Pins
    /// the formula so a future refactor can't silently collapse the
    /// policy to `max_attempts = 1` for short fences. **Pre-C-8b
    /// this was 6 attempts / 10 s — the 1× fence assumption that
    /// smoke-r10 disproved.**
    #[test]
    fn r14a6_policy_from_cfg_short_timeout() {
        let p = VmIndexRetryPolicy::from_host_fence_timeout(
            20,
            crate::config::WakeResponseMode::Sync,
        );
        assert_eq!(
            p.max_attempts, 16,
            "20 s fence post-C-8b sync: (2*20 - 10 headroom) / 2 s interval + 1 = 16"
        );
        assert_eq!(p.interval, Duration::from_secs(2));
    }

    /// **R14-A6 zero-fence edge case**: `host_fence_timeout_secs == 0`
    /// is the explicit "disable the fence" knob (NOT recommended in
    /// production but valid for some test setups). The derived policy
    /// must still produce at least one decisive reserve attempt
    /// rather than collapsing to a degenerate 0-attempt loop that
    /// would skip the reserve entirely.
    #[test]
    fn r14a6_policy_from_cfg_zero_fence_still_attempts_once() {
        let p = VmIndexRetryPolicy::from_host_fence_timeout(
            0,
            crate::config::WakeResponseMode::Sync,
        );
        assert!(
            p.max_attempts >= 1,
            "zero fence must still attempt the reserve at least once; got {}",
            p.max_attempts
        );
    }

    /// **C-8a (T-8b-smoke-r9 cluster review)**: the deadline-cap MUST
    /// override a conservative `host_fence_timeout_secs`. The pre-C-8a
    /// derivation took only the fence into account, so a production
    /// fence of 120 s produced a 110 s budget — 50 s past the 60 s ntex
    /// client deadline, re-introducing C-7-class silent cancellation.
    ///
    /// This test pins the new behaviour: for any fence ≥ 60 s the
    /// budget caps at `CLIENT_DEADLINE - CLIENT_HEADROOM = 50 s`,
    /// guaranteeing the exhausted-budget log fires before ntex drops
    /// the future on client disconnect.
    #[test]
    fn r14a6_from_cfg_caps_at_client_deadline() {
        let p = VmIndexRetryPolicy::from_host_fence_timeout(
            120,
            crate::config::WakeResponseMode::Sync,
        );
        let wall_ms = p.interval.as_millis() as u64
            * u64::from(p.max_attempts.saturating_sub(1));
        assert!(
            wall_ms <= 50_000,
            "120 s fence MUST cap at CLIENT_DEADLINE - HEADROOM = 50 s; \
             got {} ms (attempts={}, interval={:?}). C-8a regression — \
             see docs/reviews/sandbox-snapshot-restore-deferred.md.",
            wall_ms,
            p.max_attempts,
            p.interval
        );
        // The cap is the HARD ceiling — exactly 26×2=52 ms… no, 26
        // attempts with 25 sleeps × 2 s = 50 s wall-time. Pin the
        // attempt count so a future regression that loosens the cap
        // (e.g. lifts the headroom to 5 s) fails loudly here.
        assert_eq!(
            p.max_attempts, 26,
            "120 s fence with deadline-cap: (60 - 10) / 2 + 1 = 26 attempts; got {}",
            p.max_attempts
        );
        assert_eq!(p.interval, Duration::from_secs(2));
    }

    /// **C-8b (T-8b-smoke-r10 cluster review)**: the fence-derived
    /// ceiling must envelope the *full* source-teardown wall-time,
    /// which smoke-r10 empirically measured at **2× the fence** (60.164 s
    /// at `host_fence=30 s`). Pre-C-8b the policy at fence=30 was 11
    /// attempts / 20 s — exhausted ~40 s before the actual vm_index
    /// release. Post-C-8b the same input yields 26 attempts / 50 s
    /// (deadline-ceil binds since fence-ceil would be 50 s too).
    ///
    /// Pins: at fence=30, max_attempts ≥ 21 (≥40 s wall-time budget),
    /// strictly more than the pre-C-8b 11. Catches a future regression
    /// that drops the 2× factor back to 1×.
    ///
    /// See `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-smoke-r10.md`
    /// for the smoke trace establishing the 2× ratio.
    #[test]
    fn c8b_default_policy_envelopes_doubled_fence() {
        // 30 s is the cluster-smoke fence (C-8 cluster config).
        // Sync mode: the C-8b/C-8a contract holds under the
        // dual-ceiling MIN (post-C-7-LT-1 the test names the mode
        // explicitly).
        let p = VmIndexRetryPolicy::from_host_fence_timeout(
            30,
            crate::config::WakeResponseMode::Sync,
        );
        let wall_ms = p.interval.as_millis() as u64
            * u64::from(p.max_attempts.saturating_sub(1));
        assert!(
            p.max_attempts >= 21,
            "C-8b: 30 s fence post-fix must yield ≥21 attempts (≥40 s budget) \
             to envelope the 2× teardown wall-time; got {} attempts. \
             Pre-C-8b this was 11 attempts / 20 s and silently 503-d \
             while teardown was still 40 s away from completing.",
            p.max_attempts,
        );
        // And the budget MUST still fit under the 60 s ntex deadline
        // (C-8a invariant — silent-cancel regression remains
        // structurally prevented by the MIN-of-two design).
        assert!(
            wall_ms <= 50_000,
            "C-8b must NOT regress C-8a: budget must remain ≤50 s under \
             the 60 s ntex deadline; got {} ms (attempts={})",
            wall_ms,
            p.max_attempts,
        );
        // Pin the exact attempt count at the cluster-smoke fence so a
        // future refactor that subtly changes the formula (e.g. wrong
        // headroom or wrong factor) fails loudly here. Math:
        //   teardown_est = 2 * 30 = 60
        //   fence_ceil   = 60 - 10 = 50
        //   deadline_ceil = 60 - 10 = 50
        //   MIN(50, 50) = 50, /2 + 1 = 26
        assert_eq!(
            p.max_attempts, 26,
            "C-8b: 30 s fence → (2*30 - 10) / 2 + 1 = 26 attempts; got {}",
            p.max_attempts,
        );
    }

    /// **C-7-LT-1 (T-8b-smoke-r12 cluster review)**: under
    /// `WakeResponseMode::Async`, the ntex client deadline no longer
    /// binds the wake retry loop (it runs on `detach_isolated` with
    /// no client-side cancellation). The budget therefore drops the
    /// MIN-with-deadline ceiling and uses `2×fence + HEADROOM` as a
    /// safety margin past the empirical source-teardown wall-time.
    ///
    /// Smoke-r12 measured 60.166 s teardown at fence=30 s; the sync
    /// 50 s cap surfaced as `vm_index_unavailable` 10 s before
    /// teardown completed. Async at fence=30 = 2*30 + 10 = 70 s
    /// budget → 36 attempts × 2 s = 70 s, enveloping the wall-time
    /// with ~10 s slack.
    #[test]
    fn c7_lt_1_async_mode_fence_30_yields_70s_budget() {
        let p = VmIndexRetryPolicy::from_host_fence_timeout(
            30,
            crate::config::WakeResponseMode::Async,
        );
        // 2*30 + 10 = 70 s; /2 + 1 = 36 attempts (35 sleeps × 2 s = 70 s).
        assert_eq!(
            p.max_attempts, 36,
            "C-7-LT-1: async fence=30 must yield 36 attempts \
             (2*30 + 10 = 70 s budget; 70/2 + 1 = 36); got {}",
            p.max_attempts,
        );
        assert_eq!(p.interval, Duration::from_secs(2));
        let wall_ms = p.interval.as_millis() as u64
            * u64::from(p.max_attempts.saturating_sub(1));
        // The smoke-r12 empirical teardown was 60.166 s — async budget
        // MUST envelope that.
        assert!(
            wall_ms >= 60_166,
            "C-7-LT-1 async fence=30 budget must envelope the 60.166 s \
             smoke-r12 teardown wall-time; got {} ms",
            wall_ms,
        );
    }

    /// **C-7-LT-1 sync-mode regression pin**: at the same fence=30,
    /// sync mode must still cap at 26 attempts / 50 s — the C-8b
    /// contract is unchanged by the C-7-LT-1 split. Without this
    /// pin a future refactor could accidentally collapse the two
    /// branches and silently regress C-8a (sync silent-cancel).
    #[test]
    fn c7_lt_1_sync_mode_fence_30_preserves_c8b_budget() {
        let p = VmIndexRetryPolicy::from_host_fence_timeout(
            30,
            crate::config::WakeResponseMode::Sync,
        );
        // C-8b dual-ceiling MIN: MIN(2*30 - 10, 60 - 10) = MIN(50, 50)
        // = 50 s → 26 attempts.
        assert_eq!(
            p.max_attempts, 26,
            "C-7-LT-1 sync fence=30 must preserve C-8b: 26 attempts; got {}",
            p.max_attempts,
        );
        let wall_ms = p.interval.as_millis() as u64
            * u64::from(p.max_attempts.saturating_sub(1));
        assert!(
            wall_ms <= 50_000,
            "C-7-LT-1 sync MUST NOT regress C-8a: budget must remain \
             ≤50 s under the 60 s ntex deadline; got {} ms",
            wall_ms,
        );
    }

    /// **C-7-LT-1 long-fence async case**: under async mode a large
    /// fence value (e.g. operator-conservative fence=120 s) should
    /// scale linearly with `2*fence + HEADROOM`. Sync mode would
    /// clamp this at the 50 s deadline ceiling; async drops the
    /// clamp entirely.
    ///
    /// Pin: fence=120 async → 2*120 + 10 = 250 s → 126 attempts.
    /// Sync mode at the same fence still caps at 26 attempts / 50 s
    /// (separately covered by `r14a6_from_cfg_caps_at_client_deadline`).
    #[test]
    fn c7_lt_1_async_mode_fence_120_unbinds_deadline() {
        let p_async = VmIndexRetryPolicy::from_host_fence_timeout(
            120,
            crate::config::WakeResponseMode::Async,
        );
        // 2*120 + 10 = 250; /2 + 1 = 126 attempts.
        assert_eq!(
            p_async.max_attempts, 126,
            "C-7-LT-1: async fence=120 must yield 126 attempts \
             (2*120 + 10 = 250 s budget); got {}",
            p_async.max_attempts,
        );

        // And cross-check the sync companion is STILL clamped at 26
        // (the deadline-ceil wins in sync). This pins the MODE split
        // — a regression collapsing async back to sync would surface
        // here as `p_async.max_attempts == p_sync.max_attempts`.
        let p_sync = VmIndexRetryPolicy::from_host_fence_timeout(
            120,
            crate::config::WakeResponseMode::Sync,
        );
        assert_eq!(p_sync.max_attempts, 26);
        assert!(
            p_async.max_attempts > p_sync.max_attempts,
            "C-7-LT-1: at fence=120 the async budget MUST exceed sync \
             (sync clamped by deadline-ceil; async unbound). Got \
             async={}, sync={}",
            p_async.max_attempts,
            p_sync.max_attempts,
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// `RealRestoreBackend` — production impl mirroring nomad_ch's
// create-side path (submit job, poll alloc-running, poll livez)
// with two key differences:
//
//   1. ZSBX_RESTORE_FROM=<alloc_dir> set in the spawned task's env
//      so the wrapper's PR 3f branch invokes
//      `cloud-hypervisor --restore source_url=file://<alloc_dir>`.
//   2. The vm_index is forced to the source slot (from the snapshot
//      row); cluster-fallback is documented but not implemented in
//      v1. A reserve() collision surfaces as 503
//      `vm_index_unavailable`.
//
// The trait's methods are sync. Each method uses the blocking ureq
// client directly; the handler invokes them from an async task and
// will block its compio worker for the duration. This is acceptable
// because (a) restore is a one-shot, infrequent op (not on the hot
// path) and (b) the wall time is dominated by CH boot + agent
// readiness — same shape as the existing nomad_ch create flow that
// already runs blocking-ish in spawn_blocking.
// ────────────────────────────────────────────────────────────────────

use std::collections::BTreeSet;

/// Minimal vm_index tracker used by the restore backend. Independent
/// of `NomadCHBackend::vm_index_allocator` because the restore path
/// runs ahead of any controller-managed registry — the in-memory
/// state for the restored alloc lives only in the restore-flow
/// scratch (alloc dir + Nomad job).  Production wiring shares the
/// allocator with `NomadCHBackend` via `Arc<Mutex<_>>` so a v2
/// cross-backend create cannot collide with an in-flight restore.
#[derive(Debug, Default)]
pub struct VmIndexReservations {
    reserved: BTreeSet<i16>,
}

impl VmIndexReservations {
    pub fn new() -> Self {
        Self {
            reserved: BTreeSet::new(),
        }
    }

    /// Returns Err if `vm_index` is already reserved.
    pub fn reserve(&mut self, vm_index: i16) -> Result<(), String> {
        if !self.reserved.insert(vm_index) {
            return Err(format!("vm_index {vm_index} already reserved"));
        }
        Ok(())
    }

    pub fn release(&mut self, vm_index: i16) {
        self.reserved.remove(&vm_index);
    }
}

/// Production restore backend. Submits a Nomad job that mirrors the
/// shape of `NomadCHBackend::create`'s, with `ZSBX_RESTORE_FROM` set.
pub struct RealRestoreBackend {
    cfg: NomadCHConfig,
    /// Controller-wide memory_mb default — written into the restore
    /// alloc's `ZSBX_VM_MEMORY_MB` env (Phase B fix #6). The wrapper
    /// passes this through to CH's `--memory size=${N}M,shared=on`
    /// flag; for restore it must match the snapshot's memory size
    /// (CH refuses to restore against a size mismatch).
    memory_mb: u32,
    /// Controller-wide cpu count — for `ZSBX_VM_CPUS_BOOT`. Same
    /// match-the-snapshot constraint applies.
    cpus: f32,
    /// Wall-time budget for the spawned alloc to reach
    /// `ClientStatus="running"` (mirrors `cfg.alloc_running_timeout_secs`).
    alloc_running_timeout: Duration,
    /// Wall-time budget for `/livez` to return 200 once the alloc is
    /// running. Mirrors `cfg.agent_livez_timeout_secs`. v1 polls
    /// unsigned /livez only — the signed /version fingerprint check
    /// requires plumbing the per-sandbox signing key out of the
    /// sealed record, which is a follow-up PR.
    agent_livez_timeout: Duration,
    /// **B18 fix (cluster smoke 2026-05-24 r4)**: shared
    /// `vm_index` allocator with `NomadCHBackend`. When present,
    /// `reserve_vm_index` and `release_vm_index` go through this
    /// allocator instead of the local `VmIndexReservations` below —
    /// so create-side `alloc()` cannot hand the same slot to a fresh
    /// sandbox while the restored VM is still alive on that
    /// tap/IP. Wired by `crate::AppState::from_config` from
    /// `Backend::vm_index_allocator()`. `None` only for unit tests
    /// that don't construct a `NomadCHBackend`.
    shared_allocator: Option<Arc<Mutex<crate::backend::nomad_ch::VmIndexAllocator>>>,
    /// Process-local reservation map for the source vm_index. Used
    /// only when `shared_allocator` is `None` (unit tests). In
    /// production this is bypassed entirely — `RealRestoreBackend::
    /// with_shared_allocator` wires through to the create-side pool.
    reservations: Arc<Mutex<VmIndexReservations>>,
    /// **B19 fix (cluster smoke 2026-05-23 r4 post-fix)**: shared
    /// `Arc<NomadCHBackend>` handle. When present, the trait's
    /// `register_restored` call after `wait_for_livez` Ok delegates
    /// to `NomadCHBackend::register_restored(...)`, which inserts
    /// the per-sandbox record into the backend's state map. Without
    /// the share, the restored VM stays invisible to the registry
    /// and every post-wake `exec`/`stop`/`delete` hits the "sandbox
    /// not found" branch + leaks the vm_index slot across controller
    /// uptime. Wired by `crate::AppState::from_config` from
    /// `Backend::nomad_ch_handle()`. `None` only for unit tests
    /// that don't construct a `NomadCHBackend` (the default trait
    /// impl returns `Ok(())` for those).
    nomad_handle: Option<Arc<crate::backend::nomad_ch::NomadCHBackend>>,
    /// **C-7-LT-1 (T-8b-smoke-r12)**: wake response contract mode
    /// (Sync vs. Async). Threaded into
    /// `VmIndexRetryPolicy::from_host_fence_timeout` so the retry
    /// budget can drop the ntex-client-deadline cap under async —
    /// where the wake loop runs on `detach_isolated` with no
    /// client-side cancellation. Defaults to `Sync` so existing
    /// unit-test constructors keep the pre-C-7-LT-1 budget shape.
    /// Production wiring (`AppState::from_config`) sets this via
    /// [`Self::with_wake_response_mode`] from
    /// `WakeResponseMode::from_env()`.
    wake_response_mode: crate::config::WakeResponseMode,
}

impl std::fmt::Debug for RealRestoreBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealRestoreBackend")
            .field("nomad_addr", &self.cfg.nomad_addr)
            .field("datacenter", &self.cfg.datacenter)
            .field("alloc_running_timeout", &self.alloc_running_timeout)
            .field("agent_livez_timeout", &self.agent_livez_timeout)
            .finish_non_exhaustive()
    }
}

impl RealRestoreBackend {
    pub fn new(cfg: NomadCHConfig, memory_mb: u32, cpus: f32) -> Self {
        let alloc_running_timeout =
            Duration::from_secs(cfg.alloc_running_timeout_secs);
        let agent_livez_timeout =
            Duration::from_secs(cfg.agent_livez_timeout_secs);
        Self {
            cfg,
            memory_mb,
            cpus,
            alloc_running_timeout,
            agent_livez_timeout,
            shared_allocator: None,
            reservations: Arc::new(Mutex::new(VmIndexReservations::new())),
            nomad_handle: None,
            // C-7-LT-1 default: Sync preserves the pre-r12 budget
            // shape for unit tests + back-compat fixture constructors
            // that don't go through `AppState::from_config`. Production
            // wiring sets this via `with_wake_response_mode` from
            // `WakeResponseMode::from_env()`.
            wake_response_mode: crate::config::WakeResponseMode::Sync,
        }
    }

    /// **C-7-LT-1 fix (T-8b-smoke-r12)**: install the wake response
    /// contract mode. Threaded into
    /// `VmIndexRetryPolicy::from_host_fence_timeout` so the retry
    /// budget drops the ntex-client-deadline cap in async mode (where
    /// the wake loop runs on `detach_isolated` with no client-side
    /// cancellation). Pre-C-7-LT-1, the policy capped at 50 s under
    /// async too — racing the empirical 60.166 s source-teardown
    /// wall-time and surfacing as `vm_index_unavailable` even though
    /// the budget was governed by a deadline that no longer applied.
    pub(crate) fn with_wake_response_mode(
        mut self,
        mode: crate::config::WakeResponseMode,
    ) -> Self {
        self.wake_response_mode = mode;
        self
    }

    /// **B18 fix**: install a shared `vm_index` allocator. The
    /// production wiring (`AppState::from_config`) extracts the
    /// allocator from `Backend::vm_index_allocator()` and passes it
    /// here so the restore path's `reserve(source_slot)` and the
    /// create path's `alloc()` share state. Without this share, a
    /// CREATE after WAKE can hand the same tap/IP to a fresh sandbox,
    /// surfacing as a stale-pubkey 401 on /version (cluster smoke
    /// 2026-05-24 r4; 11/16 c=4 cycles failed once slots 1-6 had
    /// been used once).
    pub(crate) fn with_shared_allocator(
        mut self,
        allocator: Arc<Mutex<crate::backend::nomad_ch::VmIndexAllocator>>,
    ) -> Self {
        self.shared_allocator = Some(allocator);
        self
    }

    /// **B19 fix**: install a shared `Arc<NomadCHBackend>` handle so
    /// `register_restored(...)` can route the post-wake state-map
    /// insert back into the backend. Without this, the restored VM
    /// stays invisible to the registry — every post-wake
    /// `exec`/`stop`/`delete` returns "sandbox not found" and the
    /// vm_index slot leaks (cluster smoke 2026-05-23: ~10 wakes
    /// per controller boot exhausted the allocator). Wired by
    /// `crate::AppState::from_config` from
    /// `Backend::nomad_ch_handle()`.
    pub(crate) fn with_nomad_handle(
        mut self,
        handle: Arc<crate::backend::nomad_ch::NomadCHBackend>,
    ) -> Self {
        self.nomad_handle = Some(handle);
        self
    }
}

impl RestoreBackend for RealRestoreBackend {
    fn reserve_vm_index(&self, vm_index: i16) -> Result<(), String> {
        if let Some(shared) = self.shared_allocator.as_ref() {
            // B18 fix: route through the create-side allocator so a
            // concurrent CREATE on the same worker cannot hand the
            // same tap/IP to a fresh sandbox while a restored VM is
            // still alive on this slot. i16 → u16 cast: vm_index is
            // validated [1..=255] on the producer side (proposal §
            // 5.0; allocator's reserve() rejects out-of-range with
            // a clear error).
            let i = u16::try_from(vm_index).map_err(|_| {
                format!("reserve_vm_index: vm_index {vm_index} out of u16 range")
            })?;
            return shared
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .reserve(i);
        }
        self.reservations
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .reserve(vm_index)
    }

    fn release_vm_index(&self, vm_index: i16) {
        if let Some(shared) = self.shared_allocator.as_ref() {
            // B18 fix: release back into the shared pool so the
            // create-side `alloc()` can immediately reuse the slot
            // once a restored VM has been torn down (or the wake
            // path rolled back via `teardown_restore`).
            if let Ok(i) = u16::try_from(vm_index) {
                shared
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .release(i);
            }
            return;
        }
        self.reservations
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .release(vm_index);
    }

    fn restore_alloc_dir(&self, sandbox_id: Uuid) -> PathBuf {
        // Stable across attempts so ZSBX_RESTORE_FROM is deterministic.
        // <host_state_dir>/<sandbox-id>/restore/
        self.cfg
            .host_state_dir
            .join(sandbox_id.simple().to_string())
            .join("restore")
    }

    fn submit_restore_job(
        &self,
        sandbox_id: Uuid,
        vm_index: i16,
        alloc_dir: &Path,
        user_id: &str,
    ) -> Result<(), String> {
        let job_id = format!("zsbx-restore-{}", sandbox_id.simple());
        // R12-I1 (T-8 blocker): the wake path now respects the same
        // SANDBOX_TASK_DRIVER feature flag T-7 wired into the cold-boot
        // builder. Without this, CREATEs under T-8 cutover use the Go
        // plugin (`Driver: "ch"`) but RESTOREs would still hand Nomad
        // a `Driver: "raw_exec"` jobspec — once the bash wrapper is
        // removed in T-8b-cutover, every wake fails. Reading the env
        // once here mirrors `build_nomad_job_json`'s call site.
        let mode = crate::backend::nomad_ch::task_driver_mode_from_env();

        // T-8b-stress Bug 1 fix: before submitting the restore job,
        // assert that the disk images the driver's preflight will
        // stat are actually on disk. The wake path expects
        // `stop_preserving_state` (snapshot's source teardown) to
        // have left `workspace.img` in place, and the per-user
        // `home.img` to have been mkfs'd by the source sandbox's
        // create() — but neither is re-asserted at the wake's submit
        // site. A missing image surfaces at the driver as a generic
        // "Failed tasks" alloc rollup; surfacing it HERE turns it
        // into a clean controller-side error with the offending path
        // string, preserving the observability symmetry with the
        // cold-boot path's post-stage assertion in
        // `create_ext4_image_if_missing`. Mirrors the driver's
        // `preflightDiskPaths` discipline (3-strike pattern: user_id,
        // rootfs_source, now workspace.img + home.img).
        let host_dir = self
            .cfg
            .host_state_dir
            .join(sandbox_id.simple().to_string());
        let workspace_img = host_dir.join("workspace.img");
        let user_home_img = self
            .cfg
            .user_home_dir_root
            .join(user_id)
            .join("home.img");
        crate::backend::nomad_ch::assert_disk_image_present(&workspace_img).map_err(|e| {
            format!(
                "restore submit: workspace.img missing for sandbox {} \
                 (snapshot teardown should have preserved it via \
                 stop_preserving_state; controller will not submit \
                 restore job that the driver's preflight would reject \
                 with a generic Failed-tasks rollup): {e}",
                sandbox_id
            )
        })?;
        crate::backend::nomad_ch::assert_disk_image_present(&user_home_img).map_err(|e| {
            format!(
                "restore submit: user_home.img missing for sandbox {} \
                 user {} (per-user image should persist across the user's \
                 sandboxes — source create() mkfs'd it; only host disk \
                 corruption or out-of-band rm would explain this): {e}",
                sandbox_id, user_id
            )
        })?;

        let job_json = build_restore_nomad_job_json(
            &job_id,
            &self.cfg,
            vm_index as u16,
            alloc_dir,
            sandbox_id,
            user_id,
            self.memory_mb,
            self.cpus,
            mode,
        );
        let body = serde_json::to_vec(&job_json)
            .map_err(|e| format!("serialize Nomad job JSON: {e}"))?;
        let url = format!("{}/v1/jobs", self.cfg.nomad_addr);
        let resp = nomad_post_blocking(&url, &body, Duration::from_secs(15))?;
        if resp.status != 200 {
            return Err(format!(
                "POST {url} → status {}: {}",
                resp.status,
                resp.body.trim()
            ));
        }
        // Now poll until the alloc reaches running (or terminal).
        wait_for_alloc_running_blocking(
            &self.cfg.nomad_addr,
            &job_id,
            self.alloc_running_timeout,
        )
    }

    fn wait_for_livez(
        &self,
        _sandbox_id: Uuid,
        vm_index: i16,
    ) -> Result<(), String> {
        let agent_url = self.derive_agent_url(vm_index);
        wait_for_livez_blocking(&agent_url, self.agent_livez_timeout)
    }

    /// **R14-A6 (architecture-r14)**: derive the wake-retry budget
    /// from `cfg.host_fence_timeout_secs` instead of inheriting the
    /// hard-coded `VmIndexRetryPolicy::default()`. The source vm_index
    /// is released only after the host-fence clears, so the wake
    /// budget is fundamentally a function of the fence timeout — see
    /// [`VmIndexRetryPolicy::from_host_fence_timeout`] for the formula
    /// + the trade-off if an operator sets `host_fence_timeout_secs`
    /// past the ntex client deadline.
    ///
    /// **C-7-LT-1 (T-8b-smoke-r12)**: the wake response mode is
    /// threaded in so async-mode retries are NOT capped at the
    /// (now-vestigial) 60 s ntex client deadline — the async wake
    /// loop runs on `detach_isolated` with no client-side cancellation.
    fn vm_index_retry_policy(&self) -> VmIndexRetryPolicy {
        VmIndexRetryPolicy::from_host_fence_timeout(
            self.cfg.host_fence_timeout_secs,
            self.wake_response_mode,
        )
    }

    fn derive_agent_url(&self, vm_index: i16) -> String {
        // Same formula `NomadCHBackend::derive_agent_url` uses
        // (lines ~1521 of nomad_ch.rs); centralised here so the
        // RealRestoreBackend doesn't need to thread an Arc<NomadCH>
        // just to compute the URL.
        format!(
            "http://10.{}.{}.2:7777",
            self.cfg.subnet_second_octet,
            100u16 + (vm_index as u16)
        )
    }

    fn teardown_restore(&self, sandbox_id: Uuid, vm_index: i16) {
        let job_id = format!("zsbx-restore-{}", sandbox_id.simple());
        // Best-effort DELETE; ignore errors. The orphan-prune sweep
        // will mop up if Nomad is unreachable right now.
        let url = format!("{}/v1/job/{}?purge=true", self.cfg.nomad_addr, job_id);
        if let Err(e) = nomad_delete_blocking(&url, Duration::from_secs(10)) {
            tracing::warn!(
                sandbox_id = %sandbox_id,
                vm_index,
                error = %e,
                "restore teardown: nomad DELETE failed (non-fatal)"
            );
        }
        // **R10-C1 fix (concurrency-r10 2026-05-25)**: drop the
        // state-map entry that `register_restored` may have inserted.
        // Pre-fix this was missed: if `register_restored` had run
        // (success branch) and then a LATER step failed (e.g. the
        // final `update_sandbox_status(Running)` returned CasLost,
        // legitimate under a parallel admin stop), the rollback path
        // released the vm_index BUT left the state-map entry behind.
        // The next `create()` would then reserve the same vm_index
        // (now free in the allocator) and end up with TWO state-map
        // entries pointing at the same slot — and the older entry's
        // eventual `stop_inner` would release the live tenant's slot.
        //
        // Mirroring `stop_inner`'s `state.write().remove(&sandbox_id)`,
        // the call is idempotent: no-op if `register_restored` never
        // ran on this `sandbox_id` (early-rollback path). The
        // structural cure is R4-A2's `LeasedVmSlot` RAII; this 1-line
        // interim is the cheap insurance until that lands.
        if let Some(handle) = self.nomad_handle.as_ref() {
            let removed = handle.unregister_restored(sandbox_id);
            if removed {
                tracing::info!(
                    sandbox_id = %sandbox_id,
                    vm_index,
                    "restore teardown: removed nomad-ch state-map entry \
                     (R10-C1 rollback path)"
                );
            }
        }
        // Always release the vm_index regardless of teardown outcome.
        self.release_vm_index(vm_index);
    }

    /// **B19 fix**: install the restored sandbox into
    /// `NomadCHBackend::state` via the shared handle. The handler
    /// has already unsealed the signing key from `Persistence` and
    /// passes it in. The agent_url is re-derived here from
    /// `vm_index` + the backend's `subnet_second_octet` — same
    /// shape as `wait_for_livez`. Returns Err if the handle is
    /// `None` (production wiring should always set it; if it's
    /// missing we surface the misconfiguration loudly rather than
    /// silently no-op'ing — a silent no-op would re-introduce the
    /// pre-B19 "sandbox not found" + slot-leak symptoms).
    fn register_restored(
        &self,
        sandbox_id: Uuid,
        vm_index: i16,
        signing_key_bytes: [u8; 32],
        user_id: &str,
    ) -> Result<(), String> {
        let handle = self.nomad_handle.as_ref().ok_or_else(|| {
            "register_restored: RealRestoreBackend has no shared NomadCHBackend \
             handle (B19 wiring missing — check AppState::from_config)".to_string()
        })?;
        let vm_index_u16 = u16::try_from(vm_index).map_err(|_| {
            format!("register_restored: vm_index {vm_index} out of u16 range")
        })?;
        // Same agent_url formula as `wait_for_livez` above. Keeps the
        // shape that `wait_for_agent_livez` and `derive_agent_url`
        // use — `http://10.<subnet_second_octet>.<100+idx>.2:7777`.
        let agent_url = format!(
            "http://10.{}.{}.2:7777",
            self.cfg.subnet_second_octet,
            100u16 + vm_index_u16
        );
        handle.register_restored(
            sandbox_id,
            vm_index_u16,
            signing_key_bytes,
            agent_url,
            user_id.to_string(),
        )
    }
}

/// Build the Nomad job JSON for a restore alloc. Same shape as
/// `build_nomad_job_json` in nomad_ch but with the restore branch
/// active. We don't share the helper because the restore path doesn't
/// have a `user_id`/`project_id` to plumb through Meta — those are
/// already recorded on the source sandbox row in pg, the wrapper
/// doesn't need them.
///
/// R12-I1 (T-8 blocker): the `mode` arg mirrors the
/// `build_nomad_job_json_with` T-7 added to the cold-boot path.
///
/// - `RawExec` (default): `Driver: "raw_exec"`, `Config: { command:
///   <wrapper> }`, restore-specific env (`ZSBX_RESTORE_FROM=<alloc_dir>`)
///   ridges the bash wrapper into the restore branch at
///   nomad-vm-wrapper.sh:364.
/// - `ChPlugin`: `Driver: "ch"` (the Go plugin's declared name) +
///   typed `Config` matching `nomad-driver-ch/ch/task_config.go::TaskConfig`,
///   with `restore_from = <alloc_dir>` triggering the driver's
///   `cloud-hypervisor --restore source_url=…` branch. The Env block
///   stays populated under both modes (largely redundant under ChPlugin
///   but kept for debugging parity).
#[allow(clippy::too_many_arguments)]
fn build_restore_nomad_job_json(
    job_id: &str,
    cfg: &NomadCHConfig,
    vm_index: u16,
    alloc_dir: &Path,
    sandbox_id: Uuid,
    user_id: &str,
    memory_mb: u32,
    cpus: f32,
    mode: crate::backend::nomad_ch::TaskDriverMode,
) -> serde_json::Value {
    // Phase B fix #6 (later: virtio-blk pivot, bug #11): the wrapper
    // up-front validates a 5-env block (VM_INDEX + ARTIFACT_DIR +
    // RUNTIME + MEMORY_MB + CPUS_BOOT) on both branches, and on
    // cold-boot it additionally validates WORKSPACE_IMG +
    // USER_HOME_IMG + PUBKEY_HEX. Restore doesn't *need* the latter
    // three (CH `--restore` reads the disk paths + cmdline from the
    // snapshot's saved config.json), but we still emit the image
    // paths because the controller derives them deterministically
    // and the wrapper's defensive `[ -f $PATH ]` check on the
    // images catches a hand-edited restore jobspec with a typo'd
    // path before CH's "block device file" error.
    //
    // PUBKEY_HEX is left empty on restore: it's hex-only validated
    // only when set, and we have no separate persisted hex form on
    // hand here (the signing key lives in the persist layer as raw
    // 32 bytes; the restore path doesn't need to recompute the hex
    // because CH never reads the cmdline on restore).
    let host_dir = cfg
        .host_state_dir
        .join(sandbox_id.simple().to_string());
    let workspace_img = host_dir.join("workspace.img");
    let user_home_img = cfg
        .user_home_dir_root
        .join(user_id)
        .join("home.img");
    // cpus_boot: ceil(cpus) with a min of 1; mirrors nomad_ch::cpus_boot.
    let cpus_boot = if !cpus.is_finite() {
        1u32
    } else {
        let n = cpus.ceil() as i64;
        if n < 1 { 1 } else { n as u32 }
    };

    // The per-VM Env block. Populated under BOTH driver modes — under
    // ChPlugin it's largely redundant with the typed Config but the
    // Go driver ignores Env, so leaving it for debugging + symmetry
    // with the cold-boot builder's invariant. The wrapper reads these
    // when raw_exec is the active driver.
    //
    // virtio-blk pivot (bug #11): the three virtio-fs share dirs are
    // gone from the cold-boot env contract; mirror that here by
    // emitting the two image paths. The controller derives both paths
    // the same way cold-boot does (host_state_dir +
    // user_home_dir_root) so the wrapper's existence checks pass.
    // PUBKEY_HEX is left unset on restore: CH ignores --cmdline on
    // --restore, and the wrapper's hex validation now skips when
    // ZSBX_RESTORE_FROM is set.
    let env = serde_json::json!({
        "ZSBX_VM_INDEX": vm_index.to_string(),
        "ZSBX_ARTIFACT_DIR": cfg.runtime_dir.display().to_string(),
        "ZSBX_RUNTIME": "${NOMAD_TASK_DIR}",
        "ZSBX_WORKSPACE_IMG": workspace_img.display().to_string(),
        "ZSBX_USER_HOME_IMG": user_home_img.display().to_string(),
        // Must match the snapshot's saved config — CH refuses to
        // restore against a memory size mismatch. Pulled from the
        // controller's SandboxConfig at backend construction.
        "ZSBX_VM_MEMORY_MB": memory_mb.to_string(),
        "ZSBX_VM_CPUS_BOOT": cpus_boot.to_string(),
        // The wrapper's PR 3f restore branch reads this and switches
        // to `cloud-hypervisor --restore source_url=file://<dir>`.
        "ZSBX_RESTORE_FROM": alloc_dir.display().to_string(),
        "ZSBX_SUBNET_BASE_OCTET": cfg.subnet_second_octet.to_string(),
    });

    let resources = serde_json::json!({
        // Match nomad_ch.rs: CPU MHz advisory under raw_exec + CH;
        // memory comes from the snapshot's saved config.
        //
        // MemoryMaxMB = 2 × MemoryMB (bug-#9 fix, 2026-05-22). CH
        // v51.1 mmap-faults full guest RAM during restore which gets
        // memcg-accounted; without 2× slack the cgroup OOM-kills CH
        // at ~t=30s before /livez is reachable. Mirrors cold-boot's
        // jobspec in nomad_ch.rs.
        "CPU": 500,
        "MemoryMB": memory_mb,
        "MemoryMaxMB": memory_mb * 2,
    });

    // Driver + Config — only material difference between the two
    // modes. Under ChPlugin the typed surface matches the Go driver's
    // TaskConfig struct in nomad-driver-ch/ch/task_config.go, with
    // `restore_from` carrying the staged snapshot dir (the driver's
    // StartTask branches on this to spawn `cloud-hypervisor --restore
    // source_url=file://<RestoreFrom>`).
    let (driver_name, config): (&str, serde_json::Value) = match mode {
        crate::backend::nomad_ch::TaskDriverMode::RawExec => (
            "raw_exec",
            serde_json::json!({
                "command": cfg.wrapper_path.display().to_string(),
            }),
        ),
        crate::backend::nomad_ch::TaskDriverMode::ChPlugin => {
            // Field names + types mirror nomad-driver-ch/ch/task_config.go::TaskConfig.
            // The cold-boot builder in nomad_ch.rs emits the same
            // shape; the only differences here are:
            //   - restore_from carries the staged snapshot dir (the
            //     driver dispatches on non-empty),
            //   - pubkey_hex is left empty because CH ignores
            //     --cmdline on --restore (parallels the wrapper's
            //     restore-branch skipping the PUBKEY_HEX validator),
            //   - sandbox_id passes through cfg-agnostic so the
            //     driver's logs carry it.
            //
            // We deliberately do NOT set `command` here — the Go
            // driver's TaskConfig has no such field; including it
            // would either be ignored (best case) or fail HCL decode
            // if the schema gets stricter.
            let kernel_path = cfg.runtime_dir.join("vmlinuz");
            // C-7-LT-12a (smoke-r22): the source bytes for rootfs.img
            // the driver hardlinks (or copies on EXDEV) into runDir
            // before CH spawn. Matches cold-boot's materializeRootfs
            // source path (`$ZSBX_ARTIFACT_DIR/rootfs-slim.img` where
            // `$ZSBX_ARTIFACT_DIR == cfg.runtime_dir`); the restore
            // path needs an EXPLICIT field because the snapshot's
            // config.json names the (now-GC'd) source-alloc dir for
            // disks[0].path, so the driver has no other handle on the
            // real source bytes once the rewriter retargets the path.
            let rootfs_source = cfg.runtime_dir.join("rootfs-slim.img");
            (
                "ch",
                serde_json::json!({
                    "vm_index": vm_index,
                    "kernel": kernel_path.display().to_string(),
                    "cpus": cpus_boot,
                    "memory_mb": memory_mb,
                    "restore_from": alloc_dir.display().to_string(),
                    "sandbox_id": sandbox_id.simple().to_string(),
                    // C-7-LT-7 / r21-A1: user_id feeds the driver's
                    // per-user-home path allow-list. Restore is the only
                    // builder invoked during WAKE; without this field the
                    // driver sees empty user_id and rejects
                    // /var/zeroship/ch/users/<user_id>/home.img.
                    // Mirrors cold-boot builder (nomad_ch.rs:2451).
                    "user_id": user_id,
                    "workspace_img": workspace_img.display().to_string(),
                    "user_home_img": user_home_img.display().to_string(),
                    "rootfs_source": rootfs_source.display().to_string(),
                    // Empty: CH ignores --cmdline on --restore.
                    "pubkey_hex": "",
                    "subnet_base_octet": cfg.subnet_second_octet,
                    // Block-lists: empty triggers driver-side
                    // auto-synthesis from the typed fields above
                    // (matches T-3 default behaviour + nomad_ch.rs
                    // cold-boot builder).
                    "disks": [],
                    "fs": [],
                    "net": [],
                }),
            )
        }
    };

    serde_json::json!({
        "Job": {
            "ID": job_id,
            "Name": job_id,
            "Type": "service",
            "Datacenters": [cfg.datacenter],
            "Meta": {
                "zeroship.sandbox": sandbox_id.to_string(),
                "zeroship.user": user_id,
                "zeroship.vm_index": vm_index.to_string(),
                "zeroship.kind": "restore",
            },
            "TaskGroups": [{
                "Name": "vm",
                "Count": 1,
                "RestartPolicy": {
                    "Attempts": 0,
                    "Mode": "fail",
                    "Interval": 30_000_000_000u64,
                    "Delay":     5_000_000_000u64,
                },
                "ReschedulePolicy": {
                    "Attempts": 0,
                    "Unlimited": false,
                },
                "Tasks": [{
                    "Name": "ch",
                    "Driver": driver_name,
                    "Config": config,
                    "Env": env,
                    "Resources": resources,
                    "KillTimeout": 10_000_000_000u64,
                }],
            }],
        }
    })
}

/// Sync HTTP response shape — mirrors `AgentResponse` in nomad_ch.
struct BlockingResponse {
    status: u16,
    body: String,
}

fn nomad_post_blocking(
    url: &str,
    body: &[u8],
    timeout: Duration,
) -> Result<BlockingResponse, String> {
    let req = ureq::post(url)
        .timeout(timeout)
        .set("content-type", "application/json");
    send_ureq_blocking(req, body)
}

fn nomad_get_blocking(
    url: &str,
    timeout: Duration,
) -> Result<BlockingResponse, String> {
    let req = ureq::get(url).timeout(timeout);
    send_ureq_blocking(req, &[])
}

fn nomad_delete_blocking(
    url: &str,
    timeout: Duration,
) -> Result<BlockingResponse, String> {
    let req = ureq::delete(url).timeout(timeout);
    send_ureq_blocking(req, &[])
}

fn send_ureq_blocking(
    req: ureq::Request,
    body: &[u8],
) -> Result<BlockingResponse, String> {
    use std::io::Read;
    let send = if body.is_empty() {
        req.call()
    } else {
        req.send_bytes(body)
    };
    match send {
        Ok(resp) => {
            let status = resp.status();
            let mut bytes = Vec::new();
            let _ = resp.into_reader().take(8 * 1024 * 1024).read_to_end(&mut bytes);
            Ok(BlockingResponse {
                status,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            })
        }
        Err(ureq::Error::Status(code, resp)) => {
            let mut bytes = Vec::new();
            let _ = resp.into_reader().take(8 * 1024).read_to_end(&mut bytes);
            Ok(BlockingResponse {
                status: code,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            })
        }
        Err(e) => Err(format!("{e}")),
    }
}

/// Sync version of `wait_for_alloc_running` (nomad_ch.rs) — polls
/// the job's allocations every 250 ms until at least one reaches
/// `ClientStatus="running"`, or terminal (failed/lost) → Err, or the
/// deadline expires.
fn wait_for_alloc_running_blocking(
    nomad_addr: &str,
    job_id: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let url = format!("{nomad_addr}/v1/job/{job_id}/allocations");
    let mut last_status: Option<String> = None;
    let mut last_err: Option<String> = None;
    while Instant::now() < deadline {
        match nomad_get_blocking(&url, Duration::from_secs(5)) {
            Ok(r) if r.status == 200 => {
                let allocs: Result<serde_json::Value, _> =
                    serde_json::from_str(&r.body);
                match allocs {
                    Ok(allocs) => {
                        for a in allocs.as_array().into_iter().flatten() {
                            let cs = a["ClientStatus"]
                                .as_str()
                                .unwrap_or("")
                                .to_string();
                            if cs == "running" {
                                return Ok(());
                            }
                            if cs == "failed" || cs == "lost" {
                                let desc = a["ClientDescription"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string();
                                // T-8b-stress-r2 controller v34: harvest
                                // per-task TaskEvent DisplayMessage too
                                // (the actionable driver-side error;
                                // Nomad's `ClientDescription` is a
                                // generic "Failed tasks" rollup). Single
                                // helper shared with `nomad_ch.rs::
                                // wait_for_alloc_running` so cold-boot
                                // and wake errors carry the same shape.
                                let driver_msgs =
                                    crate::backend::nomad_ch::extract_failed_task_event_msgs(a);
                                let composed = if driver_msgs.is_empty() {
                                    format!("nomad alloc terminal status={cs}: {desc}")
                                } else {
                                    format!(
                                        "nomad alloc terminal status={cs}: {desc}: {}",
                                        driver_msgs.join(" | ")
                                    )
                                };
                                return Err(composed);
                            }
                            last_status = Some(cs);
                        }
                    }
                    Err(e) => {
                        last_err = Some(format!("parse allocs: {e}"));
                    }
                }
            }
            Ok(r) => {
                last_err = Some(format!("status {} body={}", r.status, r.body.trim()));
            }
            Err(e) => {
                last_err = Some(e);
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let mut msg = format!(
        "restore: nomad alloc never reached running for {job_id} (last status={:?})",
        last_status.unwrap_or_else(|| "<no allocs>".to_string())
    );
    if let Some(e) = last_err {
        msg.push_str(&format!(" last_err={e}"));
    }
    Err(msg)
}

/// Sync version of `wait_for_agent_livez` — polls just the unsigned
/// `/livez` until 200, or the deadline. v1 skips the signed /version
/// fingerprint check; that requires plumbing the per-sandbox signing
/// key out of the sealed record (follow-up PR). For v1 the absence
/// of the fp check is acceptable because the restore path forces the
/// source vm_index — there's no "stale tenant" because the prior
/// alloc already terminated as part of the snapshot's destructive
/// teardown.
fn wait_for_livez_blocking(
    base_url: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let url = format!("{base_url}/livez");
    let mut last: Option<String> = None;
    while Instant::now() < deadline {
        match nomad_get_blocking(&url, Duration::from_millis(500)) {
            Ok(r) if r.status == 200 => return Ok(()),
            Ok(r) => last = Some(format!("status {}", r.status)),
            Err(e) => last = Some(e),
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    Err(format!(
        "restore: agent at {base_url} never returned 200 on /livez (last={})",
        last.unwrap_or_else(|| "<no responses>".into())
    ))
}

/// Bug #22 fix (cluster smoke 2026-05-23 r6+B22-fixer). After CH
/// `--restore` brings the VM back, the guest's `CLOCK_REALTIME` is
/// frozen at the snapshot-time value, so the agent's strict 5-second
/// skew check rejects every subsequent signed RPC with 401
/// unauthorized. This helper issues a single signed `POST
/// /_clock_resync` with the current controller timestamp; the agent
/// verifies the signature WITHOUT applying the skew gate
/// (`verify_kind_skew_bypass`) and `settimeofday(2)`s
/// `CLOCK_REALTIME` to the request's signed ts. After it returns Ok
/// the agent's wall clock matches the controller's and subsequent
/// `/exec`, `/files`, etc. work under the normal strict-skew path.
///
/// **Signing:** uses [`zeroship_sandbox_agent::sig::sign`] with the
/// per-sandbox Ed25519 signing key the caller has just unsealed from
/// [`crate::persist::Persistence`]. Same canonical/header format every
/// other controller→agent RPC uses; the only difference is the agent
/// dispatches `/_clock_resync` through the skew-bypass verifier.
///
/// **R7-S1 body shape (wire change).** The signed body is now
/// `{"sandbox_id": <uuid>, "ts": <unix_secs>, "challenge": <hex64>}`.
/// The `sandbox_id` field lets the agent reject a captured resync
/// from sandbox A replayed against sandbox B; the `challenge` field
/// (32 random bytes hex-encoded) lets the agent reject a captured
/// resync from cycle-N replayed against cycle-N+1 of the same sandbox.
/// Pre-R7-S1 controllers signed just `{"ts": ...}` — those messages
/// no longer pass the new agent's body validation. Rolling upgrade:
/// rootfs v5 + controller v18 land together; the wire change is
/// atomic in deployment.
///
/// **Security:** the controller's private key is the trust anchor —
/// an in-VM attacker can't forge this call. The agent's nonce LRU
/// prevents a captured resync from being replayed within the agent's
/// process lifetime; the new **challenge LRU** prevents the post-
/// restore replay-DoS (a fresh outer nonce on the same captured body
/// would otherwise slip past). The endpoint is the ONLY one that
/// bypasses the skew window; every other agent endpoint stays on
/// strict 5-second skew.
///
/// **Failure mode:** any non-200 response (including the agent's
/// genuine 401 if signing-key bytes are wrong) surfaces as `Err`. The
/// caller (`do_restore_inner`) maps this to a `Backend(...)` error so
/// the wake path rolls back to `Snapshotted` rather than wedging the
/// row at `Restoring` with a broken VM.
pub(crate) async fn clock_resync_post_restore(
    agent_url: &str,
    sandbox_id: Uuid,
    signing_key_bytes: &[u8; 32],
) -> Result<(), String> {
    use ed25519_dalek::SigningKey;
    use zeroship_sandbox_agent::sig;

    let signing_key = SigningKey::from_bytes(signing_key_bytes);
    let url = format!("{agent_url}/_clock_resync");
    let path = "/_clock_resync".to_string();
    let url_for_blocking = url.clone();
    // R7-S1: bind the sandbox UUID into the signed body so the agent's
    // handler can assert it matches its own boot-time-known id. We
    // capture by value because spawn_blocking takes `'static` closures.
    // B24-FOLLOWUP: use .simple() (32-char hex, no hyphens) to match
    // the canonical form the wrapper validator accepts and that the
    // agent reads from SANDBOX_AGENT_SANDBOX_ID. Hyphenated form would
    // 401 every clock_resync on the restore path.
    let sandbox_id_str = sandbox_id.simple().to_string();
    compio::runtime::spawn_blocking(move || {
        // Use std::time directly here (mirror of `unix_now` in
        // nomad_ch.rs) — clock_resync targets `CLOCK_REALTIME` so
        // we want CLOCK_REALTIME bytes too, matching whatever the
        // controller's wall clock thinks "now" is.
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // R7-S1: mint a fresh per-restore challenge (32 random bytes
        // hex-encoded → 64 chars to match the agent's
        // `RESYNC_CHALLENGE_HEX_LEN`). The agent's challenge LRU
        // rejects any value it has seen recently — without this bind,
        // a captured resync could be replayed POST-cycle to set a
        // stale ts and 401 every strict-skew RPC.
        let challenge = match clock_resync_random_hex(32) {
            Ok(c) => c,
            Err(e) => return Err(format!("challenge gen: {e}")),
        };
        // Bind sandbox_id + ts + challenge into the body so the
        // agent's body-hash check covers all three and an in-VM
        // attacker can't substitute a different value for any of them.
        // R7-S1 wire shape (pre-R7-S1 was just `{"ts": ts}`).
        let body = serde_json::json!({
            "sandbox_id": sandbox_id_str,
            "ts": ts,
            "challenge": challenge,
        })
        .to_string();
        let body_bytes = body.as_bytes();
        let nonce = match clock_resync_nonce() {
            Ok(n) => n,
            Err(e) => return Err(format!("nonce gen: {e}")),
        };
        let signature = sig::sign(&signing_key, "POST", &path, body_bytes, ts, &nonce);
        let resp = ureq::post(&url_for_blocking)
            .timeout(Duration::from_secs(10))
            .set("content-type", "application/json")
            .set("x-sbx-timestamp", &ts.to_string())
            .set("x-sbx-nonce", &nonce)
            .set("x-sbx-signature", &signature)
            .send_bytes(body_bytes);
        match resp {
            Ok(r) => {
                let status = r.status();
                if status == 200 {
                    return Ok(());
                }
                let body_excerpt = r.into_string().unwrap_or_default();
                Err(format!(
                    "/_clock_resync status {status}: {}",
                    body_excerpt.chars().take(256).collect::<String>()
                ))
            }
            Err(ureq::Error::Status(code, r)) => {
                let body_excerpt = r.into_string().unwrap_or_default();
                Err(format!(
                    "/_clock_resync status {code}: {}",
                    body_excerpt.chars().take(256).collect::<String>()
                ))
            }
            Err(e) => Err(format!("/_clock_resync transport: {e}")),
        }
    })
    .await
    .unwrap_or_else(|p| Err(format!("clock_resync spawn_blocking panic: {p:?}")))
}

/// Random hex nonce for `/_clock_resync`. Matches the agent's
/// `MAX_NONCE_LEN=64` ceiling with plenty of margin (32 hex chars
/// over 128 bits of entropy). Same `/dev/urandom`-backed shape as
/// `random_hex` in `crates/sandbox/src/restore.rs`; kept local so
/// the restore module doesn't reach into backend internals — bug
/// #22's surgical scope is fully contained to (a) restore_handler,
/// (b) the agent crate.
fn clock_resync_nonce() -> Result<String, String> {
    clock_resync_random_hex(16)
}

/// R7-S1 helper: fetch `n_bytes` from `/dev/urandom` and hex-encode.
/// Used for both the per-restore `challenge` field (n_bytes=32 → 64
/// hex chars, matching the agent's `RESYNC_CHALLENGE_HEX_LEN`) and
/// the resync nonce (n_bytes=16 → 32 hex chars). Same source as
/// `random_hex` in `crates/sandbox/src/restore.rs`; kept local so the
/// restore module doesn't reach into backend internals.
fn clock_resync_random_hex(n_bytes: usize) -> Result<String, String> {
    use std::io::Read as _;
    let mut buf = vec![0u8; n_bytes];
    std::fs::File::open("/dev/urandom")
        .map_err(|e| format!("open /dev/urandom: {e}"))?
        .read_exact(&mut buf)
        .map_err(|e| format!("read /dev/urandom: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod real_backend_tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU32, Ordering as AOrdering};
    use std::thread;

    fn fresh_dir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "zsbx-restore-real-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn base_cfg(nomad_addr: String, host_state: PathBuf) -> NomadCHConfig {
        // T-8b-stress Bug 1: route user_home_dir_root under the
        // per-test host_state dir so `stage_disk_image_preconditions`
        // can write to it without needing /var/zeroship/ch/users
        // (which doesn't exist in CI). Production points both at the
        // real layout (HOST_STATE_DIR=/var/zeroship/ch,
        // USER_HOME_ROOT=/var/zeroship/ch/users); the relative layout
        // is preserved.
        let user_home_dir_root = host_state.join("users");
        NomadCHConfig {
            nomad_addr,
            datacenter: "dc1".into(),
            wrapper_path: PathBuf::from("/etc/zeroship/nomad-vm-wrapper.sh"),
            runtime_dir: PathBuf::from("/var/lib/zeroship/ch"),
            host_state_dir: host_state,
            user_home_dir_root,
            vm_index_floor: 1,
            vm_index_ceil: 155,
            alloc_running_timeout_secs: 1,
            agent_livez_timeout_secs: 1,
            host_fence_timeout_secs: 30,
            startup_orphan_cleanup: false,
            subnet_second_octet: 99,
        }
    }

    /// Tiny synchronous HTTP server thread for tests. Accepts one
    /// connection at a time; serves a fixed `(status, body)` pair.
    /// The handler is a closure that returns `(status, body, kind)`
    /// per request so we can vary responses across calls.
    fn spawn_fake_nomad<F>(handler: F) -> (String, Arc<AtomicU32>)
    where
        F: Fn(u32) -> (u16, String) + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let counter = Arc::new(AtomicU32::new(0));
        let c2 = counter.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut s = match stream {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let n = c2.fetch_add(1, AOrdering::SeqCst);
                let (status, body) = handler(n);
                use std::io::{Read, Write};
                // Drain request — at least one chunk; ureq sends
                // headers + maybe body. We don't actually parse.
                let mut buf = [0u8; 8192];
                let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
                let _ = s.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(response.as_bytes());
                let _ = s.flush();
            }
        });
        (format!("http://{addr}"), counter)
    }

    /// T-8b-stress Bug 1 helper: stage the workspace.img + home.img
    /// preconditions that `submit_restore_job` now asserts before it
    /// even submits the Nomad job. Mirrors the on-disk state that
    /// `stop_preserving_state` (snapshot teardown) + `create()` (source
    /// mkfs) leave behind in production. Tests that drive
    /// `submit_restore_job` directly need this; the full happy-path
    /// (via `restore_sandbox`) goes through `store.get` first, which
    /// implicitly stages a real artifact tree.
    fn stage_disk_image_preconditions(
        cfg: &crate::config::NomadCHConfig,
        sandbox_id: Uuid,
        user_id: &str,
    ) {
        let host_dir = cfg
            .host_state_dir
            .join(sandbox_id.simple().to_string());
        std::fs::create_dir_all(&host_dir).unwrap();
        // Non-empty content so the post-stage `assert_disk_image_present`
        // check (size > 0) passes.
        std::fs::write(host_dir.join("workspace.img"), b"fake-workspace").unwrap();

        let user_home_dir = cfg.user_home_dir_root.join(user_id);
        std::fs::create_dir_all(&user_home_dir).unwrap();
        std::fs::write(user_home_dir.join("home.img"), b"fake-home").unwrap();
    }

    /// Submit + alloc-running succeeds when Nomad returns 200 then a
    /// running alloc.
    #[test]
    fn submit_restore_job_succeeds_when_nomad_returns_running() {
        let host_state = fresh_dir();
        let (nomad_addr, calls) = spawn_fake_nomad(move |n| match n {
            0 => (200, "{}".to_string()),
            _ => (
                200,
                r#"[{"ClientStatus":"running","ClientDescription":"ok"}]"#.to_string(),
            ),
        });
        let cfg = base_cfg(nomad_addr, host_state.clone());
        let backend = RealRestoreBackend::new(cfg.clone(), 1024, 2.0);
        let sid = Uuid::now_v7();
        let alloc_dir = backend.restore_alloc_dir(sid);
        std::fs::create_dir_all(&alloc_dir).unwrap();
        // T-8b-stress Bug 1: submit_restore_job now asserts the disk
        // images are present before submitting (cold-boot/restore
        // parity check). Stage the source-create's residual files.
        stage_disk_image_preconditions(&cfg, sid, "usr_test");

        backend
            .submit_restore_job(sid, 7, &alloc_dir, "usr_test")
            .expect("submit_restore_job must succeed");
        assert!(
            calls.load(AOrdering::SeqCst) >= 2,
            "expected at least submit + 1 poll; got {}",
            calls.load(AOrdering::SeqCst)
        );
        let _ = std::fs::remove_dir_all(&host_state);
    }

    /// 500 on POST surfaces as a recoverable error (the handler will
    /// CAS restoring → snapshotted).
    #[test]
    fn submit_restore_job_errors_when_nomad_500s() {
        let host_state = fresh_dir();
        let (nomad_addr, _) = spawn_fake_nomad(|_| {
            (500, r#"{"error":"nomad: backend down"}"#.to_string())
        });
        let cfg = base_cfg(nomad_addr, host_state.clone());
        let backend = RealRestoreBackend::new(cfg.clone(), 1024, 2.0);
        let sid = Uuid::now_v7();
        let alloc_dir = backend.restore_alloc_dir(sid);
        std::fs::create_dir_all(&alloc_dir).unwrap();
        // T-8b-stress Bug 1: stage disk-image preconditions so the
        // preflight assertion lets us actually reach the Nomad POST.
        stage_disk_image_preconditions(&cfg, sid, "usr_test");

        let err = backend
            .submit_restore_job(sid, 8, &alloc_dir, "usr_test")
            .expect_err("500 must error");
        assert!(err.contains("status 500"), "{err}");
        let _ = std::fs::remove_dir_all(&host_state);
    }

    /// reserve_vm_index returns Err on collision (simulates v1's
    /// "no cluster fallback" surface).
    #[test]
    fn reserve_vm_index_collides() {
        let cfg = base_cfg("http://127.0.0.1:1".into(), fresh_dir());
        let backend = RealRestoreBackend::new(cfg, 1024, 2.0);
        backend.reserve_vm_index(42).expect("first must succeed");
        let err = backend.reserve_vm_index(42).expect_err("collision");
        assert!(err.contains("already reserved"), "{err}");
        backend.release_vm_index(42);
        backend.reserve_vm_index(42).expect("post-release must succeed");
    }

    /// **B18 regression** (cluster smoke 2026-05-24 r4): the wake
    /// path's `reserve_vm_index` must block a concurrent create-side
    /// `alloc()` from handing out the same slot. Pre-fix, two
    /// separate allocators (NomadCHBackend's `vm_index_allocator` and
    /// RealRestoreBackend's private `VmIndexReservations`) let a
    /// CREATE-after-WAKE silently reuse a slot held by a live
    /// restored VM, surfacing as a stale-pubkey 401 on /version.
    /// Post-fix, `with_shared_allocator` routes reserve/release
    /// through the create-side pool so this is impossible.
    #[test]
    fn b18_shared_allocator_blocks_create_side_reuse() {
        use crate::backend::nomad_ch::VmIndexAllocator;
        let cfg = base_cfg("http://127.0.0.1:1".into(), fresh_dir());
        // Create-side allocator. Models `NomadCHBackend::vm_index_allocator`.
        let shared = Arc::new(Mutex::new(VmIndexAllocator::new(1, 5)));
        let backend = RealRestoreBackend::new(cfg, 1024, 2.0)
            .with_shared_allocator(shared.clone());
        // Simulate the prior cycle: create-side allocs slot 1, then
        // releases it as part of `stop_preserving_state` (snapshot
        // teardown). Slot 1 is now in `freed`.
        let slot = shared.lock().unwrap().alloc().unwrap();
        assert_eq!(slot, 1);
        shared.lock().unwrap().release(slot);
        // Wake reserves the slot via RealRestoreBackend.
        backend.reserve_vm_index(slot as i16).expect("wake reserve");
        // Now a concurrent create-side `alloc()` MUST NOT hand back
        // slot 1 (the restored VM is still alive on that tap/IP).
        // Pre-B18-fix it did — the freed set still contained slot 1
        // because reserve() ran against a separate allocator.
        let next = shared.lock().unwrap().alloc().unwrap();
        assert_ne!(
            next, slot,
            "B18 regression: create-side allocator handed out slot {slot} \
             while restored VM still owned it via wake reserve()"
        );
        // Release on wake teardown frees the slot for the next create.
        backend.release_vm_index(slot as i16);
        let reclaimed = shared.lock().unwrap().alloc().unwrap();
        assert_eq!(
            reclaimed, slot,
            "post-release, the slot must be reusable by create-side alloc"
        );
    }

    /// **B18 regression**: with shared allocator, calling
    /// `reserve_vm_index` twice for the same slot must fail (the
    /// allocator's new in-flight collision detection refuses
    /// double-reserve of a live slot, even when the prior reserver is
    /// the create-side `alloc()` rather than another wake).
    #[test]
    fn b18_shared_allocator_rejects_double_reserve_of_live_slot() {
        use crate::backend::nomad_ch::VmIndexAllocator;
        let cfg = base_cfg("http://127.0.0.1:1".into(), fresh_dir());
        let shared = Arc::new(Mutex::new(VmIndexAllocator::new(1, 5)));
        let backend = RealRestoreBackend::new(cfg, 1024, 2.0)
            .with_shared_allocator(shared.clone());
        // Create-side hands out slot 1; the slot is now live in the
        // shared pool (next=2, freed={}).
        let live = shared.lock().unwrap().alloc().unwrap();
        assert_eq!(live, 1);
        // Wake tries to reserve the same live slot — must error.
        // Pre-B18-fix the allocator's `reserve()` would silently
        // succeed (no in-flight detection), and the wake handler
        // would proceed to submit a restore alloc that collides with
        // the live create-side sandbox.
        let err = backend
            .reserve_vm_index(live as i16)
            .expect_err("must reject reserve of in-flight slot");
        assert!(
            err.contains("already reserved"),
            "expected 'already reserved' error, got: {err}"
        );
    }

    /// Alloc-never-running times out and the error mentions the job
    /// id + last status. Uses a 1-second cfg budget (set in
    /// `base_cfg`).
    #[test]
    fn submit_restore_job_times_out_when_alloc_never_running() {
        let host_state = fresh_dir();
        let (nomad_addr, _) = spawn_fake_nomad(|n| match n {
            0 => (200, "{}".to_string()), // POST OK
            _ => (
                200,
                r#"[{"ClientStatus":"pending","ClientDescription":"queued"}]"#.to_string(),
            ),
        });
        let cfg = base_cfg(nomad_addr, host_state.clone());
        let backend = RealRestoreBackend::new(cfg.clone(), 1024, 2.0);
        let sid = Uuid::now_v7();
        let alloc_dir = backend.restore_alloc_dir(sid);
        std::fs::create_dir_all(&alloc_dir).unwrap();
        // T-8b-stress Bug 1: stage disk-image preconditions so the
        // preflight assertion lets us actually reach the
        // wait_for_alloc_running poll loop that this test exercises.
        stage_disk_image_preconditions(&cfg, sid, "usr_test");

        let err = backend
            .submit_restore_job(sid, 9, &alloc_dir, "usr_test")
            .expect_err("never-running must time out");
        assert!(
            err.contains("never reached running"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&host_state);
    }

    /// T-8b-stress Bug 1 (positive parity test for the restore path):
    /// `submit_restore_job` MUST refuse to submit a Nomad job when
    /// workspace.img is missing — the preflight assertion catches it
    /// at the controller side with a clear path-named error, instead
    /// of letting the driver surface a generic "Failed tasks" alloc
    /// rollup. Mirrors the cold-boot path's post-stage assertion in
    /// `create_ext4_image_if_missing`.
    #[test]
    fn submit_restore_job_rejects_missing_workspace_img() {
        let host_state = fresh_dir();
        // Nomad shouldn't even be contacted — but spawn a server that
        // counts hits so we can assert zero submit calls.
        let (nomad_addr, calls) = spawn_fake_nomad(|_| {
            (200, r#"[{"ClientStatus":"running"}]"#.to_string())
        });
        let cfg = base_cfg(nomad_addr, host_state.clone());
        let backend = RealRestoreBackend::new(cfg, 1024, 2.0);
        let sid = Uuid::now_v7();
        let alloc_dir = backend.restore_alloc_dir(sid);
        std::fs::create_dir_all(&alloc_dir).unwrap();
        // INTENTIONALLY do NOT stage workspace.img/home.img.

        let err = backend
            .submit_restore_job(sid, 7, &alloc_dir, "usr_test")
            .expect_err("missing workspace.img must reject before submit");
        assert!(
            err.contains("workspace.img missing"),
            "err must name the missing precondition; got: {err}"
        );
        // Crucially, no submit RPC fired.
        assert_eq!(
            calls.load(AOrdering::SeqCst),
            0,
            "expected zero Nomad calls when controller-side preflight rejects",
        );
        let _ = std::fs::remove_dir_all(&host_state);
    }

    /// T-8b-stress Bug 1: `submit_restore_job` MUST also catch
    /// missing user_home.img (the second per-user disk). Both
    /// preconditions must fire — the error path for the second
    /// surfaces a different message (workspace was OK, user_home
    /// was the gap), so an operator triaging logs can disambiguate.
    #[test]
    fn submit_restore_job_rejects_missing_user_home_img() {
        let host_state = fresh_dir();
        let (nomad_addr, calls) = spawn_fake_nomad(|_| {
            (200, r#"[{"ClientStatus":"running"}]"#.to_string())
        });
        let cfg = base_cfg(nomad_addr, host_state.clone());
        let backend = RealRestoreBackend::new(cfg.clone(), 1024, 2.0);
        let sid = Uuid::now_v7();
        let alloc_dir = backend.restore_alloc_dir(sid);
        std::fs::create_dir_all(&alloc_dir).unwrap();
        // Stage ONLY workspace.img, not home.img.
        let host_dir = cfg
            .host_state_dir
            .join(sid.simple().to_string());
        std::fs::create_dir_all(&host_dir).unwrap();
        std::fs::write(host_dir.join("workspace.img"), b"fake-workspace").unwrap();

        let err = backend
            .submit_restore_job(sid, 7, &alloc_dir, "usr_test")
            .expect_err("missing user_home.img must reject before submit");
        assert!(
            err.contains("user_home.img missing"),
            "err must name the missing precondition; got: {err}"
        );
        assert_eq!(
            calls.load(AOrdering::SeqCst),
            0,
            "expected zero Nomad calls when controller-side preflight rejects",
        );
        let _ = std::fs::remove_dir_all(&host_state);
    }

    // ─── Bug #22 fix: /_clock_resync handshake ─────────────────────
    //
    // The wake path's `clock_resync_post_restore` issues a single
    // signed POST /_clock_resync to repair the guest's frozen-at-
    // snapshot CLOCK_REALTIME (CH `--restore` preserves the wall
    // clock; the agent's strict 5-second skew would otherwise 401
    // every subsequent /exec). These tests pin:
    //   - happy path: 200 from the agent → `Ok(())`,
    //   - 401 from the agent → `Err(... status 401 ...)`,
    //   - transport failure (closed listener) → `Err(... transport ...)`.

    /// Tiny fake-agent that serves a configurable HTTP response shape.
    /// Mirrors `spawn_fake_nomad` above (we keep it separate so a
    /// future fake-nomad change can't accidentally break the
    /// clock-resync tests).
    fn spawn_fake_agent<F>(handler: F) -> (String, Arc<AtomicU32>)
    where
        F: Fn(u32, &[u8]) -> (u16, String) + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let counter = Arc::new(AtomicU32::new(0));
        let c2 = counter.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut s = match stream {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let n = c2.fetch_add(1, AOrdering::SeqCst);
                use std::io::{Read, Write};
                let mut buf = vec![0u8; 8192];
                let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
                let n_read = s.read(&mut buf).unwrap_or(0);
                buf.truncate(n_read);
                let (status, body) = handler(n, &buf);
                let response = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(response.as_bytes());
                let _ = s.flush();
            }
        });
        (format!("http://{addr}"), counter)
    }

    /// Happy path: agent returns 200 → resync succeeds.
    #[ntex::test]
    async fn clock_resync_post_restore_happy_path() {
        let (agent_url, calls) = spawn_fake_agent(|_, _req| {
            (200, r#"{"resynced":true,"ts":1700000000}"#.to_string())
        });
        let signing_key_bytes = [0xabu8; 32];
        let sandbox_id = Uuid::now_v7();
        let r = clock_resync_post_restore(&agent_url, sandbox_id, &signing_key_bytes).await;
        assert!(r.is_ok(), "happy path must Ok; got {r:?}");
        assert_eq!(
            calls.load(AOrdering::SeqCst),
            1,
            "expected exactly one POST to /_clock_resync"
        );
    }

    /// Agent 401 (e.g., wrong signing-key bytes) → Err carries the
    /// status code so the wake path can roll back with diagnostic text.
    #[ntex::test]
    async fn clock_resync_post_restore_surfaces_agent_401() {
        let (agent_url, _calls) = spawn_fake_agent(|_, _req| {
            (401, r#"{"error":"unauthorized"}"#.to_string())
        });
        let signing_key_bytes = [0xcdu8; 32];
        let sandbox_id = Uuid::now_v7();
        let err = clock_resync_post_restore(&agent_url, sandbox_id, &signing_key_bytes)
            .await
            .expect_err("401 must error");
        assert!(
            err.contains("/_clock_resync status 401"),
            "error text must surface the 401: {err}"
        );
    }

    /// Transport failure (port not listening) → Err. Uses a port we
    /// know nothing will be listening on.
    #[ntex::test]
    async fn clock_resync_post_restore_transport_error() {
        // Bind + immediately drop so the port is closed.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let agent_url = format!("http://{addr}");
        let sandbox_id = Uuid::now_v7();
        let r =
            clock_resync_post_restore(&agent_url, sandbox_id, &[0u8; 32]).await;
        let err = r.expect_err("closed port must error");
        // ureq surfaces this as a transport error; the wrapper text
        // distinguishes from the status-code variant so an operator
        // can grep the right shape out of the controller log.
        assert!(
            err.contains("transport"),
            "expected transport error wrapper text; got: {err}"
        );
    }

    /// **R7-S1**: pin the wire-format change. The body the controller
    /// sends to `/_clock_resync` MUST carry `sandbox_id` + `ts` +
    /// `challenge`. A future contributor who removes any of these
    /// re-opens the post-restore replay-DoS surface; this test reads
    /// the bytes the fake agent received and asserts the JSON keys
    /// + the 64-char challenge width.
    #[ntex::test]
    async fn clock_resync_post_restore_binds_sandbox_id_and_challenge() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        // Hand-rolled fake agent that reads UNTIL the body content-
        // length is satisfied. The default `spawn_fake_agent` reads
        // one 8KB chunk and may miss the body on slow TCP packets;
        // this variant explicitly drains both halves of the request.
        let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_for_handler = captured.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let mut stream = match listener.accept() {
                Ok((s, _)) => s,
                Err(_) => return,
            };
            let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
            // Drain the request in a loop until we've seen CRLF CRLF
            // (header terminator) AND the body bytes implied by
            // Content-Length, OR until the read times out.
            let mut all = Vec::with_capacity(4096);
            let mut buf = [0u8; 4096];
            let mut header_end: Option<usize> = None;
            let mut content_length: Option<usize> = None;
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        all.extend_from_slice(&buf[..n]);
                        if header_end.is_none() {
                            if let Some(idx) =
                                all.windows(4).position(|w| w == b"\r\n\r\n")
                            {
                                header_end = Some(idx + 4);
                                // Parse Content-Length from headers.
                                let hdrs = &all[..idx];
                                for line in hdrs.split(|b| *b == b'\n') {
                                    let line = String::from_utf8_lossy(line);
                                    let line = line.trim();
                                    if line.to_ascii_lowercase()
                                        .starts_with("content-length:")
                                    {
                                        if let Some((_, v)) = line.split_once(':') {
                                            if let Ok(n) = v.trim().parse::<usize>() {
                                                content_length = Some(n);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if let (Some(end), Some(cl)) = (header_end, content_length) {
                            if all.len() >= end + cl {
                                break;
                            }
                        }
                    }
                    Err(_) => break, // read timeout — we have what we have
                }
            }
            // Hand the captured body to the assertion code via the
            // shared Arc<Mutex<>>. Empty body falls through to the
            // assertion's panic message.
            if let (Some(end), Some(cl)) = (header_end, content_length) {
                let slice_end = (end + cl).min(all.len());
                let mut g = captured_for_handler.lock().unwrap();
                *g = all[end..slice_end].to_vec();
            } else if let Some(end) = header_end {
                let mut g = captured_for_handler.lock().unwrap();
                *g = all[end..].to_vec();
            }
            let body = r#"{"resynced":true,"ts":1700000000}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        });
        let agent_url = format!("http://{addr}");

        let signing_key_bytes = [0xefu8; 32];
        let sandbox_id = Uuid::now_v7();
        let r =
            clock_resync_post_restore(&agent_url, sandbox_id, &signing_key_bytes)
                .await;
        assert!(r.is_ok(), "happy path must Ok; got {r:?}");
        // Body has all three fields and the challenge is exactly the
        // 64-char hex width the agent demands.
        let body_bytes = captured.lock().unwrap().clone();
        let body_str = String::from_utf8_lossy(&body_bytes).to_string();
        let parsed: serde_json::Value = serde_json::from_str(&body_str)
            .unwrap_or_else(|e| panic!("body not valid JSON: {e}: body={body_str:?}"));
        // R7-S1 + B24-FOLLOWUP: sandbox_id field carries the .simple()
        // form (32-hex, no hyphens) — same canonical form the wrapper
        // env-injects as SANDBOX_AGENT_SANDBOX_ID and that the agent
        // boots with. Hyphenated form would 401 the resync.
        assert_eq!(
            parsed["sandbox_id"].as_str().unwrap_or(""),
            sandbox_id.simple().to_string(),
            "R7-S1+B24-FOLLOWUP: body must bind the canonical .simple() form"
        );
        // R7-S1: challenge is a 64-char lowercase hex string (32 random bytes).
        let challenge = parsed["challenge"].as_str().unwrap_or("");
        assert_eq!(
            challenge.len(),
            64,
            "R7-S1: challenge MUST be exactly 64 hex chars (32 random bytes); got len={}",
            challenge.len()
        );
        assert!(
            challenge.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "R7-S1: challenge MUST be lowercase hex; got {challenge:?}"
        );
        // R7-S1: ts is present + numeric.
        assert!(
            parsed["ts"].is_u64(),
            "R7-S1: body must carry a numeric ts; got {parsed:?}"
        );
    }

    // ─── R10-C1 + R10-C2 regression (concurrency-r10 2026-05-25):
    //     `teardown_restore` on the rollback path must
    //       (a) remove the nomad-ch state-map entry that
    //           `register_restored` inserted, and
    //       (b) release the vm_index back into the shared allocator,
    //     so that a subsequent `register_restored` at the same slot
    //     can succeed. Without (a), a late-rollback (e.g.
    //     `update_sandbox_status(Running) → CasLost`) leaves a ghost
    //     state-map entry at a slot the next `create` would land on.
    //     Source: docs/reviews/sandbox-snapshot-restore-concurrency-2026-05-25-r10.md
    //     R10-C1 + R10-C2.

    /// R10-C1: full integration over `RealRestoreBackend` +
    /// `NomadCHBackend` wired via `with_nomad_handle`. Walks the
    /// rollback shape: `register_restored` (success branch) → late
    /// failure → `teardown_restore` must wipe the state-map entry +
    /// release the vm_index. Then a second `register_restored` at the
    /// same `sandbox_id` succeeds (Vacant slot).
    #[ntex::test]
    async fn r10_c1_teardown_restore_removes_state_map_entry() {
        use crate::backend::nomad_ch::NomadCHBackend;
        // Fake nomad that 200s the rollback DELETE.
        let host_state = fresh_dir();
        let (nomad_addr, _calls) =
            spawn_fake_nomad(|_| (200, "{}".to_string()));
        let mut sandbox_cfg = crate::config::SandboxConfig::new_fixture();
        sandbox_cfg.nomad_ch.nomad_addr = nomad_addr.clone();
        sandbox_cfg.nomad_ch.host_state_dir = host_state.clone();
        sandbox_cfg.nomad_ch.vm_index_floor = 4;
        sandbox_cfg.nomad_ch.vm_index_ceil = 6;
        // Construct the real NomadCH backend so we can wire it as a
        // shared handle into RealRestoreBackend.
        let nomad_backend =
            Arc::new(NomadCHBackend::new(sandbox_cfg.clone(), None).expect("nomad new"));
        let allocator = nomad_backend.vm_index_allocator();

        // Build the RealRestoreBackend with both the shared allocator
        // (so release_vm_index lands in the right pool) AND the
        // nomad_handle (so teardown_restore can call unregister_restored).
        let restore_cfg = base_cfg(nomad_addr, host_state.clone());
        let backend = RealRestoreBackend::new(restore_cfg, 1024, 2.0)
            .with_shared_allocator(allocator.clone())
            .with_nomad_handle(nomad_backend.clone());

        let id = Uuid::now_v7();
        let vm_index: i16 = 5;

        // Reserve + register: mirrors the do_restore_inner success
        // path up to (but not including) the final CAS.
        backend
            .reserve_vm_index(vm_index)
            .expect("reserve must succeed");
        backend
            .register_restored(
                id,
                vm_index,
                [0xc1u8; 32],
                "usr_r10c1_int",
            )
            .expect("register_restored must succeed");
        // Sanity: the entry landed in the state map.
        assert!(
            nomad_backend.contains_for_test(id),
            "test setup: register_restored must place an entry"
        );

        // Now drive the rollback path. Pre-R10-C1 fix this would have
        // released the vm_index but left the state-map entry behind.
        backend.teardown_restore(id, vm_index);

        // (a) State-map empty for this id: the R10-C1 invariant.
        assert!(
            !nomad_backend.contains_for_test(id),
            "R10-C1 regression: teardown_restore did NOT remove the \
             nomad-ch state-map entry; ghost entry leaks past the rollback"
        );

        // (b) vm_index released: a follow-on alloc() hands out the
        //     freed slot 5 (freed-set wins over `next` in the
        //     VmIndexAllocator).
        let reclaimed = allocator.lock().unwrap().alloc().expect("alloc");
        assert_eq!(
            reclaimed, vm_index as u16,
            "R10-C1: vm_index slot {vm_index} not returned to allocator \
             after teardown_restore; got {reclaimed}"
        );

        // (c) After unregister, a subsequent register_restored at the
        //     same `sandbox_id` succeeds — symmetric inverse closure.
        backend
            .register_restored(
                id,
                vm_index,
                [0xc2u8; 32],
                "usr_r10c1_int_re",
            )
            .expect(
                "R10-C1: post-teardown, register_restored at the same \
                 sandbox_id must succeed (Vacant slot)",
            );

        let _ = std::fs::remove_dir_all(&host_state);
    }

    /// R10-C1: early-rollback path (teardown_restore fires BEFORE
    /// register_restored ever ran). The state-map remove must be a
    /// no-op (idempotent — matches `stop_inner`'s tolerance) and the
    /// vm_index release must still happen.
    #[ntex::test]
    async fn r10_c1_teardown_restore_early_rollback_is_idempotent() {
        use crate::backend::nomad_ch::NomadCHBackend;
        let host_state = fresh_dir();
        let (nomad_addr, _calls) =
            spawn_fake_nomad(|_| (200, "{}".to_string()));
        let mut sandbox_cfg = crate::config::SandboxConfig::new_fixture();
        sandbox_cfg.nomad_ch.nomad_addr = nomad_addr.clone();
        sandbox_cfg.nomad_ch.host_state_dir = host_state.clone();
        sandbox_cfg.nomad_ch.vm_index_floor = 10;
        sandbox_cfg.nomad_ch.vm_index_ceil = 12;
        let nomad_backend =
            Arc::new(NomadCHBackend::new(sandbox_cfg, None).expect("nomad new"));
        let allocator = nomad_backend.vm_index_allocator();

        let restore_cfg = base_cfg(nomad_addr, host_state.clone());
        let backend = RealRestoreBackend::new(restore_cfg, 1024, 2.0)
            .with_shared_allocator(allocator.clone())
            .with_nomad_handle(nomad_backend.clone());

        let id = Uuid::now_v7();
        let vm_index: i16 = 11;
        backend
            .reserve_vm_index(vm_index)
            .expect("reserve must succeed");
        // Note: we skip register_restored — this is the early-rollback
        // shape (e.g. submit_restore_job Err).
        assert!(
            !nomad_backend.contains_for_test(id),
            "test setup: no state-map entry before teardown"
        );

        // Must not panic; must release the slot.
        backend.teardown_restore(id, vm_index);
        let reclaimed = allocator.lock().unwrap().alloc().expect("alloc");
        assert_eq!(
            reclaimed, vm_index as u16,
            "R10-C1 early-rollback: vm_index slot not released"
        );

        let _ = std::fs::remove_dir_all(&host_state);
    }

    /// R10-C2: pin the structural shape of the rollback call. The
    /// `do_restore_inner` rollback path MUST wrap `teardown_restore`
    /// in `compio::runtime::spawn_blocking` so the ntex worker doesn't
    /// park on the sync ureq DELETE (up to 10 s). A future contributor
    /// who removes the wrap re-opens the worker-park; this test reads
    /// the source via `include_str!` and asserts the wrap is present
    /// near the rollback call site. Structural assertion because the
    /// runtime behaviour is hard to unit-test deterministically.
    #[test]
    fn r10_c2_rollback_teardown_is_spawn_blocking_wrapped() {
        const SRC: &str = include_str!("restore_handler.rs");
        // Anchor the search at the rollback comment; require that
        // `compio::runtime::spawn_blocking` AND the teardown call
        // appear within the next ~2 KB and that spawn_blocking precedes
        // the call (i.e., the call is INSIDE the wrap). We match on
        // `.teardown_restore(` (call shape with the dot+paren) so the
        // search ignores prose mentions of "teardown_restore" inside
        // the comment block immediately after the anchor.
        let anchor = "// Best-effort teardown of the partially-spawned alloc.";
        let start = SRC
            .find(anchor)
            .expect("R10-C2 anchor comment moved; update the test");
        let window = &SRC[start..start.saturating_add(2048)];
        let sb_idx = window
            .find("compio::runtime::spawn_blocking")
            .unwrap_or_else(|| panic!(
                "R10-C2 regression: rollback path no longer wraps \
                 teardown_restore in compio::runtime::spawn_blocking; \
                 the sync ureq DELETE will park the ntex worker for up \
                 to 10 s."
            ));
        // Find the FIRST actual call (dot-form). The prose in the
        // R10-C2 comment block mentions the bare identifier but never
        // the `.teardown_restore(` call shape.
        let td_idx = window.find(".teardown_restore(").unwrap_or_else(|| {
            panic!("R10-C2: expected `.teardown_restore(` call in rollback window")
        });
        assert!(
            sb_idx < td_idx,
            "R10-C2 regression: spawn_blocking wrap must precede the \
             teardown_restore call (got sb={sb_idx} td={td_idx})"
        );
    }

    /// **R14-A6 wake-path call site**: `RealRestoreBackend` overrides
    /// `vm_index_retry_policy` so the wake budget tracks
    /// `cfg.host_fence_timeout_secs` instead of inheriting the
    /// hard-coded `Default`. Regression-pin: if a future refactor
    /// removes the override (or accidentally restores
    /// `VmIndexRetryPolicy::default()` here), this test catches it by
    /// constructing two backends with different fence timeouts and
    /// asserting their derived policies differ. The trait-method
    /// dispatch is what the wake loop in `reserve_vm_index_with_retry`
    /// actually consults — so this pins the production code path, not
    /// just the formula.
    #[test]
    fn r14a6_real_backend_derives_policy_from_cfg_host_fence_timeout() {
        let root = fresh_dir();
        let mut cfg_short = base_cfg(String::from("http://127.0.0.1:1"), root.clone());
        cfg_short.host_fence_timeout_secs = 20;
        let backend_short = RealRestoreBackend::new(cfg_short, 1024, 2.0);

        let mut cfg_long = base_cfg(String::from("http://127.0.0.1:1"), root.clone());
        cfg_long.host_fence_timeout_secs = 120;
        let backend_long = RealRestoreBackend::new(cfg_long, 1024, 2.0);

        let p_short = backend_short.vm_index_retry_policy();
        let p_long = backend_long.vm_index_retry_policy();
        assert!(
            p_long.max_attempts > p_short.max_attempts,
            "R14-A6 regression: RealRestoreBackend must derive policy \
             from cfg.host_fence_timeout_secs — a longer fence should \
             yield more attempts. short=20s→{} attempts, long=120s→{} \
             attempts. If this fails, the wake call site likely fell \
             back to `VmIndexRetryPolicy::default()` and is no longer \
             cfg-driven.",
            p_short.max_attempts,
            p_long.max_attempts
        );
        // Also pin: NOT the default. 20 s fence with the cfg formula
        // is 6 attempts (≠ default 25), so a Default-only fallback
        // would be detected here.
        let default = VmIndexRetryPolicy::default();
        assert_ne!(
            p_short.max_attempts, default.max_attempts,
            "R14-A6 regression: short-fence backend's policy must NOT \
             match the hard-coded Default ({} attempts) — that would \
             mean the override is missing.",
            default.max_attempts
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

// ─── R12-I1 (T-8 blocker): wake-path SANDBOX_TASK_DRIVER feature
//     flag coverage. Mirrors the T-7 tests in
//     `crates/sandbox/src/backend/nomad_ch.rs` that pin the cold-boot
//     builder under both modes; here we pin the restore-path builder
//     under the same flag so a controller running with
//     `SANDBOX_TASK_DRIVER=ch_plugin` (T-8 cutover) doesn't have a
//     split-brain where CREATEs hit the Go driver but RESTOREs still
//     try raw_exec. Without these tests the regression would only
//     surface on cluster once the bash wrapper is removed.
#[cfg(test)]
mod r12_i1_tests {
    use super::*;
    use crate::backend::nomad_ch::TaskDriverMode;
    // R13-Q1: share the SANDBOX_TASK_DRIVER env-mutex with the
    // cold-boot tests in `backend::nomad_ch::tests`. Both modules
    // mutate the SAME process-global env var; using two separate
    // `Mutex<()>` statics (the prior `R12_I1_ENV_LOCK` here +
    // `T7_ENV_LOCK` there) failed to serialise across modules in the
    // shared test binary. See `backend::nomad_ch::test_env_lock`.
    use crate::backend::nomad_ch::test_env_lock::with_task_driver_env;

    fn fixture_cfg() -> NomadCHConfig {
        NomadCHConfig {
            nomad_addr: "http://127.0.0.1:4646".into(),
            datacenter: "dc1".into(),
            wrapper_path: PathBuf::from("/etc/zeroship/nomad-vm-wrapper.sh"),
            runtime_dir: PathBuf::from("/var/lib/zeroship/ch"),
            host_state_dir: PathBuf::from("/var/zeroship/ch"),
            user_home_dir_root: PathBuf::from("/var/zeroship/ch/users"),
            vm_index_floor: 1,
            vm_index_ceil: 155,
            alloc_running_timeout_secs: 120,
            agent_livez_timeout_secs: 30,
            host_fence_timeout_secs: 30,
            startup_orphan_cleanup: false,
            subnet_second_octet: 99,
        }
    }

    /// `SANDBOX_TASK_DRIVER` unset (default) → wake-path emits the
    /// historical `Driver: "raw_exec"` + bash-wrapper Config. This is
    /// the back-compat contract: the controller MUST NOT opt a fleet
    /// into the Go driver implicitly on either CREATE or wake.
    ///
    /// Drives `submit_restore_job` indirectly by calling the public
    /// (well, pub(crate)) `build_restore_nomad_job_json` with the same
    /// `task_driver_mode_from_env()` arg the production caller uses.
    #[test]
    fn nomad_restore_job_spec_uses_raw_exec_by_default() {
        with_task_driver_env(None, || {
            let cfg = fixture_cfg();
            let sid = Uuid::now_v7();
            let alloc_dir = Path::new("/var/zeroship/ch/snap/restore");
            let mode = crate::backend::nomad_ch::task_driver_mode_from_env();
            let v = build_restore_nomad_job_json(
                "zsbx-restore-default",
                &cfg,
                7,
                alloc_dir,
                sid,
                "usr_alice",
                1024,
                2.0,
                mode,
            );
            let task = &v["Job"]["TaskGroups"][0]["Tasks"][0];
            assert_eq!(
                task["Driver"], "raw_exec",
                "wake-path default MUST stay raw_exec — without this \
                 invariant the bash-wrapper rollout couldn't depend on \
                 the controller picking a known transport"
            );
            assert_eq!(
                task["Config"]["command"],
                "/etc/zeroship/nomad-vm-wrapper.sh",
                "raw_exec mode must still call the bash wrapper"
            );
            // ZSBX_RESTORE_FROM keeps its env-block presence under
            // raw_exec — that's how the wrapper picks the restore
            // branch (line 364 of nomad-vm-wrapper.sh).
            assert_eq!(
                task["Env"]["ZSBX_RESTORE_FROM"],
                "/var/zeroship/ch/snap/restore",
            );
        });
    }

    /// `SANDBOX_TASK_DRIVER=ch_plugin` → wake-path emits `Driver: "ch"`,
    /// matching the Go driver's PluginName const in
    /// `nomad-driver-ch/ch/driver.go`. This is the actual R12-I1 fix:
    /// pre-fix this case still emitted `raw_exec` and would split-brain
    /// against a CREATE under the same flag.
    #[test]
    fn nomad_restore_job_spec_uses_ch_when_flag_set() {
        with_task_driver_env(Some("ch_plugin"), || {
            let cfg = fixture_cfg();
            let sid = Uuid::now_v7();
            let alloc_dir = Path::new("/var/zeroship/ch/snap/restore");
            let mode = crate::backend::nomad_ch::task_driver_mode_from_env();
            let v = build_restore_nomad_job_json(
                "zsbx-restore-flagged",
                &cfg,
                7,
                alloc_dir,
                sid,
                "usr_alice",
                1024,
                2.0,
                mode,
            );
            let task = &v["Job"]["TaskGroups"][0]["Tasks"][0];
            assert_eq!(
                task["Driver"], "ch",
                "R12-I1: wake-path under SANDBOX_TASK_DRIVER=ch_plugin \
                 MUST emit Driver=\"ch\" — matches nomad-driver-ch::ch::PluginName"
            );
        });
    }

    /// Wake-path under ChPlugin populates the typed
    /// `Config.restore_from` from the staged alloc_dir. The Go driver
    /// branches on non-empty `RestoreFrom` to spawn
    /// `cloud-hypervisor --restore source_url=file://<dir>`; an empty
    /// value would silently mis-route through cold-boot and crash CH
    /// on the missing kernel/cmdline.
    #[test]
    fn ch_plugin_restore_jobspec_populates_restore_from() {
        let cfg = fixture_cfg();
        let sid = Uuid::now_v7();
        let alloc_dir = Path::new("/var/zeroship/ch/snap-deadbeef/restore");
        let v = build_restore_nomad_job_json(
            "zsbx-restore-rf",
            &cfg,
            5,
            alloc_dir,
            sid,
            "usr_alice",
            1024,
            2.0,
            TaskDriverMode::ChPlugin,
        );
        let config = &v["Job"]["TaskGroups"][0]["Tasks"][0]["Config"];
        assert_eq!(
            config["restore_from"].as_str(),
            Some("/var/zeroship/ch/snap-deadbeef/restore"),
            "ChPlugin wake-path MUST set Config.restore_from to the \
             staged alloc_dir — driver dispatches on non-empty here"
        );
        // Sanity-check the rest of the typed surface (mirrors the
        // cold-boot ch_plugin_jobspec_includes_all_task_config_fields
        // test in nomad_ch.rs::tests).
        assert_eq!(config["vm_index"].as_u64(), Some(5));
        assert_eq!(
            config["kernel"].as_str(),
            Some("/var/lib/zeroship/ch/vmlinuz"),
        );
        assert_eq!(config["cpus"].as_u64(), Some(2));
        assert_eq!(config["memory_mb"].as_u64(), Some(1024));
        assert_eq!(config["subnet_base_octet"].as_u64(), Some(99));
        // r21-A1: user_id MUST appear in the restore-path Config so the
        // driver's per-user-home allow-list accepts the disk path.
        // This assertion is what was missing and masked the gap.
        assert_eq!(
            config["user_id"].as_str(),
            Some("usr_alice"),
            "restore-path ChPlugin Config MUST carry user_id — driver \
             v8 allow-list rejects home.img without it (r21-A1)"
        );
        // C-7-LT-12a (smoke-r22): rootfs_source MUST appear in the
        // restore-path Config so the driver knows where to hardlink
        // the rootfs.img bytes from. Pre-fix the driver's rewriter
        // retargeted disks[0].path → <runDir>/rootfs.img and the
        // task_dir allow-list (C-7-LT-6) accepted it, but nothing
        // staged a real file at the destination — CH then aborted
        // at `VM Restore failed: DeviceManager(Disk(NotFound))`. The
        // source path matches cold-boot's materializeRootfs source
        // (`<runtime_dir>/rootfs-slim.img`).
        assert_eq!(
            config["rootfs_source"].as_str(),
            Some("/var/lib/zeroship/ch/rootfs-slim.img"),
            "restore-path ChPlugin Config MUST carry rootfs_source — \
             driver v12 hardlinks this into runDir/rootfs.img before \
             CH spawn (C-7-LT-12a)"
        );
    }

    /// C-7-LT-12a invariant: the restore-path `rootfs_source` field
    /// MUST point at the same on-disk artifact the cold-boot path's
    /// `materializeRootfs` copies from. Cold-boot emits the source
    /// implicitly through `ZSBX_ARTIFACT_DIR` (== `runtime_dir`) +
    /// the hard-coded `chRootfsSourceName = "rootfs-slim.img"` in the
    /// driver; the restore path emits the FULL path explicitly because
    /// the driver has no env-derived handle on the cold-boot artifact
    /// dir once the rewriter rewrites disks[0].path away from the
    /// source alloc. A mismatch would mean wake VMs boot off different
    /// rootfs bytes than fresh VMs — a class of bug the test pins
    /// preemptively.
    #[test]
    fn ch_plugin_restore_jobspec_rootfs_source_matches_cold_boot() {
        let cfg = fixture_cfg();
        let sid = Uuid::now_v7();
        let alloc_dir = Path::new("/var/zeroship/ch/snap/restore");
        let v = build_restore_nomad_job_json(
            "zsbx-restore-rootfs",
            &cfg,
            5,
            alloc_dir,
            sid,
            "usr_alice",
            1024,
            2.0,
            TaskDriverMode::ChPlugin,
        );
        let config = &v["Job"]["TaskGroups"][0]["Tasks"][0]["Config"];
        let env = &v["Job"]["TaskGroups"][0]["Tasks"][0]["Env"];

        // Cold-boot source: ZSBX_ARTIFACT_DIR (== runtime_dir) +
        // "/rootfs-slim.img" baked into the driver. The restore path
        // emits the same composition as an absolute path so the
        // driver-side hardlink/copy uses identical bytes.
        let artifact_dir = env["ZSBX_ARTIFACT_DIR"]
            .as_str()
            .expect("ZSBX_ARTIFACT_DIR must be emitted");
        let want = format!("{artifact_dir}/rootfs-slim.img");

        assert_eq!(
            config["rootfs_source"].as_str(),
            Some(want.as_str()),
            "rootfs_source MUST match cold-boot's materializeRootfs \
             source (ZSBX_ARTIFACT_DIR + /rootfs-slim.img); a mismatch \
             means wake VMs boot off different bytes than fresh VMs"
        );
    }

    /// raw_exec's `Config.command` field MUST NOT appear under
    /// ChPlugin. The Go driver's TaskConfig has no such field; an
    /// errant `command` either is silently ignored (best case) or
    /// fails HCL decode if the schema gets stricter. Pre-R12-I1 the
    /// wake-path was hardcoded raw_exec — this test would never have
    /// caught the split-brain.
    #[test]
    fn ch_plugin_restore_jobspec_omits_command() {
        let cfg = fixture_cfg();
        let sid = Uuid::now_v7();
        let alloc_dir = Path::new("/var/zeroship/ch/snap/restore");
        let v = build_restore_nomad_job_json(
            "zsbx-restore-no-cmd",
            &cfg,
            5,
            alloc_dir,
            sid,
            "usr_alice",
            1024,
            2.0,
            TaskDriverMode::ChPlugin,
        );
        let config = &v["Job"]["TaskGroups"][0]["Tasks"][0]["Config"];
        assert!(
            config["command"].is_null(),
            "ChPlugin wake-path Config must NOT carry the raw_exec \
             `command` field — Go TaskConfig has no such tag, got: {config:?}",
        );
    }

    /// R12-I1 defence-in-depth: even if the env flag is unset, an
    /// explicit `TaskDriverMode::ChPlugin` arg forces the ch branch.
    /// Pins the parameter wiring independently of
    /// `task_driver_mode_from_env`.
    #[test]
    fn ch_plugin_mode_arg_forces_ch_driver_regardless_of_env() {
        // No env lock needed — we don't read the env on this path.
        let cfg = fixture_cfg();
        let sid = Uuid::now_v7();
        let alloc_dir = Path::new("/var/zeroship/ch/snap/restore");
        let v = build_restore_nomad_job_json(
            "zsbx-restore-mode-arg",
            &cfg,
            5,
            alloc_dir,
            sid,
            "usr_alice",
            1024,
            2.0,
            TaskDriverMode::ChPlugin,
        );
        let task = &v["Job"]["TaskGroups"][0]["Tasks"][0];
        assert_eq!(task["Driver"], "ch");
    }

    /// R22-T1 / R21-API2 — ChPlugin Config field-list parity contract.
    ///
    /// Twice (r21-A1: `user_id`; C-7-LT-12a: `rootfs_source`) a field
    /// landed in cold-boot's Config emitter but missed the restore-path
    /// emitter, leading to cluster failures only caught by smoke-rN.
    ///
    /// This test asserts that the symmetric difference between the
    /// cold-boot and restore-path Config key sets is EXACTLY the two
    /// documented intentional divergences:
    ///
    ///   • `rootfs_source` — restore-path only: cold-boot relies on
    ///                       `ZSBX_ARTIFACT_DIR` env + the hard-coded
    ///                       `chRootfsSourceName` const in the driver;
    ///                       the restore emitter provides the full path
    ///                       explicitly because the rewriter has already
    ///                       moved disks[0].path away from the source
    ///                       alloc by the time the driver runs (C-7-LT-12a).
    ///
    /// Note: `pubkey_hex` and `restore_from` ARE present in both paths
    /// (with different values — empty string on cold-boot / restore-path
    /// respectively), so they are NOT in the symmetric difference.
    ///
    /// Any other asymmetry means a field was added to one emitter but
    /// not the other — the test names both sides in the failure message
    /// so the author knows exactly what to add.
    #[test]
    fn ch_plugin_config_field_list_parity() {
        use std::collections::HashSet;

        // ── cold-boot side ────────────────────────────────────────────
        // `build_nomad_job_json_with` takes a &SandboxConfig; use the
        // shared fixture (same runtime_dir / subnet_second_octet as
        // fixture_cfg() above so paths compare cleanly if ever needed).
        let cold_sandbox_cfg = crate::config::SandboxConfig::new_fixture();
        let cold_v = crate::backend::nomad_ch::build_nomad_job_json_with(
            "zsbx-parity-cold",
            &cold_sandbox_cfg,
            7,
            Path::new("/var/zeroship/ch/abc/workspace.img"),
            Path::new("/var/zeroship/ch/users/usr_alice/home.img"),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "usr_alice",
            "proj1",
            "abcdef0123456789abcdef0123456789",
            None, // cold-boot: no restore_from
            TaskDriverMode::ChPlugin,
        );
        let cold_config = &cold_v["Job"]["TaskGroups"][0]["Tasks"][0]["Config"];
        let cold_fields: HashSet<&str> = cold_config
            .as_object()
            .expect("cold-boot Config must be a JSON object")
            .keys()
            .map(|s| s.as_str())
            .collect();

        // ── restore-path side ─────────────────────────────────────────
        let restore_cfg = fixture_cfg();
        let sid = Uuid::now_v7();
        let alloc_dir = Path::new("/var/zeroship/ch/snap-deadbeef/restore");
        let restore_v = build_restore_nomad_job_json(
            "zsbx-parity-restore",
            &restore_cfg,
            7,
            alloc_dir,
            sid,
            "usr_alice",
            1024,
            2.0,
            TaskDriverMode::ChPlugin,
        );
        let restore_config = &restore_v["Job"]["TaskGroups"][0]["Tasks"][0]["Config"];
        let restore_fields: HashSet<&str> = restore_config
            .as_object()
            .expect("restore-path Config must be a JSON object")
            .keys()
            .map(|s| s.as_str())
            .collect();

        // ── parity assertion ──────────────────────────────────────────
        // The ONLY documented intentional divergence. Update this set
        // only when a new deliberate asymmetry is agreed and documented.
        // • rootfs_source — restore-path only (cold-boot uses
        //                   ZSBX_ARTIFACT_DIR env + driver const;
        //                   restore emits full path explicitly, C-7-LT-12a)
        //
        // pubkey_hex and restore_from are present in BOTH paths
        // (with semantically different values — empty string on the
        // non-applicable side), so they are NOT in the diff set.
        let expected_diff: HashSet<&str> = ["rootfs_source"].iter().copied().collect();

        let cold_only: HashSet<&str> = cold_fields.difference(&restore_fields).copied().collect();
        let restore_only: HashSet<&str> =
            restore_fields.difference(&cold_fields).copied().collect();
        let actual_diff: HashSet<&str> = cold_fields
            .symmetric_difference(&restore_fields)
            .copied()
            .collect();

        assert_eq!(
            actual_diff,
            expected_diff,
            "ChPlugin Config field-list drifted from the documented contract.\n\
             cold-only (add to restore emitter?):   {cold_only:?}\n\
             restore-only (add to cold emitter?):   {restore_only:?}\n\
             expected symmetric diff:               {expected_diff:?}\n\
             \n\
             Only {{\"rootfs_source\"}} is the documented intentional divergence.\n\
             See R22-T1 / R21-API2 for the contract history.",
        );
    }
}

// Silence unused-Arc warning if no caller imports the alias.
#[allow(dead_code)]
fn _arc_anchor() -> Option<Arc<()>> {
    None
}
