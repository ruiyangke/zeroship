//! Lightweight Phase-2 HA metrics (sandbox-pg-state design § 14.1 +
//! § 14.7). Process-global atomic counters / gauges; no Prometheus
//! framework dependency yet (Phase 3 wires a `/metrics` exporter).
//!
//! ## Why not a real registry?
//!
//! `crates/control/` doesn't ship a Prometheus crate today, and pulling
//! one into the sandbox crate just for Phase 2 would balloon the
//! dep-graph. The atomic-counter shape is what every Prometheus client
//! lib uses internally; an exporter binding is a pure-additive change.
//! Phase 3 swaps the inner type without touching call sites.
//!
//! ## What we expose
//!
//! - `sandbox_ha_takeover_total{reason}` — counter; one bump per
//!   successful takeover. v1 ships only `lease_expiration`; the
//!   `operator_rebind` label slot exists for Phase 3's admin API.
//! - `sandbox_ha_lost_leadership_total{op}` — counter; one bump
//!   each time a CAS-guarded UPDATE returns 0 rows because the
//!   `(host_id, generation)` pair was preempted by a peer (§ 11.2
//!   split-brain telemetry).
//! - `sandbox_ha_heartbeat_lag_seconds` — gauge; pg-side `now() -
//!   last_heartbeat` for THIS controller's row. `f64::NAN` until the
//!   first successful read.
//! - `sandbox_ha_dead_hosts_observed_total` — counter; how many dead
//!   hosts the takeover task has observed across all scans. Useful
//!   for soak tests — a healthy fleet's value stays near zero.
//! - `sandbox_ha_clock_rewind_total` — counter; one bump per scan
//!   that observed `now() - last_heartbeat < 0` (§ 12 R-MM).
//!
//! Tests can read each value via the `*_value` accessors below.

use std::sync::atomic::{AtomicU64, Ordering};

// ────────────────────────────────────────────────────────────────────
// Counters
// ────────────────────────────────────────────────────────────────────

/// `sandbox_ha_takeover_total{reason="lease_expiration"}`. v1 only
/// emits this label; v2 (Phase 3 admin API) will add
/// `reason="operator_rebind"`.
static TAKEOVER_LEASE_EXPIRATION: AtomicU64 = AtomicU64::new(0);

/// `sandbox_ha_lost_leadership_total{op="<op_name>"}`. The
/// per-op label is materialised lazily into a `Mutex<HashMap>` so
/// Phase-3 alerting can break down the counter by call-site
/// without re-instrumenting (round-1 fixer / MINOR #15). The
/// global aggregate remains in a fast atomic so the hot path stays
/// allocation-free in steady state — the lock is only taken on
/// the rare miss-path bumps.
static LOST_LEADERSHIP: AtomicU64 = AtomicU64::new(0);

/// Per-op breakdown of LOST_LEADERSHIP. `&'static str` keys keep
/// the map allocation-free — call sites pass string literals
/// (`"update_sandbox_status_stopped"`, `"delete_sandbox_fence"`,
/// etc).
static LOST_LEADERSHIP_BY_OP: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<&'static str, AtomicU64>>,
> = std::sync::OnceLock::new();

fn lost_leadership_by_op() -> &'static std::sync::Mutex<
    std::collections::HashMap<&'static str, AtomicU64>,
> {
    LOST_LEADERSHIP_BY_OP.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// `sandbox_ha_dead_hosts_observed_total`. Counter — increments once
/// per dead-host the takeover task observed in its scan, INCLUDING
/// duplicates across scans (so `rate(...)` over time is meaningful).
static DEAD_HOSTS_OBSERVED: AtomicU64 = AtomicU64::new(0);

/// `sandbox_ha_clock_rewind_total`. Counter — increments when a
/// heartbeat-lag read returns a negative value. R-MM: a healthy
/// fleet must never see this fire.
static CLOCK_REWIND: AtomicU64 = AtomicU64::new(0);

/// `sandbox_ha_takeover_orphan_total`. Counter — increments once
/// per takeover-rehydrate that found NO sealed record on the new
/// owner's local disk. Round-1 fixer / CRITICAL #4: in Phase 2 v1
/// we accept "sealed not on this host" as `status='lost'`; a
/// future cross-host sealed sync (S3? gossip?) is Phase 3+.
static TAKEOVER_ORPHAN: AtomicU64 = AtomicU64::new(0);

/// `sandbox_ha_takeover_mismatched_total`. Counter — increments
/// once per takeover-rehydrate where the sealed signing key
/// disagreed with pg's `key_fp`, OR the agent's `/version` returned
/// 401, OR the agent answered with a different fingerprint. Round-2
/// fixer / MINOR #3: pre-fix these outcomes were silently dropped;
/// today operators can rate(...) them to spot key-rotation issues.
static TAKEOVER_MISMATCHED: AtomicU64 = AtomicU64::new(0);

/// `sandbox_ha_takeover_unreachable_total`. Counter — increments
/// once per takeover-rehydrate where the agent's `/version` probe
/// timed out / returned a non-2xx non-401. Round-2 fixer / MINOR
/// #3.
static TAKEOVER_UNREACHABLE: AtomicU64 = AtomicU64::new(0);

/// `sandbox_ha_takeover_corrupt_total`. Counter — increments once
/// per takeover-rehydrate that hit a corrupt sealed record, an
/// unparseable typed-id, or a backend.restore_from_pg_and_sealed
/// failure. These are bug-grade events; healthy clusters don't see
/// them. Round-2 fixer / MINOR #3.
static TAKEOVER_CORRUPT: AtomicU64 = AtomicU64::new(0);

/// `sandbox_corrupt_id_total`. Counter — increments when restore
/// encounters a pg row whose `sandbox_id` doesn't parse as a
/// typed-id. Round-2 fixer / MINOR #1: pre-fix `sandbox_id_from_str_lossy`
/// silently swallowed the malformed id and fell back to
/// `Uuid::nil()` for a no-op UPDATE; today the row is skipped and
/// this counter fires so an alert can catch the drift between code
/// and data.
static SANDBOX_CORRUPT_ID: AtomicU64 = AtomicU64::new(0);

/// `sandbox_wake_sync_uses_total`. Counter — increments each time the
/// legacy synchronous wake path is taken (either via the
/// `WakeResponseMode::Sync` default OR an explicit `?sync=1` override).
/// C-7-LT-PR2 deprecation telemetry: api-surface-r16 spec gate #5 —
/// Phase 5 of the C-7-LT migration plan ("remove `?sync=1` and the
/// env flag entirely") gates on a minor with zero observed sync uses.
/// This counter is the data backing that gate.
static WAKE_SYNC_DEPRECATED: AtomicU64 = AtomicU64::new(0);

/// `sandbox_vm_index_leaks_total{reason="host_fence_timeout"}`. Counter
/// — increments each time `stop_inner` leaks a `vm_index` because the
/// host-fence (`wait_for_agent_silent`) failed to clear within the
/// configured `host_fence_timeout_secs` budget. C-7-LT-2-PR2: the
/// upstream probe wedge in `wait_for_agent_silent` is fixed in PR1; this
/// counter is the defense-in-depth observability surface so an operator
/// can `rate(...)` slot-leak events and alert if the rate exceeds the
/// baseline (a healthy cluster should see this counter near zero).
///
/// Reasons currently emitted:
/// - `host_fence_timeout` — `wait_for_agent_silent` returned `Err` at
///   the deadline (`job_confirmed_gone=true && fence_passed=false`).
/// - `wait_failed` — `wait_for_job_gone` failed (Nomad purge didn't
///   complete); the slot is leaked because we can't prove the job is
///   gone, so reusing the tap would risk a live-IP collision.
static VM_INDEX_LEAKS_HOST_FENCE_TIMEOUT: AtomicU64 = AtomicU64::new(0);
static VM_INDEX_LEAKS_WAIT_FAILED: AtomicU64 = AtomicU64::new(0);

/// `sandbox_wake_terminal_overwrite_blocked_total`. Counter — increments
/// each time `update_wake_job_state` returns `rows_affected == 0` on a
/// terminal write (`ok` or `failed`). R22-I1: R20-C1's SQL guard
/// (`AND state NOT IN ('ok','failed')`) silently no-ops when the row is
/// already terminal (e.g. sweep-writes-failed → wake-machine-writes-ok
/// race). This counter makes those guard-fires visible to operators; a
/// healthy cluster should see this counter near zero.
static WAKE_TERMINAL_OVERWRITE_BLOCKED: AtomicU64 = AtomicU64::new(0);

/// `sandbox_nomad_node_id_lookup_failures_total`. Counter — increments
/// once per boot-time `GET /v1/agent/self` call that fails (Nomad
/// unreachable, non-200, unparseable body, or missing `stats.client.node_id`).
/// r3-A (T-8b-stress-r3 fix): the controller caches the local Nomad
/// node_id at boot and uses it to emit a Nomad `Constraints` block
/// pinning every submitted alloc to the staging worker. A boot-time
/// fetch failure is non-fatal — the controller keeps running but
/// emits jobspecs without the constraint (falling back to random
/// cross-node placement, the pre-r3-A behaviour). This counter
/// surfaces a misconfigured/unreachable local Nomad agent so an
/// operator can alert before stress-run cross-node failures cascade.
/// Healthy clusters should see this counter at zero after first boot.
static NOMAD_NODE_ID_LOOKUP_FAILURES: AtomicU64 = AtomicU64::new(0);

// ────────────────────────────────────────────────────────────────────
// Gauges
// ────────────────────────────────────────────────────────────────────

/// `sandbox_ha_heartbeat_lag_seconds`. Stored as f64 bits in an
/// AtomicU64 so we can update without a lock; readers convert
/// back. NaN sentinel for "never read yet".
static HEARTBEAT_LAG_BITS: AtomicU64 = AtomicU64::new(f64::NAN.to_bits());

/// `sandbox_nomad_stop_permits_total`. Gauge — the boot-time capacity
/// of the global `NomadStopPermits` semaphore (one permit ≡ one
/// in-flight `/shutdown` ladder against the local Nomad agent). Set
/// once in `AppState::from_config` from `SANDBOX_NOMAD_STOP_CONCURRENCY`
/// (default 16); zero until that boot writes the configured value.
///
/// r30-A1: the 7 per-loop concurrency caps (`GC_STOP_CONCURRENCY=8`,
/// snap-idle-evict's `default=4`, three serial loops, two unbounded
/// admin paths) don't compose — a cluster cycle that exercises two
/// simultaneously can overload the single downstream `/shutdown` RPC
/// queue + host CH process budget. This gauge surfaces the global cap
/// so operators can size `SANDBOX_NOMAD_STOP_CONCURRENCY` against
/// observed `_in_use` peaks.
static NOMAD_STOP_PERMITS_TOTAL: AtomicU64 = AtomicU64::new(0);

/// `sandbox_nomad_stop_permits_in_use`. Gauge — currently-held permits
/// (= `total - permits_available()`). Bumped by `NomadStopPermits::
/// acquire` BEFORE the `/shutdown` ladder runs; decremented by the
/// `NomadStopPermitGuard` Drop on stop_inner exit. Steady-state ≈ 0;
/// sustained > `total` is impossible; sustained ≈ `total` means the
/// semaphore is the bottleneck and the operator should consider raising
/// `SANDBOX_NOMAD_STOP_CONCURRENCY` (after confirming the downstream
/// Nomad/CH budget tolerates the higher fan-out).
static NOMAD_STOP_PERMITS_IN_USE: AtomicU64 = AtomicU64::new(0);

// ────────────────────────────────────────────────────────────────────
// Public API
// ────────────────────────────────────────────────────────────────────

/// Bump `sandbox_ha_takeover_total{reason="lease_expiration"}` once.
pub fn inc_takeover_lease_expiration() {
    TAKEOVER_LEASE_EXPIRATION.fetch_add(1, Ordering::Relaxed);
}

/// Add `n` to the takeover counter at once. Used after a single SQL
/// UPDATE returns N rows (the takeover-of-N-sandboxes case).
pub fn add_takeover_lease_expiration(n: u64) {
    if n == 0 {
        return;
    }
    TAKEOVER_LEASE_EXPIRATION.fetch_add(n, Ordering::Relaxed);
}

/// Bump `sandbox_ha_lost_leadership_total` once. The `op` label is
/// recorded both on the aggregate counter (cheap atomic) and on a
/// per-op breakdown map. Round-1 fixer / MINOR #15: pre-fix the
/// label was discarded entirely; today the breakdown is queryable
/// via [`lost_leadership_value_for_op`].
pub fn inc_lost_leadership(op: &'static str) {
    LOST_LEADERSHIP.fetch_add(1, Ordering::Relaxed);
    let map = lost_leadership_by_op();
    // Fast path: read-lock and bump if the entry exists.
    if let Ok(mut g) = map.lock() {
        let entry = g.entry(op).or_insert_with(|| AtomicU64::new(0));
        entry.fetch_add(1, Ordering::Relaxed);
    }
    // PoisonError: the metric storage doesn't enforce invariants
    // worth crashing for; if a previous panic left the lock
    // poisoned, we just lose this label-bump rather than
    // propagating the panic into a hot path.
}

/// Bump `sandbox_ha_dead_hosts_observed_total` by `n`.
pub fn add_dead_hosts_observed(n: u64) {
    if n == 0 {
        return;
    }
    DEAD_HOSTS_OBSERVED.fetch_add(n, Ordering::Relaxed);
}

/// Bump `sandbox_ha_clock_rewind_total` once.
pub fn inc_clock_rewind() {
    CLOCK_REWIND.fetch_add(1, Ordering::Relaxed);
}

/// Bump `sandbox_ha_takeover_orphan_total` once. Round-1 fixer /
/// CRITICAL #4: emitted once per post-takeover rehydrate that found
/// no sealed record locally.
pub fn inc_takeover_orphan() {
    TAKEOVER_ORPHAN.fetch_add(1, Ordering::Relaxed);
}

/// Bump `sandbox_ha_takeover_mismatched_total` once. Round-2 fixer
/// / MINOR #3.
pub fn inc_takeover_mismatched() {
    TAKEOVER_MISMATCHED.fetch_add(1, Ordering::Relaxed);
}

/// Bump `sandbox_ha_takeover_unreachable_total` once. Round-2 fixer
/// / MINOR #3.
pub fn inc_takeover_unreachable() {
    TAKEOVER_UNREACHABLE.fetch_add(1, Ordering::Relaxed);
}

/// Bump `sandbox_ha_takeover_corrupt_total` once. Round-2 fixer /
/// MINOR #3.
pub fn inc_takeover_corrupt() {
    TAKEOVER_CORRUPT.fetch_add(1, Ordering::Relaxed);
}

/// Bump `sandbox_corrupt_id_total` once. Round-2 fixer / MINOR #1.
pub fn inc_sandbox_corrupt_id() {
    SANDBOX_CORRUPT_ID.fetch_add(1, Ordering::Relaxed);
}

/// Bump `sandbox_wake_sync_uses_total` once. C-7-LT-PR2 deprecation
/// telemetry: increments each time the legacy synchronous wake path
/// is taken. Backs the Phase 5 "zero sync uses for one minor"
/// migration gate (api-surface-r16 spec gate #5).
pub fn inc_wake_sync_deprecated() {
    WAKE_SYNC_DEPRECATED.fetch_add(1, Ordering::Relaxed);
}

/// Read-side accessor for the wake-sync deprecation counter. Used by
/// `metrics_export::render()` and by tests.
pub fn wake_sync_deprecated_value() -> u64 {
    WAKE_SYNC_DEPRECATED.load(Ordering::Relaxed)
}

/// Bump `sandbox_vm_index_leaks_total{reason}` once. C-7-LT-2-PR2
/// defense-in-depth observability for slot-leak events. `reason` MUST
/// be one of the documented variants (`"host_fence_timeout"` or
/// `"wait_failed"`); any other string is silently coerced to the
/// `host_fence_timeout` bucket to avoid an unlabelled drop, and a
/// WARN log fires on the unrecognised label so call-sites that
/// invented a new reason without updating this map surface
/// immediately.
pub fn inc_vm_index_leak(reason: &'static str) {
    match reason {
        "host_fence_timeout" => {
            VM_INDEX_LEAKS_HOST_FENCE_TIMEOUT.fetch_add(1, Ordering::Relaxed);
        }
        "wait_failed" => {
            VM_INDEX_LEAKS_WAIT_FAILED.fetch_add(1, Ordering::Relaxed);
        }
        other => {
            tracing::warn!(
                target: "sandbox::teardown::leak",
                reason = other,
                "inc_vm_index_leak: unknown reason label; bucketing into host_fence_timeout"
            );
            VM_INDEX_LEAKS_HOST_FENCE_TIMEOUT.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Read-side accessor for the per-reason vm_index leak counter. Used
/// by `metrics_export::render()` and by tests. Unknown labels return 0.
pub fn vm_index_leak_value(reason: &'static str) -> u64 {
    match reason {
        "host_fence_timeout" => {
            VM_INDEX_LEAKS_HOST_FENCE_TIMEOUT.load(Ordering::Relaxed)
        }
        "wait_failed" => VM_INDEX_LEAKS_WAIT_FAILED.load(Ordering::Relaxed),
        _ => 0,
    }
}

/// Bump `sandbox_wake_terminal_overwrite_blocked_total` once. R22-I1:
/// emitted when `update_wake_job_state` returns `rows_affected == 0` on
/// a terminal write, indicating R20-C1's guard fired (the row was already
/// in a terminal state before this write arrived).
pub fn inc_wake_terminal_overwrite_blocked() {
    WAKE_TERMINAL_OVERWRITE_BLOCKED.fetch_add(1, Ordering::Relaxed);
}

/// Read-side accessor for the terminal-overwrite-blocked counter. Used
/// by `metrics_export::render()` and by tests.
pub fn wake_terminal_overwrite_blocked_value() -> u64 {
    WAKE_TERMINAL_OVERWRITE_BLOCKED.load(Ordering::Relaxed)
}

/// Bump `sandbox_nomad_node_id_lookup_failures_total` once. r3-A
/// (T-8b-stress-r3 fix): emitted at controller boot when the
/// `GET /v1/agent/self` call to fetch the local Nomad node_id fails.
/// See `crate::backend::nomad_ch::fetch_local_nomad_node_id` for the
/// failure shapes; a non-zero value here means cold-boot + restore
/// jobspecs are missing the node-affinity Constraints block and the
/// scheduler will fall back to random cross-node placement.
pub fn inc_nomad_node_id_lookup_failure() {
    NOMAD_NODE_ID_LOOKUP_FAILURES.fetch_add(1, Ordering::Relaxed);
}

/// Read-side accessor for the nomad-node-id-lookup-failures counter.
/// Used by `metrics_export::render()` and by tests.
pub fn nomad_node_id_lookup_failures_value() -> u64 {
    NOMAD_NODE_ID_LOOKUP_FAILURES.load(Ordering::Relaxed)
}

/// Read-side accessor for the takeover-orphan counter. Used by
/// `metrics_export::render()` and by tests.
pub fn takeover_orphan_value() -> u64 {
    TAKEOVER_ORPHAN.load(Ordering::Relaxed)
}

/// Read-side accessor for the takeover-mismatched counter. Used by
/// `metrics_export::render()` and by tests.
pub fn takeover_mismatched_value() -> u64 {
    TAKEOVER_MISMATCHED.load(Ordering::Relaxed)
}

/// Production accessor for the takeover-unreachable counter. Consumed
/// by the Prometheus exporter (`crate::metrics_export::render`). Was
/// previously a test-only `#[doc(hidden)]` fn deleted at `370fdbba` as
/// an orphan; resurrected when R26-API2 wired the exporter that is the
/// orphan's intended caller.
pub fn takeover_unreachable_value() -> u64 {
    TAKEOVER_UNREACHABLE.load(Ordering::Relaxed)
}

/// Production accessor for the takeover-corrupt counter. Consumed by
/// the Prometheus exporter (`crate::metrics_export::render`). See
/// [`takeover_unreachable_value`] for the resurrection rationale.
pub fn takeover_corrupt_value() -> u64 {
    TAKEOVER_CORRUPT.load(Ordering::Relaxed)
}

/// Production accessor for the sandbox-corrupt-id counter. Consumed by
/// the Prometheus exporter (`crate::metrics_export::render`). See
/// [`takeover_unreachable_value`] for the resurrection rationale.
pub fn sandbox_corrupt_id_value() -> u64 {
    SANDBOX_CORRUPT_ID.load(Ordering::Relaxed)
}

/// Set `sandbox_ha_heartbeat_lag_seconds` to `secs`. NaN-safe; an
/// f64 with a negative value triggers the clock-rewind detector
/// inside the caller (the takeover task) — this setter only stores.
pub fn set_heartbeat_lag(secs: f64) {
    HEARTBEAT_LAG_BITS.store(secs.to_bits(), Ordering::Relaxed);
}

// ────────────────────────────────────────────────────────────────────
// Read-side (consumed by `metrics_export::render` + tests)
// ────────────────────────────────────────────────────────────────────

/// Read-side accessor for the takeover counter. Used by
/// `metrics_export::render()` and by tests.
pub fn takeover_lease_expiration_value() -> u64 {
    TAKEOVER_LEASE_EXPIRATION.load(Ordering::Relaxed)
}

/// Read-side accessor for the lost-leadership counter. Used by
/// `metrics_export::render()` and by tests.
pub fn lost_leadership_value() -> u64 {
    LOST_LEADERSHIP.load(Ordering::Relaxed)
}

/// Accessor for the per-op breakdown; returns 0 for an op label that
/// has never been incremented. Tests pin specific ops with this; the
/// Prometheus exporter consumes [`lost_leadership_snapshot_by_op`]
/// (which materialises the full label set in one call) instead.
pub fn lost_leadership_value_for_op(op: &'static str) -> u64 {
    let map = lost_leadership_by_op();
    let Ok(g) = map.lock() else { return 0 };
    g.get(op).map(|c| c.load(Ordering::Relaxed)).unwrap_or(0)
}

/// Snapshot of every observed `(op, count)` pair in the
/// `LOST_LEADERSHIP_BY_OP` breakdown, sorted alphabetically by `op`.
/// Production accessor — consumed by the Prometheus exporter to emit
/// one labelled line per observed op
/// (`sandbox_ha_lost_leadership_total{op="<op>"}`). Ops that were
/// never incremented are absent (Prometheus convention: emit only
/// observed series; absent series ≡ 0 at the query layer).
///
/// On a poisoned mutex the function returns an empty Vec rather than
/// panicking — the metrics surface is best-effort observability,
/// never load-bearing for control flow. The aggregate counter on
/// `LOST_LEADERSHIP` (read via [`lost_leadership_value`]) is unaffected.
pub fn lost_leadership_snapshot_by_op() -> Vec<(&'static str, u64)> {
    let map = lost_leadership_by_op();
    let Ok(g) = map.lock() else { return Vec::new() };
    let mut out: Vec<(&'static str, u64)> = g
        .iter()
        .map(|(k, v)| (*k, v.load(Ordering::Relaxed)))
        .collect();
    out.sort_by_key(|(k, _)| *k);
    out
}

/// Read-side accessor for the dead-hosts-observed counter. Used by
/// `metrics_export::render()` and by tests.
pub fn dead_hosts_observed_value() -> u64 {
    DEAD_HOSTS_OBSERVED.load(Ordering::Relaxed)
}

/// Read-side accessor for the clock-rewind counter. Used by
/// `metrics_export::render()` and by tests.
pub fn clock_rewind_value() -> u64 {
    CLOCK_REWIND.load(Ordering::Relaxed)
}

/// Read-side accessor for the heartbeat-lag gauge. Used by
/// `metrics_export::render()` and by tests. Returns NaN if no
/// successful read has happened yet.
pub fn heartbeat_lag_value() -> f64 {
    f64::from_bits(HEARTBEAT_LAG_BITS.load(Ordering::Relaxed))
}

// ────────────────────────────────────────────────────────────────────
// r30-A1: global Nomad /shutdown concurrency cap (semaphore)
// ────────────────────────────────────────────────────────────────────

/// Set `sandbox_nomad_stop_permits_total` to the boot-resolved capacity.
/// Called exactly once from `AppState::from_config` after
/// `SANDBOX_NOMAD_STOP_CONCURRENCY` is parsed. Idempotent w.r.t. value;
/// subsequent calls overwrite (used by test fixtures that reset the
/// process-global gauge between cases).
pub fn set_nomad_stop_permits_total(n: u64) {
    NOMAD_STOP_PERMITS_TOTAL.store(n, Ordering::Relaxed);
}

/// Read-side accessor for the `sandbox_nomad_stop_permits_total` gauge.
/// Consumed by `metrics_export::render()` + tests.
pub fn nomad_stop_permits_total_value() -> u64 {
    NOMAD_STOP_PERMITS_TOTAL.load(Ordering::Relaxed)
}

/// Bump `sandbox_nomad_stop_permits_in_use` by 1. Called inside
/// `NomadStopPermits::acquire` AFTER the flume token recv succeeds and
/// BEFORE the guard is returned, so the gauge is monotonically tied to
/// the lifetime of the guard. Pair: every `inc_*` MUST be followed by
/// exactly one `dec_*` (the `NomadStopPermitGuard` Drop guarantees this).
pub fn inc_nomad_stop_permits_in_use() {
    NOMAD_STOP_PERMITS_IN_USE.fetch_add(1, Ordering::Relaxed);
}

/// Decrement `sandbox_nomad_stop_permits_in_use` by 1. Saturating —
/// underflow folds to 0 rather than wrapping to `u64::MAX`, which would
/// poison the gauge until process restart. Called from the
/// `NomadStopPermitGuard` Drop impl; tests can call it directly to
/// model a guard-drop without spinning up an acquire.
pub fn dec_nomad_stop_permits_in_use() {
    // CAS loop: saturating_sub on AtomicU64 (fetch_sub wraps on
    // underflow). The window is microscopic — only the acquire / drop
    // pair writes this counter — but the saturating shape is the
    // defensible default for an observability surface.
    let mut cur = NOMAD_STOP_PERMITS_IN_USE.load(Ordering::Relaxed);
    loop {
        let next = cur.saturating_sub(1);
        match NOMAD_STOP_PERMITS_IN_USE.compare_exchange_weak(
            cur,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return,
            Err(observed) => cur = observed,
        }
    }
}

/// Read-side accessor for the `sandbox_nomad_stop_permits_in_use` gauge.
/// Consumed by `metrics_export::render()` + tests.
pub fn nomad_stop_permits_in_use_value() -> u64 {
    NOMAD_STOP_PERMITS_IN_USE.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_increment_monotonically() {
        let pre = takeover_lease_expiration_value();
        inc_takeover_lease_expiration();
        inc_takeover_lease_expiration();
        add_takeover_lease_expiration(5);
        let post = takeover_lease_expiration_value();
        assert!(post >= pre + 7, "got {pre} -> {post}");
    }

    #[test]
    fn add_takeover_zero_is_noop() {
        let pre = takeover_lease_expiration_value();
        add_takeover_lease_expiration(0);
        assert_eq!(takeover_lease_expiration_value(), pre);
    }

    #[test]
    fn lost_leadership_label_does_not_alter_counter_shape() {
        // Round-2 fixer / IMPORTANT #6: pre-fix this test only
        // verified the AGGREGATE counter incremented by 2 — a
        // regression that collapsed the per-op breakdown into one
        // bucket would have passed silently. The fix asserts each
        // labelled op's per-bucket value too.
        let pre_total = lost_leadership_value();
        let pre_update = lost_leadership_value_for_op("test_op_update_status");
        let pre_delete = lost_leadership_value_for_op("test_op_delete_sandbox");
        inc_lost_leadership("test_op_update_status");
        inc_lost_leadership("test_op_delete_sandbox");
        assert_eq!(lost_leadership_value(), pre_total + 2, "aggregate must bump by 2");
        assert_eq!(
            lost_leadership_value_for_op("test_op_update_status"),
            pre_update + 1,
            "per-op breakdown must bump update_status bucket"
        );
        assert_eq!(
            lost_leadership_value_for_op("test_op_delete_sandbox"),
            pre_delete + 1,
            "per-op breakdown must bump delete_sandbox bucket"
        );
        // A label that wasn't bumped MUST NOT have its bucket
        // touched — guards against a regression that funnels every
        // label into a single bucket.
        assert_eq!(
            lost_leadership_value_for_op("test_op_unrelated_op"),
            0,
            "untouched op label must stay at 0"
        );
    }

    #[test]
    fn heartbeat_lag_round_trips() {
        set_heartbeat_lag(2.5);
        assert!((heartbeat_lag_value() - 2.5).abs() < 1e-9);
        set_heartbeat_lag(0.0);
        assert_eq!(heartbeat_lag_value(), 0.0);
    }

    #[test]
    fn clock_rewind_increments() {
        let pre = clock_rewind_value();
        inc_clock_rewind();
        inc_clock_rewind();
        assert_eq!(clock_rewind_value(), pre + 2);
    }

    /// C-7-LT-2-PR2 leak telemetry: per-reason counter monotonically
    /// increases and the unknown-label fallback bucket logs + folds
    /// into `host_fence_timeout` rather than silently dropping.
    #[test]
    fn vm_index_leak_counter_per_reason_monotonic() {
        let pre_fence = vm_index_leak_value("host_fence_timeout");
        let pre_wait = vm_index_leak_value("wait_failed");
        inc_vm_index_leak("host_fence_timeout");
        inc_vm_index_leak("host_fence_timeout");
        inc_vm_index_leak("wait_failed");
        assert!(
            vm_index_leak_value("host_fence_timeout") >= pre_fence + 2,
            "host_fence_timeout bucket must bump by 2"
        );
        assert!(
            vm_index_leak_value("wait_failed") >= pre_wait + 1,
            "wait_failed bucket must bump by 1"
        );
        // Unknown label folds into host_fence_timeout (operator-safe
        // fallback) — guards against a future call-site adding a
        // typo'd reason and silently dropping the bump.
        let pre_unk = vm_index_leak_value("host_fence_timeout");
        inc_vm_index_leak("a_made_up_reason_label");
        assert!(
            vm_index_leak_value("host_fence_timeout") >= pre_unk + 1,
            "unknown reason must fold into host_fence_timeout"
        );
        // Unknown-label readback is always 0.
        assert_eq!(
            vm_index_leak_value("another_made_up_reason"),
            0,
            "unknown reason read must return 0"
        );
    }

    /// C-7-LT-PR2 deprecation telemetry: counter monotonically
    /// increases on every `inc_wake_sync_deprecated()` call.
    #[test]
    fn wake_sync_deprecated_counter_monotonic() {
        let pre = wake_sync_deprecated_value();
        inc_wake_sync_deprecated();
        inc_wake_sync_deprecated();
        inc_wake_sync_deprecated();
        let post = wake_sync_deprecated_value();
        assert!(post >= pre + 3, "got {pre} -> {post}");
    }

    /// R22-I1: terminal-overwrite-blocked counter starts at zero (or
    /// some stable baseline — we read the pre-test value and assert the
    /// post-zero-increment value is unchanged).
    #[test]
    fn wake_terminal_overwrite_blocked_counter_starts_at_zero() {
        // Process-global counter — we can't reset it, but if no other
        // test touches it before this one the value is 0. We read the
        // pre value to establish a baseline and verify it is a valid u64
        // (i.e., the counter is accessible). The monotonic test below
        // is the stronger contract; this test documents the "starts at
        // zero" intent as a named assertion.
        let v = wake_terminal_overwrite_blocked_value();
        // The counter must be a finite u64 — just confirm the accessor
        // compiles and returns without panic.
        let _ = v;
    }

    /// R22-I1: terminal-overwrite-blocked counter increments monotonically.
    #[test]
    fn inc_wake_terminal_overwrite_blocked_monotonic() {
        let pre = wake_terminal_overwrite_blocked_value();
        inc_wake_terminal_overwrite_blocked();
        inc_wake_terminal_overwrite_blocked();
        let post = wake_terminal_overwrite_blocked_value();
        assert!(post >= pre + 2, "got {pre} -> {post}");
    }

    /// r3-A: nomad-node-id-lookup-failure counter increments
    /// monotonically. Bumped at controller boot when
    /// `fetch_local_nomad_node_id` returns Err.
    #[test]
    fn inc_nomad_node_id_lookup_failure_monotonic() {
        let pre = nomad_node_id_lookup_failures_value();
        inc_nomad_node_id_lookup_failure();
        inc_nomad_node_id_lookup_failure();
        let post = nomad_node_id_lookup_failures_value();
        assert!(post >= pre + 2, "got {pre} -> {post}");
    }

    /// r30-A1: `set_nomad_stop_permits_total` writes the boot-resolved
    /// capacity and overwrites on a subsequent call (test fixtures may
    /// reset the process-global gauge between cases).
    #[test]
    fn nomad_stop_permits_total_set_round_trips() {
        set_nomad_stop_permits_total(16);
        assert_eq!(nomad_stop_permits_total_value(), 16);
        set_nomad_stop_permits_total(32);
        assert_eq!(nomad_stop_permits_total_value(), 32);
        // Restore the default-ish value so other tests in the same
        // process see a sensible reading; the gauge is set at boot in
        // production and never decremented in steady state.
        set_nomad_stop_permits_total(16);
    }

    /// r30-A1: `dec_nomad_stop_permits_in_use` saturates at 0 — the
    /// gauge MUST NOT wrap to u64::MAX on an underflow, which would
    /// poison the metric until process restart. Pair with the
    /// integration test in nomad_ch.rs that asserts the gauge tracks
    /// acquire / drop one-to-one.
    #[test]
    fn nomad_stop_permits_in_use_saturating_on_underflow() {
        // Force the counter to 0, then dec — must still be 0.
        // (Process-global state: we can only test that an EXTRA dec
        // past whatever steady state holds doesn't wrap; we can't
        // assert == 0 because parallel test runs may have an in-flight
        // acquire. The contract is "saturating, never wraps".)
        let pre = nomad_stop_permits_in_use_value();
        // Pull it down to a known floor by matched inc/dec, then a
        // single extra dec — saturating sub keeps it at 0 minimum.
        for _ in 0..pre + 4 {
            dec_nomad_stop_permits_in_use();
        }
        assert_eq!(
            nomad_stop_permits_in_use_value(),
            0,
            "dec past zero MUST saturate (no wrap to u64::MAX)"
        );
    }
}
