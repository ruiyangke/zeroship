//! Per-app atomic usage counters.
//!
//! All counters use `AtomicU64` for lock-free concurrent updates
//! from multiple HTTP handler threads. The meter is shared via `Arc`.
//!
//! v3.0: Uses CounterRegistry for dynamic, plugin-extensible counters.
//! Core resources are accessed via CoreHandles for O(1) fast-path.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::plan::QuotaPlan;
use crate::registry::{CoreHandles, CounterRegistry, RegistryBuilder};

/// Usage counters for a single app within a billing period.
pub struct AppMeter {
    pub plan: QuotaPlan,
    pub period_start: SystemTime,
    /// Dynamic counters — core + plugin resources.
    pub counters: CounterRegistry,
    /// Core resource handles for fast-path access from router.
    pub core: CoreHandles,
    /// Spending enforcement action, set by billing reconciler, read by enforcer.
    /// 0=Allow, 1=Warn, 2=Degrade, 3=Block
    pub spend_action: AtomicU8,
}

impl AppMeter {
    pub fn new(plan: QuotaPlan) -> Self {
        let mut builder = RegistryBuilder::new();
        let core = builder.register_core();
        Self {
            plan,
            period_start: SystemTime::now(),
            counters: builder.build(),
            core,
            spend_action: AtomicU8::new(0), // Allow
        }
    }

    /// Restore counters from the warm tier after a restart (crash recovery, spec §4.6).
    /// Populates atomic counters from a stored HashMap so enforcement resumes
    /// from where it left off, not from zero.
    pub fn from_stored(plan: QuotaPlan, stored: &HashMap<String, u64>) -> Self {
        let meter = Self::new(plan);
        // Restore counters from stored values
        for (name, &value) in stored {
            if let Some(handle) = meter.counters.handle_for(name) {
                meter.counters.increment(handle, value);
            }
        }
        meter
    }

    /// Look up a counter by resource name (maps string keys to atomic counters).
    pub fn get_counter(&self, name: &str) -> u64 {
        self.counters.get(name).unwrap_or(0)
    }

    /// Record a completed request's usage via core handles.
    /// Uses Release ordering so Acquire reads on other cores (enforcer, flusher)
    /// see the updated values. Required for ARM/AArch64 correctness.
    pub fn record_request(&self, cpu_us: u64, wall_us: u64, egress: u64) {
        self.counters.increment(self.core.requests, 1);
        self.counters.increment(self.core.cpu_us, cpu_us);
        self.counters.increment(self.core.wall_us, wall_us);
        self.counters.increment(self.core.egress_bytes, egress);
    }

    /// Take a consistent snapshot of current usage as name → value map.
    /// Uses Acquire ordering to see all Release writes from record_request().
    pub fn snapshot(&self) -> HashMap<String, u64> {
        self.counters.snapshot()
    }

    /// Seconds since the billing period started.
    pub fn period_age_secs(&self) -> f64 {
        self.period_start
            .elapsed()
            .unwrap_or_default()
            .as_secs_f64()
    }

    /// Reset all counters for a new billing period.
    /// Uses Release ordering so subsequent Acquire reads see zeros.
    pub fn reset_period(&self) {
        self.counters.reset_all();
        self.spend_action.store(0, Ordering::Release);
    }
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
    pub fn get_usage(&self, app_id: &str) -> Option<HashMap<String, u64>> {
        let meters = self.meters.lock().unwrap();
        meters.get(app_id).map(|m| m.snapshot())
    }

    /// Get usage snapshots for all apps.
    pub fn all_usage(&self) -> HashMap<String, HashMap<String, u64>> {
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

    /// Atomically swap an app's meter for a fresh one (period rollover).
    /// Returns the old meter (for draining and reading), or None if app not found.
    pub fn swap_for_rollover(&self, app_id: &str) -> Option<Arc<AppMeter>> {
        let mut meters = self.meters.lock().unwrap();
        let old = meters.remove(app_id)?;
        let new = Arc::new(AppMeter::new(old.plan.clone()));
        meters.insert(app_id.to_string(), new);
        Some(old)
    }

    /// Recover counters from the warm tier after a restart (spec §4.6).
    /// For each app in the store, loads the stored counters and populates
    /// the hot-tier atomics so enforcement resumes from the last-flushed state.
    pub fn recover_from_store(
        &self,
        store: &dyn appbase_core::meter_store::MeterStore,
        app_ids: &[String],
    ) {
        for app_id in app_ids {
            match store.load(app_id) {
                Ok(stored) if !stored.is_empty() => {
                    let meter = Arc::new(AppMeter::from_stored(
                        self.default_plan.clone(),
                        &stored,
                    ));
                    self.meters.lock().unwrap().insert(app_id.clone(), meter);
                    eprintln!("[metering] Recovered counters for {app_id}: {} resources", stored.len());
                }
                Ok(_) => {} // empty, no recovery needed
                Err(e) => eprintln!("[metering] Failed to recover {app_id}: {e}"),
            }
        }
    }
}
