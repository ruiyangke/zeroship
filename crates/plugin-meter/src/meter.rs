//! Per-worker usage meter — atomic per-`(app_id, metric)` counters.
//!
//! Ported from the dead `crates/platform/src/metering/meter.rs` (the
//! tokio-era monolith): the atomic-counter + snapshot + reset logic
//! survives; the tokio flush task, the plan/quota coupling, and the
//! `CounterRegistry` indirection are dropped. This is a small,
//! runtime-agnostic core: `increment` bumps a counter, `drain` takes a
//! consistent snapshot AND resets to zero (reset-after-snapshot), so the
//! flush task can POST the snapshot and, on ack, the next interval starts
//! from zero. On a flush FAILURE the caller carries the snapshot forward
//! by re-merging it (see [`Meter::merge`]), so nothing is lost.
//!
//! One `Meter` is shared (via `Arc`) across every worker thread in a
//! process. `env.meter.increment` bumps it; the platform auto-counters
//! (requests today; cpu/wall/egress/ingress are wired as the worker grows
//! those numbers) feed the same instance. A single flush task per process
//! drains it and emits one `UsageReport` with the next monotonic sequence.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

use uuid::Uuid;
use zeroship_core::types::{AppUsage, UsageReport};

/// The five fixed platform counters, addressed by name from
/// `env.meter.increment` callers too (so an SDK that increments
/// `"requests"` lands in the fixed field, not `custom`). Anything not in
/// this set is a `custom` metric.
pub const FIXED_METRICS: [&str; 5] =
    ["requests", "cpu_us", "wall_us", "egress_bytes", "ingress_bytes"];

/// Is `metric` one of the five reserved platform counter names?
#[must_use]
pub fn is_fixed_metric(metric: &str) -> bool {
    FIXED_METRICS.contains(&metric)
}

/// Per-app atomic counters. The five fixed counters are dedicated atomics
/// (O(1), no map lookup on the hot path); `custom` is a locked map keyed by
/// metric name. A fresh `AppCounters` is all-zero.
#[derive(Debug, Default)]
struct AppCounters {
    requests: AtomicU64,
    cpu_us: AtomicU64,
    wall_us: AtomicU64,
    egress_bytes: AtomicU64,
    ingress_bytes: AtomicU64,
    /// SDK-defined metrics. `Mutex` (not per-key atomics) because the key
    /// set is open and small; contention is negligible at flush cadence.
    custom: Mutex<HashMap<String, u64>>,
}

impl AppCounters {
    /// Bump a counter by `n` and return the metric's new running total
    /// (this period, pre-drain). A fixed-metric name routes to its atomic;
    /// any other name lands in `custom`. Relaxed ordering is fine — the
    /// only cross-thread synchronization that matters is the drain, which
    /// takes the outer `RwLock` write guard.
    fn add(&self, metric: &str, n: u64) -> u64 {
        match metric {
            "requests" => self.requests.fetch_add(n, Ordering::Relaxed) + n,
            "cpu_us" => self.cpu_us.fetch_add(n, Ordering::Relaxed) + n,
            "wall_us" => self.wall_us.fetch_add(n, Ordering::Relaxed) + n,
            "egress_bytes" => self.egress_bytes.fetch_add(n, Ordering::Relaxed) + n,
            "ingress_bytes" => self.ingress_bytes.fetch_add(n, Ordering::Relaxed) + n,
            other => {
                let mut c = self.custom.lock().unwrap();
                let slot = c.entry(other.to_string()).or_insert(0);
                *slot += n;
                *slot
            }
        }
    }

    /// Take a snapshot of current values AND reset them to zero in one shot
    /// (`swap`). Returns `None` when every counter is zero (nothing to
    /// flush). `swap(0)` is the atomic read-and-reset that makes the
    /// hot-path increments and the drain race-free without a lock on the
    /// fixed counters.
    fn drain(&self) -> Option<AppUsage> {
        let requests = self.requests.swap(0, Ordering::Relaxed);
        let cpu_us = self.cpu_us.swap(0, Ordering::Relaxed);
        let wall_us = self.wall_us.swap(0, Ordering::Relaxed);
        let egress_bytes = self.egress_bytes.swap(0, Ordering::Relaxed);
        let ingress_bytes = self.ingress_bytes.swap(0, Ordering::Relaxed);
        let custom = std::mem::take(&mut *self.custom.lock().unwrap());

        if requests == 0
            && cpu_us == 0
            && wall_us == 0
            && egress_bytes == 0
            && ingress_bytes == 0
            && custom.is_empty()
        {
            return None;
        }
        Some(AppUsage {
            requests,
            cpu_us,
            wall_us,
            egress_bytes,
            ingress_bytes,
            custom,
        })
    }
}

/// Process-wide usage meter. Cheap to clone behind an `Arc`. Lock-free on
/// the increment fast path once an app's `AppCounters` exists; a brief
/// write lock only on first-touch per app.
#[derive(Debug, Default)]
pub struct Meter {
    apps: RwLock<HashMap<String, AppCounters>>,
}

impl Meter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Increment `metric` for `app_id` by `n`; returns the metric's new
    /// running total this period. Auto-vivifies the app's counter set on
    /// first touch. A `metric` matching one of the five fixed names lands
    /// in that fixed field; everything else is a custom metric.
    pub fn increment(&self, app_id: &str, metric: &str, n: u64) -> u64 {
        // Fast path: read lock, app already present.
        {
            let apps = self.apps.read().unwrap();
            if let Some(c) = apps.get(app_id) {
                return c.add(metric, n);
            }
        }
        // Slow path: write lock, insert-then-add.
        let mut apps = self.apps.write().unwrap();
        apps.entry(app_id.to_string()).or_default().add(metric, n)
    }

    /// Record one completed request's platform counters in a single call —
    /// the auto-counter hook the worker calls per dispatch.
    pub fn record_request(
        &self,
        app_id: &str,
        cpu_us: u64,
        wall_us: u64,
        egress_bytes: u64,
        ingress_bytes: u64,
    ) {
        self.increment(app_id, "requests", 1);
        if cpu_us > 0 {
            self.increment(app_id, "cpu_us", cpu_us);
        }
        if wall_us > 0 {
            self.increment(app_id, "wall_us", wall_us);
        }
        if egress_bytes > 0 {
            self.increment(app_id, "egress_bytes", egress_bytes);
        }
        if ingress_bytes > 0 {
            self.increment(app_id, "ingress_bytes", ingress_bytes);
        }
    }

    /// Drain every app's counters to a `{ app_id → AppUsage }` map, resetting
    /// each to zero. Apps with zero activity since the last drain are
    /// omitted. The returned map is empty when no app saw any traffic.
    ///
    /// Held under the write lock so no increment interleaves a partial
    /// drain (a fixed-counter `swap` plus a `custom` take must be atomic
    /// *per app* relative to that app's own increments). Cross-app drains
    /// are still independent.
    #[must_use]
    pub fn drain(&self) -> HashMap<Uuid, AppUsage> {
        let apps = self.apps.write().unwrap();
        let mut out = HashMap::new();
        for (app_id, counters) in apps.iter() {
            if let Some(usage) = counters.drain() {
                // app_id strings come from APP_ID env (a UUID). Skip a
                // malformed id rather than poison the whole flush.
                match Uuid::parse_str(app_id) {
                    Ok(id) => {
                        out.insert(id, usage);
                    }
                    Err(e) => {
                        tracing::warn!(app_id = %app_id, error = %e, "meter: drain skipped non-UUID app_id");
                    }
                }
            }
        }
        out
    }

    /// Re-merge a previously-drained snapshot back into the live counters.
    /// Used to CARRY FORWARD on a flush failure: the flush task drains,
    /// the POST fails, so the snapshot is merged back and retried next
    /// tick (at-least-once producer; control dedups on sequence).
    pub fn merge(&self, snapshot: &HashMap<Uuid, AppUsage>) {
        for (app_id, usage) in snapshot {
            let id = app_id.to_string();
            self.increment(&id, "requests", usage.requests);
            self.increment(&id, "cpu_us", usage.cpu_us);
            self.increment(&id, "wall_us", usage.wall_us);
            self.increment(&id, "egress_bytes", usage.egress_bytes);
            self.increment(&id, "ingress_bytes", usage.ingress_bytes);
            for (metric, &n) in &usage.custom {
                self.increment(&id, metric, n);
            }
        }
    }
}

/// Monotonic per-`worker_id` sequence source for `UsageReport`s. One per
/// worker process; the flush task bumps it once per report it builds.
#[derive(Debug)]
pub struct SequenceSource {
    next: AtomicU64,
}

impl Default for SequenceSource {
    fn default() -> Self {
        // Start at 1 so the first report has sequence 1 (0 reads as
        // "never reported" in logs).
        Self {
            next: AtomicU64::new(1),
        }
    }
}

impl SequenceSource {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the next sequence and advance. Monotonic, never reused.
    pub fn next(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }
}

/// Build a `UsageReport` from a drained snapshot, stamping a fresh uuidv7
/// `report_id` and the next monotonic `sequence`. Returns `None` for an
/// empty snapshot (nothing to report — the flush task skips the POST).
#[must_use]
pub fn build_report(
    worker_id: &str,
    sequence: u64,
    counters: HashMap<Uuid, AppUsage>,
) -> Option<UsageReport> {
    if counters.is_empty() {
        return None;
    }
    Some(UsageReport {
        worker_id: worker_id.to_string(),
        report_id: Uuid::now_v7(),
        sequence,
        counters,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> String {
        Uuid::new_v4().to_string()
    }

    #[test]
    fn increment_then_drain_returns_total_and_resets() {
        let m = Meter::new();
        let a = app();
        m.increment(&a, "requests", 1);
        m.increment(&a, "requests", 4);

        let snap = m.drain();
        let id = Uuid::parse_str(&a).unwrap();
        assert_eq!(snap.get(&id).unwrap().requests, 5);

        // Drain resets: a second drain with no further activity is empty.
        let snap2 = m.drain();
        assert!(snap2.is_empty(), "drain must reset counters to zero");
    }

    #[test]
    fn custom_metric_lands_in_custom_map_not_fixed() {
        let m = Meter::new();
        let a = app();
        m.increment(&a, "emails_sent", 3);
        m.increment(&a, "emails_sent", 2);

        let snap = m.drain();
        let id = Uuid::parse_str(&a).unwrap();
        let usage = snap.get(&id).unwrap();
        assert_eq!(usage.requests, 0, "custom must not touch a fixed counter");
        assert_eq!(usage.custom.get("emails_sent").copied(), Some(5));
    }

    #[test]
    fn fixed_metric_name_routes_to_fixed_field() {
        let m = Meter::new();
        let a = app();
        // An SDK calling increment("cpu_us", ...) must land in the fixed
        // field, not custom (these names are reserved).
        m.increment(&a, "cpu_us", 100);
        let snap = m.drain();
        let id = Uuid::parse_str(&a).unwrap();
        let usage = snap.get(&id).unwrap();
        assert_eq!(usage.cpu_us, 100);
        assert!(usage.custom.is_empty());
    }

    #[test]
    fn per_app_isolation() {
        let m = Meter::new();
        let a = app();
        let b = app();
        m.increment(&a, "requests", 10);
        m.increment(&b, "requests", 3);
        m.increment(&b, "widgets", 1);

        let snap = m.drain();
        let ia = Uuid::parse_str(&a).unwrap();
        let ib = Uuid::parse_str(&b).unwrap();
        assert_eq!(snap.get(&ia).unwrap().requests, 10);
        assert!(snap.get(&ia).unwrap().custom.is_empty());
        assert_eq!(snap.get(&ib).unwrap().requests, 3);
        assert_eq!(snap.get(&ib).unwrap().custom.get("widgets").copied(), Some(1));
    }

    #[test]
    fn record_request_feeds_fixed_counters() {
        let m = Meter::new();
        let a = app();
        m.record_request(&a, 500, 1200, 2048, 256);
        m.record_request(&a, 100, 300, 64, 16);

        let snap = m.drain();
        let id = Uuid::parse_str(&a).unwrap();
        let u = snap.get(&id).unwrap();
        assert_eq!(u.requests, 2);
        assert_eq!(u.cpu_us, 600);
        assert_eq!(u.wall_us, 1500);
        assert_eq!(u.egress_bytes, 2112);
        assert_eq!(u.ingress_bytes, 272);
    }

    #[test]
    fn merge_carries_forward_after_failed_flush() {
        let m = Meter::new();
        let a = app();
        m.increment(&a, "requests", 7);
        m.increment(&a, "emails", 2);

        // Flush drains; pretend the POST failed → merge back.
        let drained = m.drain();
        m.merge(&drained);
        // Add a little more after the carry-forward.
        m.increment(&a, "requests", 1);

        let snap = m.drain();
        let id = Uuid::parse_str(&a).unwrap();
        let u = snap.get(&id).unwrap();
        assert_eq!(u.requests, 8, "carried-forward 7 + new 1");
        assert_eq!(u.custom.get("emails").copied(), Some(2));
    }

    #[test]
    fn empty_meter_drains_to_empty_map() {
        let m = Meter::new();
        assert!(m.drain().is_empty());
    }

    #[test]
    fn sequence_source_is_monotonic() {
        let s = SequenceSource::new();
        assert_eq!(s.next(), 1);
        assert_eq!(s.next(), 2);
        assert_eq!(s.next(), 3);
    }

    #[test]
    fn build_report_stamps_seq_and_skips_empty() {
        assert!(build_report("w1", 1, HashMap::new()).is_none());

        let id = Uuid::new_v4();
        let mut counters = HashMap::new();
        counters.insert(id, AppUsage { requests: 1, ..Default::default() });
        let r = build_report("w1", 5, counters).unwrap();
        assert_eq!(r.worker_id, "w1");
        assert_eq!(r.sequence, 5);
        assert_eq!(r.version_is_v7(), true);
    }
}

// Test-only helper hung off UsageReport via an extension trait so the
// build_report test can assert the report_id is a v7 uuid without leaking
// a method onto the public type.
#[cfg(test)]
trait UsageReportExt {
    fn version_is_v7(&self) -> bool;
}

#[cfg(test)]
impl UsageReportExt for UsageReport {
    fn version_is_v7(&self) -> bool {
        self.report_id.get_version() == Some(uuid::Version::SortRand)
    }
}
