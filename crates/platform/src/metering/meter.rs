//! Per-app atomic usage counters.
//!
//! All counters use `AtomicU64` for lock-free concurrent updates
//! from multiple HTTP handler threads. The meter is shared via `Arc`.
//!
//! v3.0: Uses CounterRegistry for dynamic, plugin-extensible counters.
//! Core resources are accessed via CoreHandles for O(1) fast-path.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

use crate::core::plugin::MeterResource;
use crate::plan::QuotaPlan;
use crate::metering::registry::{CoreHandles, CounterRegistry, RegistryBuilder};

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
    /// Set by rollover before drain wait, checked by flusher to avoid double-counting.
    pub rolling_over: AtomicBool,
}

impl AppMeter {
    /// Create a meter with core resources plus additional plugin-declared resources.
    pub fn with_resources(plan: QuotaPlan, plugin_resources: &[MeterResource]) -> Self {
        let mut builder = RegistryBuilder::new();
        let core = builder.register_core();
        // Register plugin-declared resources (db.reads, kv.writes, etc.)
        for res in plugin_resources {
            builder.register(res.clone());
        }
        Self {
            plan,
            period_start: Mutex::new(SystemTime::now()),
            counters: builder.build(),
            core,
            spend_action: AtomicU8::new(0), // Allow
            rolling_over: AtomicBool::new(false),
        }
    }

    /// Restore counters from the warm tier after a restart (crash recovery, spec §4.6).
    /// Populates atomic counters from a stored HashMap so enforcement resumes
    /// from where it left off, not from zero.
    pub fn from_stored(plan: QuotaPlan, plugin_resources: &[MeterResource], stored: &HashMap<String, u64>) -> Self {
        let meter = Self::with_resources(plan, plugin_resources);
        // Restore counters from stored values
        for (name, &value) in stored {
            if let Some(handle) = meter.counters.handle_for(name) {
                meter.counters.increment(handle, value);
            }
        }
        // Sync last_flushed to match restored values so first flush doesn't double-count
        meter.counters.sync_last_flushed();
        meter
    }

    /// Look up a counter by resource name (maps string keys to atomic counters).
    pub fn get_counter(&self, name: &str) -> u64 {
        self.counters.get(name).unwrap_or(0)
    }

    /// Record a completed request's usage via core handles.
    /// CPU and wall time stored in microseconds — no truncation.
    pub fn record_request(&self, cpu_us: u64, wall_us: u64, egress: u64, ingress: u64) {
        self.counters.increment(self.core.requests, 1);
        self.counters.increment(self.core.cpu_us, cpu_us);
        self.counters.increment(self.core.wall_us, wall_us);
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

/// Implements `PluginMeter` by looking up the current `Arc<AppMeter>` from the
/// registry on each call. Slightly slower than caching (~60ns vs ~27ns), but
/// always uses the CURRENT meter — survives period rollover without stale refs.
pub struct AppPluginMeter {
    registry: Arc<MeterRegistry>,
    app_id: String,
}

impl AppPluginMeter {
    pub fn new(registry: Arc<MeterRegistry>, app_id: String) -> Self {
        Self { registry, app_id }
    }
}

impl crate::core::plugin::PluginMeter for AppPluginMeter {
    fn increment(&self, resource_name: &str, delta: u64) {
        let meter = self.registry.get_or_create(&self.app_id);
        if let Some(handle) = meter.counters.handle_for(resource_name) {
            meter.counters.increment(handle, delta);
        }
    }
}

/// Implements `PluginQuota` by looking up the current `Arc<AppMeter>` from the
/// registry on each call. Always reads the CURRENT counter and plan values.
pub struct AppQuotaChecker {
    registry: Arc<MeterRegistry>,
    app_id: String,
}

impl AppQuotaChecker {
    pub fn new(registry: Arc<MeterRegistry>, app_id: String) -> Self {
        Self { registry, app_id }
    }
}

impl crate::core::plugin::PluginQuota for AppQuotaChecker {
    fn check(&self, resource: &str) -> Result<(), crate::core::plugin::QuotaDenied> {
        let meter = self.registry.get_or_create(&self.app_id);
        let used = meter.counters.get(resource).unwrap_or(0);

        if let Some(quota) = meter.plan.quotas.get(resource) {
            if let Some(max) = quota.max {
                if used >= max {
                    return Err(crate::core::plugin::QuotaDenied {
                        resource: resource.to_string(),
                        used,
                        limit: max,
                        message: format!("{resource} quota exceeded ({used}/{max})"),
                    });
                }
            }
        }
        Ok(())
    }
}

/// Registry of all app meters. Thread-safe, shared across handlers.
/// Uses RwLock for the map (reads dominate after warmup) + Mutex for writes.
pub struct MeterRegistry {
    meters: RwLock<HashMap<String, Arc<AppMeter>>>,
    default_plan: QuotaPlan,
    /// Plugin-declared meter resources, stored so new AppMeters include them.
    plugin_resources: Vec<MeterResource>,
}

impl MeterRegistry {
    pub fn new(default_plan: QuotaPlan, plugin_resources: Vec<MeterResource>) -> Self {
        Self {
            meters: RwLock::new(HashMap::new()),
            default_plan,
            plugin_resources,
        }
    }

    /// Get or create a meter for an app.
    /// Fast path: RwLock read (concurrent). Slow path: write lock on miss.
    pub fn get_or_create(&self, app_id: &str) -> Arc<AppMeter> {
        // Fast path: read lock (no contention with other readers)
        {
            let meters = self.meters.read().unwrap();
            if let Some(meter) = meters.get(app_id) {
                return meter.clone();
            }
        }
        // Slow path: write lock (only on first access per app)
        let mut meters = self.meters.write().unwrap();
        meters
            .entry(app_id.to_string())
            .or_insert_with(|| {
                Arc::new(AppMeter::with_resources(
                    self.default_plan.clone(),
                    &self.plugin_resources,
                ))
            })
            .clone()
    }

    /// Set a specific plan for an app, preserving existing counter values.
    ///
    /// Note: There is a small window where increments to the old meter after
    /// snapshot() but before insert() are lost. This is acceptable: the window
    /// is ~microseconds (Mutex hold time), and the warm tier has the authoritative
    /// period totals. The alternative (pausing all writes) is too expensive.
    pub fn set_plan(&self, app_id: &str, plan: QuotaPlan) {
        let mut meters = self.meters.write().unwrap();
        if let Some(old) = meters.get(app_id) {
            // Transfer counter values from old meter to new one
            let snapshot = old.counters.snapshot();
            let new_meter = AppMeter::with_resources(plan, &self.plugin_resources);
            for (name, value) in &snapshot {
                if let Some(handle) = new_meter.counters.handle_for(name) {
                    new_meter.counters.increment(handle, *value);
                }
            }
            // Sync last_flushed so first flush doesn't re-flush transferred values
            new_meter.counters.sync_last_flushed();
            meters.insert(app_id.to_string(), Arc::new(new_meter));
        } else {
            meters.insert(
                app_id.to_string(),
                Arc::new(AppMeter::with_resources(plan, &self.plugin_resources)),
            );
        }
    }

    /// Get usage snapshot for an app. Returns None if no meter exists.
    pub fn get_usage(&self, app_id: &str) -> Option<HashMap<String, u64>> {
        let meters = self.meters.read().unwrap();
        meters.get(app_id).map(|m| m.snapshot())
    }

    /// Get usage snapshots for all apps.
    pub fn all_usage(&self) -> HashMap<String, HashMap<String, u64>> {
        let meters = self.meters.read().unwrap();
        meters
            .iter()
            .map(|(id, m)| (id.clone(), m.snapshot()))
            .collect()
    }

    /// Reset a specific app's usage counters.
    pub fn reset(&self, app_id: &str) {
        let meters = self.meters.read().unwrap();
        if let Some(meter) = meters.get(app_id) {
            meter.reset_period();
        }
    }

    /// Remove an app's meter entirely.
    pub fn remove(&self, app_id: &str) {
        let mut meters = self.meters.write().unwrap();
        meters.remove(app_id);
    }

    /// Get all meters (for flusher).
    pub fn all_meters(&self) -> HashMap<String, Arc<AppMeter>> {
        let meters = self.meters.read().unwrap();
        meters.clone()
    }

    /// Atomically swap an app's meter for a fresh one (period rollover).
    /// Returns the old meter (for draining and reading), or None if app not found.
    pub fn swap_for_rollover(&self, app_id: &str) -> Option<Arc<AppMeter>> {
        let mut meters = self.meters.write().unwrap();
        let old = meters.remove(app_id)?;
        let new = Arc::new(AppMeter::with_resources(
            old.plan.clone(),
            &self.plugin_resources,
        ));
        meters.insert(app_id.to_string(), new);
        Some(old)
    }

    /// Recover counters from the warm tier after a restart (spec §4.6).
    /// For each app in the store, loads the stored counters and populates
    /// the hot-tier atomics so enforcement resumes from the last-flushed state.
    pub async fn recover_from_store(
        &self,
        store: &dyn crate::core::meter_store::MeterStore,
        app_ids: &[String],
    ) {
        for app_id in app_ids {
            match store.load(app_id).await {
                Ok(stored) if !stored.is_empty() => {
                    let meter = Arc::new(AppMeter::from_stored(
                        self.default_plan.clone(),
                        &self.plugin_resources,
                        &stored,
                    ));
                    self.meters.write().unwrap().insert(app_id.clone(), meter);
                    eprintln!("[metering] Recovered counters for {app_id}: {} resources", stored.len());
                }
                Ok(_) => {} // empty, no recovery needed
                Err(e) => eprintln!("[metering] Failed to recover {app_id}: {e}"),
            }
        }
    }
}

impl crate::core::billing::MeteringSnapshot for MeterRegistry {
    fn snapshot(&self, app_id: &str) -> Option<HashMap<String, u64>> {
        let meters = self.meters.read().unwrap();
        meters.get(app_id).map(|m| m.counters.snapshot())
    }

    fn active_apps(&self) -> Vec<String> {
        self.meters.read().unwrap().keys().cloned().collect()
    }
}

impl crate::core::billing::SpendEnforcement for MeterRegistry {
    fn set_spend_action(
        &self,
        app_id: &str,
        action: crate::core::billing::SpendAction,
    ) {
        let meters = self.meters.read().unwrap();
        if let Some(meter) = meters.get(app_id) {
            crate::core::billing::SpendAction::store(&meter.spend_action, action);
        }
    }

    fn get_spend_action(&self, app_id: &str) -> crate::core::billing::SpendAction {
        let meters = self.meters.read().unwrap();
        meters
            .get(app_id)
            .map(|m| crate::core::billing::SpendAction::load(&m.spend_action))
            .unwrap_or(crate::core::billing::SpendAction::Allow)
    }
}
