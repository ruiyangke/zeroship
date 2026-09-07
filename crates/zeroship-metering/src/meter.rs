//! Per-worker usage meter — atomic per-`(app_id, metric)` counters.
//!
//! One `Meter` is shared (via `Arc`) across every worker thread in a process.
//! There is NO creator-facing `env.meter` API: the billing signal is
//! platform-measured so app code can neither forge nor suppress it. The only
//! increment sources are the worker's platform counters via
//! [`Meter::record_request`] and trusted data primitives through `MeterHandle`.
//!
//! `drain` emits one immutable `UsageEvent` per drained `(app_id, metric)`
//! window. Each event gets a fresh uuidv7 `event_id` and a shared drain
//! wall-clock `event_time`.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use uuid::Uuid;
use zeroship_core::usage_event::{UsageEvent, UsageSubject};

const DEFAULT_SOURCE: &str = "zeroship-worker";

#[derive(Debug, Clone, PartialEq, Eq)]
struct DrainedMetric {
    meter: String,
    value: u64,
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
    /// Platform-emitted resource metrics from the trusted data primitives
    /// (`db_reads`, `db_writes`, `kv_reads`, `kv_writes`, `storage_ops`, ...).
    /// `Mutex` (not per-key atomics) because the key set is open and small;
    /// contention is negligible at drain cadence.
    custom: Mutex<HashMap<String, u64>>,
}

impl AppCounters {
    fn add_fixed_request(
        &self,
        cpu_us: u64,
        wall_us: u64,
        egress_bytes: u64,
        ingress_bytes: u64,
    ) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        if cpu_us > 0 {
            self.cpu_us.fetch_add(cpu_us, Ordering::Relaxed);
        }
        if wall_us > 0 {
            self.wall_us.fetch_add(wall_us, Ordering::Relaxed);
        }
        if egress_bytes > 0 {
            self.egress_bytes.fetch_add(egress_bytes, Ordering::Relaxed);
        }
        if ingress_bytes > 0 {
            self.ingress_bytes.fetch_add(ingress_bytes, Ordering::Relaxed);
        }
    }

    /// Bump a counter by `n`. A fixed-metric name routes to its atomic; any
    /// other name lands in `custom`.
    fn add(&self, metric: &str, n: u64) {
        match metric {
            "requests" => {
                self.requests.fetch_add(n, Ordering::Relaxed);
            }
            "cpu_us" => {
                self.cpu_us.fetch_add(n, Ordering::Relaxed);
            }
            "wall_us" => {
                self.wall_us.fetch_add(n, Ordering::Relaxed);
            }
            "egress_bytes" => {
                self.egress_bytes.fetch_add(n, Ordering::Relaxed);
            }
            "ingress_bytes" => {
                self.ingress_bytes.fetch_add(n, Ordering::Relaxed);
            }
            other => {
                let mut c = self.custom.lock().unwrap();
                let slot = c.entry(other.to_string()).or_insert(0);
                *slot += n;
            }
        }
    }

    /// Take a snapshot of current values AND reset them to zero in one shot
    /// (`swap`). Returns an empty vector when every counter is zero.
    fn drain_metrics(&self) -> Vec<DrainedMetric> {
        let mut out = Vec::new();
        push_metric(&mut out, "requests", self.requests.swap(0, Ordering::Relaxed));
        push_metric(&mut out, "cpu_us", self.cpu_us.swap(0, Ordering::Relaxed));
        push_metric(&mut out, "wall_us", self.wall_us.swap(0, Ordering::Relaxed));
        push_metric(
            &mut out,
            "egress_bytes",
            self.egress_bytes.swap(0, Ordering::Relaxed),
        );
        push_metric(
            &mut out,
            "ingress_bytes",
            self.ingress_bytes.swap(0, Ordering::Relaxed),
        );

        let custom = std::mem::take(&mut *self.custom.lock().unwrap());
        let mut custom: Vec<_> = custom.into_iter().collect();
        custom.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        for (meter, value) in custom {
            push_metric(&mut out, &meter, value);
        }
        out
    }
}

fn push_metric(out: &mut Vec<DrainedMetric>, meter: &str, value: u64) {
    if value > 0 {
        out.push(DrainedMetric {
            meter: meter.to_string(),
            value,
        });
    }
}

/// Process-wide usage meter. Cheap to clone behind an `Arc`.
///
/// The increment fast path bumps an atomic under a SHARED read guard, so
/// increments never exclude one another. A brief write lock is taken when an
/// app's `AppCounters` must be created - on first touch, and again after
/// [`Self::drain`] has evicted an idle app (see the trade documented there).
/// The one thing that does exclude increments is `drain` itself, which takes
/// the exclusive lock.
#[derive(Debug)]
pub struct Meter {
    source: String,
    apps: RwLock<HashMap<String, AppCounters>>,
}

impl Default for Meter {
    fn default() -> Self {
        Self::with_source(DEFAULT_SOURCE)
    }
}

impl Meter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a meter with the authenticated worker/source identity that
    /// namespaces emitted event ids for provider dedup.
    #[must_use]
    pub fn with_source(source: impl Into<String>) -> Self {
        let source = source.into();
        let source = if source.trim().is_empty() {
            DEFAULT_SOURCE.to_string()
        } else {
            source
        };
        Self {
            source,
            apps: RwLock::new(HashMap::new()),
        }
    }

    /// How many apps this meter currently holds counters for.
    ///
    /// Bounded by the count of apps with traffic since the last drain, NOT by
    /// the count this process has ever seen - [`Self::drain`] evicts entries
    /// that drained empty. Exposed so an operator (or a gate) can assert that
    /// boundedness rather than trust it.
    ///
    /// # Panics
    ///
    /// If the `apps` lock is poisoned, as every other accessor on this type
    /// does: a poisoned meter lock means a counter update panicked mid-write,
    /// and continuing on unknown counter state would corrupt billing input.
    #[must_use]
    pub fn tracked_app_count(&self) -> usize {
        self.apps.read().unwrap().len()
    }

    /// Increment `metric` for `app_id` by `n`. Auto-vivifies the app's counter
    /// set on first touch. A `metric` matching one of the five fixed names lands
    /// in that fixed field; everything else is a custom metric.
    pub fn increment(&self, app_id: &str, metric: &str, n: u64) {
        self.with_app_counters(app_id, |counters| counters.add(metric, n));
    }

    fn with_app_counters<R>(&self, app_id: &str, f: impl FnOnce(&AppCounters) -> R) -> R {
        // Fast path: read lock, app already present.
        {
            let apps = self.apps.read().unwrap();
            if let Some(counters) = apps.get(app_id) {
                return f(counters);
            }
        }
        // Slow path: write lock, insert if another thread did not already
        // create the app after the read guard was released.
        let mut apps = self.apps.write().unwrap();
        f(apps.entry(app_id.to_string()).or_default())
    }

    /// Record one completed request's platform counters in a single call.
    pub fn record_request(
        &self,
        app_id: &str,
        cpu_us: u64,
        wall_us: u64,
        egress_bytes: u64,
        ingress_bytes: u64,
    ) {
        self.with_app_counters(app_id, |counters| {
            counters.add_fixed_request(cpu_us, wall_us, egress_bytes, ingress_bytes);
        });
    }

    /// Drain every app's counters to `UsageEvent`s, resetting each counter to
    /// zero. Apps with zero activity since the last drain are omitted from the
    /// result AND **evicted from the map**. The returned vector is empty when
    /// no app saw any traffic.
    ///
    /// Held under the write lock so no increment interleaves a partial drain
    /// (fixed-counter `swap`s plus a `custom` take must be atomic per app
    /// relative to that app's own increments).
    ///
    /// **Why evict.** Every increment auto-vivifies an entry and nothing used to
    /// remove one, so the map's size tracked "every app this process has ever
    /// touched". That is unbounded memory on a long-lived worker, and - because
    /// this scan runs under the EXCLUSIVE lock on the outbox's ten-second
    /// cadence - it is also a periodic process-wide stall on every `env.db` /
    /// `env.kv` / `env.storage` increment, whose length grows with uptime rather
    /// than with load. Evicting on an empty drain bounds both by the ACTIVE app
    /// count.
    ///
    /// **The trade this makes, stated plainly.** The type comment above promises
    /// a write lock "only on first-touch per app". With eviction, an app that
    /// stays idle across a drain pays that first-touch write lock again when it
    /// next increments. That is the right side of the trade: an app busy enough
    /// for the write lock to matter never idles through a whole drain window, and
    /// one that does idle is by definition not on a hot path. What it buys is
    /// that neither the map nor the stall can grow without bound.
    ///
    /// It does NOT remove the stall itself, only its unbounded growth. The
    /// exclusive lock is still taken process-wide to get an atomicity property
    /// that is only needed per app; making that per-app is a separate change.
    #[must_use]
    pub fn drain(&self) -> Vec<UsageEvent> {
        let event_time = drain_wall_clock_unix();
        let mut apps = self.apps.write().unwrap();
        let mut events = Vec::new();
        apps.retain(|app_id, counters| {
            let drained = counters.drain_metrics();
            if drained.is_empty() {
                // Idle since the last drain: drop the entry instead of carrying
                // it for the life of the process. Every increment auto-vivifies,
                // so this is not a tombstone - the app reappears, from zero, the
                // next time it is touched. Without this the map size tracks every
                // app the process has EVER seen, which is unbounded memory and a
                // growing exclusive-lock hold on every drain.
                return false;
            }
            let app_uuid = match Uuid::parse_str(app_id) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(
                        app_id = %app_id,
                        error = %e,
                        "meter: drain skipped non-UUID app_id"
                    );
                    // Evict too: this entry can never produce a valid event, so
                    // retaining it only grows the map and re-warns every drain.
                    // Its counters were already zeroed by `drain_metrics` above,
                    // so keeping it would not preserve anything either.
                    return false;
                }
            };
            // The worker producer has only the server-injected app id, so the
            // subject is left UNSET and the control plane resolves the owning
            // organization from the app registry before provider forwarding.
            for metric in drained {
                events.push(UsageEvent {
                    event_id: Uuid::now_v7().to_string(),
                    source: self.source.clone(),
                    subject: UsageSubject {
                        app: Some(app_uuid),
                        organization: None,
                    },
                    meter: metric.meter,
                    value: metric.value,
                    event_time,
                    dims: BTreeMap::new(),
                });
            }
            true
        });
        events
    }
}

fn drain_wall_clock_unix() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> String {
        Uuid::new_v4().to_string()
    }

    fn event_value(events: &[UsageEvent], app_id: Uuid, meter: &str) -> Option<u64> {
        events
            .iter()
            .find(|event| event.subject.app == Some(app_id) && event.meter == meter)
            .map(|event| event.value)
    }

    fn event<'a>(events: &'a [UsageEvent], app_id: Uuid, meter: &str) -> &'a UsageEvent {
        events
            .iter()
            .find(|event| event.subject.app == Some(app_id) && event.meter == meter)
            .expect("usage event exists")
    }

    /// An app that goes idle must not be retained forever.
    ///
    /// `apps` is process-wide and every increment auto-vivifies an entry, so
    /// without eviction its size tracks "every app this process ever touched"
    /// rather than "apps with traffic". That is unbounded memory on a
    /// long-lived worker, and it is also a latency bug: `drain` walks the whole
    /// map under the EXCLUSIVE lock on a ten-second cadence, so every increment
    /// in the process stalls behind a scan whose length grows with uptime.
    /// Evicting on an empty drain bounds both by the ACTIVE app count.
    #[test]
    fn drain_evicts_apps_that_went_idle() {
        let m = Meter::new();
        let a = app();
        m.increment(&a, "requests", 1);

        // First drain: the app has traffic, so it reports and is kept.
        assert_eq!(m.drain().len(), 1);
        assert_eq!(m.tracked_app_count(), 1, "an active app must be retained");

        // Second drain: nothing happened since, so the entry must go.
        assert!(m.drain().is_empty());
        assert_eq!(
            m.tracked_app_count(),
            0,
            "an app idle since the last drain must be evicted, not retained forever",
        );

        // Re-vivification still works: the app is not tombstoned, just absent.
        m.increment(&a, "requests", 7);
        assert_eq!(m.tracked_app_count(), 1);
        let events = m.drain();
        assert_eq!(
            event_value(&events, Uuid::parse_str(&a).unwrap(), "requests"),
            Some(7),
            "a re-touched app must count from zero, not resume a stale total",
        );
    }

    #[test]
    fn increment_then_drain_returns_event_and_resets() {
        let m = Meter::with_source("worker-a");
        let a = app();
        m.increment(&a, "requests", 1);
        m.increment(&a, "requests", 4);

        let events = m.drain();
        let id = Uuid::parse_str(&a).unwrap();
        let request = event(&events, id, "requests");
        assert_eq!(request.value, 5);
        assert_eq!(request.source, "worker-a");
        assert_eq!(request.subject.app, Some(id));
        assert_eq!(request.subject.organization, None);
        assert!(!request.event_id.is_empty());
        assert_eq!(
            Uuid::parse_str(&request.event_id).unwrap().get_version(),
            Some(uuid::Version::SortRand)
        );
        assert!(request.event_time > 0);

        let events2 = m.drain();
        assert!(events2.is_empty(), "drain must reset counters to zero");
    }

    #[test]
    fn custom_metric_lands_in_event_not_fixed() {
        let m = Meter::new();
        let a = app();
        m.increment(&a, "emails_sent", 3);
        m.increment(&a, "emails_sent", 2);

        let events = m.drain();
        let id = Uuid::parse_str(&a).unwrap();
        assert_eq!(event_value(&events, id, "emails_sent"), Some(5));
        assert_eq!(event_value(&events, id, "requests"), None);
    }

    #[test]
    fn fixed_metric_name_routes_to_fixed_event() {
        let m = Meter::new();
        let a = app();
        // A producer calling increment("cpu_us", ...) must land in the fixed
        // metric, not a custom duplicate.
        m.increment(&a, "cpu_us", 100);
        let events = m.drain();
        let id = Uuid::parse_str(&a).unwrap();
        assert_eq!(event_value(&events, id, "cpu_us"), Some(100));
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn per_app_isolation() {
        let m = Meter::new();
        let a = app();
        let b = app();
        m.increment(&a, "requests", 10);
        m.increment(&b, "requests", 3);
        m.increment(&b, "widgets", 1);

        let events = m.drain();
        let ia = Uuid::parse_str(&a).unwrap();
        let ib = Uuid::parse_str(&b).unwrap();
        assert_eq!(event_value(&events, ia, "requests"), Some(10));
        assert_eq!(event_value(&events, ia, "widgets"), None);
        assert_eq!(event_value(&events, ib, "requests"), Some(3));
        assert_eq!(event_value(&events, ib, "widgets"), Some(1));
    }

    #[test]
    fn record_request_feeds_fixed_counter_events() {
        let m = Meter::new();
        let a = app();
        m.record_request(&a, 500, 1200, 2048, 256);
        m.record_request(&a, 100, 300, 64, 16);

        let events = m.drain();
        let id = Uuid::parse_str(&a).unwrap();
        assert_eq!(event_value(&events, id, "requests"), Some(2));
        assert_eq!(event_value(&events, id, "cpu_us"), Some(600));
        assert_eq!(event_value(&events, id, "wall_us"), Some(1500));
        assert_eq!(event_value(&events, id, "egress_bytes"), Some(2112));
        assert_eq!(event_value(&events, id, "ingress_bytes"), Some(272));
    }

    #[test]
    fn empty_meter_drains_to_empty_vec() {
        let m = Meter::new();
        assert!(m.drain().is_empty());
    }
}
