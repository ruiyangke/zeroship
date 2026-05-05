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

// ────────────────────────────────────────────────────────────────────
// Gauges
// ────────────────────────────────────────────────────────────────────

/// `sandbox_ha_heartbeat_lag_seconds`. Stored as f64 bits in an
/// AtomicU64 so we can update without a lock; readers convert
/// back. NaN sentinel for "never read yet".
static HEARTBEAT_LAG_BITS: AtomicU64 = AtomicU64::new(f64::NAN.to_bits());

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

/// Test-only accessor for the takeover-orphan counter.
#[doc(hidden)]
pub fn takeover_orphan_value() -> u64 {
    TAKEOVER_ORPHAN.load(Ordering::Relaxed)
}

/// Set `sandbox_ha_heartbeat_lag_seconds` to `secs`. NaN-safe; an
/// f64 with a negative value triggers the clock-rewind detector
/// inside the caller (the takeover task) — this setter only stores.
pub fn set_heartbeat_lag(secs: f64) {
    HEARTBEAT_LAG_BITS.store(secs.to_bits(), Ordering::Relaxed);
}

// ────────────────────────────────────────────────────────────────────
// Read-side (tests + future /metrics exporter)
// ────────────────────────────────────────────────────────────────────

/// Test-only accessor for the takeover counter.
#[doc(hidden)]
pub fn takeover_lease_expiration_value() -> u64 {
    TAKEOVER_LEASE_EXPIRATION.load(Ordering::Relaxed)
}

/// Test-only accessor for the lost-leadership counter.
#[doc(hidden)]
pub fn lost_leadership_value() -> u64 {
    LOST_LEADERSHIP.load(Ordering::Relaxed)
}

/// Test-only / future-exporter accessor for the per-op breakdown.
/// Returns 0 for an op label that has never been incremented.
#[doc(hidden)]
pub fn lost_leadership_value_for_op(op: &'static str) -> u64 {
    let map = lost_leadership_by_op();
    let Ok(g) = map.lock() else { return 0 };
    g.get(op).map(|c| c.load(Ordering::Relaxed)).unwrap_or(0)
}

/// Test-only accessor for the dead-hosts-observed counter.
#[doc(hidden)]
pub fn dead_hosts_observed_value() -> u64 {
    DEAD_HOSTS_OBSERVED.load(Ordering::Relaxed)
}

/// Test-only accessor for the clock-rewind counter.
#[doc(hidden)]
pub fn clock_rewind_value() -> u64 {
    CLOCK_REWIND.load(Ordering::Relaxed)
}

/// Test-only accessor for the heartbeat-lag gauge. Returns NaN if
/// no successful read has happened yet.
#[doc(hidden)]
pub fn heartbeat_lag_value() -> f64 {
    f64::from_bits(HEARTBEAT_LAG_BITS.load(Ordering::Relaxed))
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
        let pre = lost_leadership_value();
        inc_lost_leadership("update_status");
        inc_lost_leadership("delete_sandbox");
        assert_eq!(lost_leadership_value(), pre + 2);
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
}
