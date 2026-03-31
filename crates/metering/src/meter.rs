//! Per-app atomic usage counters.
//!
//! All counters use `AtomicU64` for lock-free concurrent updates
//! from multiple HTTP handler threads. The meter is shared via `Arc`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use crate::plan::QuotaPlan;

/// Usage counters for a single app within a billing period.
pub struct AppMeter {
    pub plan: QuotaPlan,
    pub period_start: SystemTime,
    pub requests: AtomicU64,
    pub cpu_time_us: AtomicU64,
    pub wall_time_us: AtomicU64,
    pub egress_bytes: AtomicU64,
    pub db_reads: AtomicU64,
    pub db_writes: AtomicU64,
    pub kv_ops: AtomicU64,
}

impl AppMeter {
    pub fn new(plan: QuotaPlan) -> Self {
        Self {
            plan,
            period_start: SystemTime::now(),
            requests: AtomicU64::new(0),
            cpu_time_us: AtomicU64::new(0),
            wall_time_us: AtomicU64::new(0),
            egress_bytes: AtomicU64::new(0),
            db_reads: AtomicU64::new(0),
            db_writes: AtomicU64::new(0),
            kv_ops: AtomicU64::new(0),
        }
    }

    /// Look up a counter by resource name (maps string keys to atomic fields).
    pub fn get_counter(&self, name: &str) -> u64 {
        match name {
            "requests" => self.requests.load(Ordering::Acquire),
            "cpu_ms" => self.cpu_time_us.load(Ordering::Acquire) / 1000,
            "wall_ms" => self.wall_time_us.load(Ordering::Acquire) / 1000,
            "egress_bytes" => self.egress_bytes.load(Ordering::Acquire),
            "db_reads" => self.db_reads.load(Ordering::Acquire),
            "db_writes" => self.db_writes.load(Ordering::Acquire),
            "kv_ops" => self.kv_ops.load(Ordering::Acquire),
            _ => 0,
        }
    }

    /// Record a completed request's usage.
    /// Uses Release ordering so Acquire reads on other cores (enforcer, flusher)
    /// see the updated values. Required for ARM/AArch64 correctness.
    pub fn record(&self, delta: &UsageDelta) {
        self.requests.fetch_add(1, Ordering::Release);
        self.cpu_time_us
            .fetch_add(delta.cpu_time.as_micros() as u64, Ordering::Release);
        self.wall_time_us
            .fetch_add(delta.wall_time.as_micros() as u64, Ordering::Release);
        self.egress_bytes
            .fetch_add(delta.egress_bytes, Ordering::Release);
        self.db_reads
            .fetch_add(delta.db_reads, Ordering::Release);
        self.db_writes
            .fetch_add(delta.db_writes, Ordering::Release);
        self.kv_ops.fetch_add(delta.kv_ops, Ordering::Release);
    }

    /// Take a consistent snapshot of current usage.
    /// Uses Acquire ordering to see all Release writes from record().
    pub fn snapshot(&self) -> UsageSnapshot {
        UsageSnapshot {
            requests: self.requests.load(Ordering::Acquire),
            cpu_time_ms: self.cpu_time_us.load(Ordering::Acquire) as f64 / 1000.0,
            wall_time_ms: self.wall_time_us.load(Ordering::Acquire) as f64 / 1000.0,
            egress_bytes: self.egress_bytes.load(Ordering::Acquire),
            db_reads: self.db_reads.load(Ordering::Acquire),
            db_writes: self.db_writes.load(Ordering::Acquire),
            kv_ops: self.kv_ops.load(Ordering::Acquire),
            period_age_secs: self
                .period_start
                .elapsed()
                .unwrap_or_default()
                .as_secs_f64(),
        }
    }

    /// Reset all counters for a new billing period.
    /// Uses Release ordering so subsequent Acquire reads see zeros.
    pub fn reset_period(&self) {
        self.requests.store(0, Ordering::Release);
        self.cpu_time_us.store(0, Ordering::Release);
        self.wall_time_us.store(0, Ordering::Release);
        self.egress_bytes.store(0, Ordering::Release);
        self.db_reads.store(0, Ordering::Release);
        self.db_writes.store(0, Ordering::Release);
        self.kv_ops.store(0, Ordering::Release);
    }
}

/// Incremental usage from a single request.
#[derive(Debug, Default, Clone)]
pub struct UsageDelta {
    pub cpu_time: Duration,
    pub wall_time: Duration,
    pub egress_bytes: u64,
    pub db_reads: u64,
    pub db_writes: u64,
    pub kv_ops: u64,
}

/// Point-in-time snapshot of an app's usage (serializable).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub requests: u64,
    pub cpu_time_ms: f64,
    pub wall_time_ms: f64,
    pub egress_bytes: u64,
    pub db_reads: u64,
    pub db_writes: u64,
    pub kv_ops: u64,
    /// Seconds since the billing period started.
    pub period_age_secs: f64,
}

/// Registry of all app meters. Thread-safe, shared across handlers.
pub struct MeterRegistry {
    meters: Mutex<HashMap<String, Arc<AppMeter>>>,
    default_plan: QuotaPlan,
}

impl MeterRegistry {
    pub fn new(default_plan: QuotaPlan) -> Self {
        Self {
            meters: Mutex::new(HashMap::new()),
            default_plan,
        }
    }

    /// Get or create a meter for an app.
    pub fn get_or_create(&self, app_id: &str) -> Arc<AppMeter> {
        let mut meters = self.meters.lock().unwrap();
        meters
            .entry(app_id.to_string())
            .or_insert_with(|| Arc::new(AppMeter::new(self.default_plan.clone())))
            .clone()
    }

    /// Set a specific plan for an app.
    pub fn set_plan(&self, app_id: &str, plan: QuotaPlan) {
        let mut meters = self.meters.lock().unwrap();
        meters.insert(app_id.to_string(), Arc::new(AppMeter::new(plan)));
    }

    /// Get usage snapshot for an app. Returns None if no meter exists.
    pub fn get_usage(&self, app_id: &str) -> Option<UsageSnapshot> {
        let meters = self.meters.lock().unwrap();
        meters.get(app_id).map(|m| m.snapshot())
    }

    /// Get usage snapshots for all apps.
    pub fn all_usage(&self) -> HashMap<String, UsageSnapshot> {
        let meters = self.meters.lock().unwrap();
        meters
            .iter()
            .map(|(id, m)| (id.clone(), m.snapshot()))
            .collect()
    }

    /// Reset a specific app's usage counters.
    pub fn reset(&self, app_id: &str) {
        let meters = self.meters.lock().unwrap();
        if let Some(meter) = meters.get(app_id) {
            meter.reset_period();
        }
    }

    /// Remove an app's meter entirely.
    pub fn remove(&self, app_id: &str) {
        let mut meters = self.meters.lock().unwrap();
        meters.remove(app_id);
    }

    /// Get all meters (for flusher).
    pub fn all_meters(&self) -> HashMap<String, Arc<AppMeter>> {
        let meters = self.meters.lock().unwrap();
        meters.clone()
    }
}
