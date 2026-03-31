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
    /// Period start time, wrapped in Mutex so reset_period(&self) can update it.
    pub period_start: Mutex<SystemTime>,
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
            period_start: Mutex::new(SystemTime::now()),
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
    ///
    /// `cpu_us` and `wall_us` are in microseconds from the isolate/timer;
    /// they are converted to milliseconds for storage (matching quota keys).
    ///
    /// NOTE: The `/ 1000` truncation loses sub-millisecond precision. Many fast
    /// requests (< 1ms CPU) will record 0ms, causing under-counting of CPU usage.
    /// A future fix should store microseconds (register as "cpu_us" / "wall_us")
    /// and update quota definitions accordingly, or use a fractional accumulator.
    pub fn record_request(&self, cpu_us: u64, wall_us: u64, egress: u64, ingress: u64) {
        self.counters.increment(self.core.requests, 1);
        self.counters.increment(self.core.cpu_ms, cpu_us / 1000);
        self.counters.increment(self.core.wall_ms, wall_us / 1000);
        self.counters.increment(self.core.egress_bytes, egress);
        self.counters.increment(self.core.ingress_bytes, ingress);
    }

    /// Take a consistent snapshot of current usage as name → value map.
    /// Uses Acquire ordering to see all Release writes from record_request().
    pub fn snapshot(&self) -> HashMap<String, u64> {
        self.counters.snapshot()
    }

    /// Seconds since the billing period started.
    pub fn period_age_secs(&self) -> f64 {
        self.period_start
            .lock()
            .unwrap()
            .elapsed()
            .unwrap_or_default()
            .as_secs_f64()
    }

    /// Reset all counters for a new billing period.
    /// Uses Release ordering so subsequent Acquire reads see zeros.
    pub fn reset_period(&self) {
        self.counters.reset_all();
        *self.period_start.lock().unwrap() = SystemTime::now();
        self.spend_action.store(0, Ordering::Release);
    }
}

/// Wrapper that implements `PluginMeter` by looking up an app's `CounterRegistry`
/// from the `MeterRegistry` at runtime. This avoids lifetime issues with passing
/// `&CounterRegistry` directly into isolates, and ensures plugin ops (db.reads,
/// kv.writes, etc.) are recorded against the correct app's meter instead of being
/// silently discarded by `NoopMeter`.
pub struct AppPluginMeter {
    registry: Arc<MeterRegistry>,
    app_id: String,
}

impl AppPluginMeter {
    pub fn new(registry: Arc<MeterRegistry>, app_id: String) -> Self {
        Self { registry, app_id }
    }
}

impl appbase_core::plugin::PluginMeter for AppPluginMeter {
    fn increment(&self, resource_name: &str, delta: u64) {
        let meter = self.registry.get_or_create(&self.app_id);
        if let Some(handle) = meter.counters.handle_for(resource_name) {
            meter.counters.increment(handle, delta);
        }
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

    /// Set a specific plan for an app, preserving existing counter values.
    pub fn set_plan(&self, app_id: &str, plan: QuotaPlan) {
        let mut meters = self.meters.lock().unwrap();
        if let Some(old) = meters.get(app_id) {
            // Transfer counter values from old meter to new one
            let snapshot = old.counters.snapshot();
            let new_meter = AppMeter::new(plan);
            for (name, value) in &snapshot {
                if let Some(handle) = new_meter.counters.handle_for(name) {
                    new_meter.counters.increment(handle, *value);
                }
            }
            meters.insert(app_id.to_string(), Arc::new(new_meter));
        } else {
            meters.insert(app_id.to_string(), Arc::new(AppMeter::new(plan)));
        }
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

impl appbase_billing::reconciler::MeteringSnapshot for MeterRegistry {
    fn snapshot(&self, app_id: &str) -> Option<HashMap<String, u64>> {
        let meters = self.meters.lock().unwrap();
        meters.get(app_id).map(|m| m.counters.snapshot())
    }

    fn active_apps(&self) -> Vec<String> {
        self.meters.lock().unwrap().keys().cloned().collect()
    }
}

impl appbase_billing::reconciler::SpendEnforcement for MeterRegistry {
    fn set_spend_action(
        &self,
        app_id: &str,
        action: appbase_billing::spend_action::SpendAction,
    ) {
        let meters = self.meters.lock().unwrap();
        if let Some(meter) = meters.get(app_id) {
            appbase_billing::spend_action::SpendAction::store(&meter.spend_action, action);
        }
    }

    fn get_spend_action(&self, app_id: &str) -> appbase_billing::spend_action::SpendAction {
        let meters = self.meters.lock().unwrap();
        meters
            .get(app_id)
            .map(|m| appbase_billing::spend_action::SpendAction::load(&m.spend_action))
            .unwrap_or(appbase_billing::spend_action::SpendAction::Allow)
    }
}
