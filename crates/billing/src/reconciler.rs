//! SpendingReconciler — background service that computes spend and sets SpendAction.
//!
//! Runs every N seconds (default 10s). For each app with a spending limit:
//! 1. Reads counter snapshot from metering (via MeteringSnapshot trait)
//! 2. Computes cost via PricingTable
//! 3. Compares against spending limit
//! 4. Sets SpendAction on the AppMeter (via SpendEnforcement trait)

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::pricing::PricingTable;
use crate::spend_action::SpendAction;

/// Interface to read metering data (implemented by quota crate).
pub trait MeteringSnapshot: Send + Sync {
    /// Get all counter values for an app.
    fn snapshot(&self, app_id: &str) -> Option<HashMap<String, u64>>;
    /// List all active app IDs.
    fn active_apps(&self) -> Vec<String>;
}

/// Interface to set spending enforcement (implemented by quota crate).
pub trait SpendEnforcement: Send + Sync {
    fn set_spend_action(&self, app_id: &str, action: SpendAction);
    fn get_spend_action(&self, app_id: &str) -> SpendAction;
}

/// Per-app spending configuration.
#[derive(Debug, Clone)]
pub struct SpendingLimit {
    /// Limit in millicents. None = no limit.
    pub limit_millicents: Option<u64>,
    /// Thresholds for warn/degrade/block (as percentages).
    pub warn_pct: u64,       // default 80
    pub degrade_pct: u64,    // default 95 (0 = skip degrade)
    pub block_pct: u64,      // default 100
}

impl Default for SpendingLimit {
    fn default() -> Self {
        Self {
            limit_millicents: None,
            warn_pct: 80,
            degrade_pct: 0, // 0 = no degrade step
            block_pct: 100,
        }
    }
}

/// Configuration for the SpendingReconciler.
#[derive(Debug)]
pub struct ReconcilerConfig {
    pub interval: Duration,
    pub pricing: Arc<PricingTable>,
    /// App ID → spending limit config.
    pub limits: HashMap<String, SpendingLimit>,
}

/// Spawn the SpendingReconciler background task.
pub fn spawn_reconciler(
    config: ReconcilerConfig,
    metering: Arc<dyn MeteringSnapshot>,
    enforcement: Arc<dyn SpendEnforcement>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(config.interval).await;
            reconcile(&config, metering.as_ref(), enforcement.as_ref());
        }
    })
}

/// Run one reconciliation cycle.
fn reconcile(
    config: &ReconcilerConfig,
    metering: &dyn MeteringSnapshot,
    enforcement: &dyn SpendEnforcement,
) {
    for (app_id, limit) in &config.limits {
        let limit_mc = match limit.limit_millicents {
            Some(lim) => lim,
            None => {
                // No spending limit — ensure allow
                enforcement.set_spend_action(app_id, SpendAction::Allow);
                continue;
            }
        };

        let usage = match metering.snapshot(app_id) {
            Some(u) => u,
            None => continue, // app not active
        };

        let cost_mc = config.pricing.compute_cost(&usage);
        let pct = if limit_mc > 0 {
            (cost_mc as f64 / limit_mc as f64) * 100.0
        } else if cost_mc > 0 {
            100.0
        } else {
            0.0
        };

        let action = if pct >= limit.block_pct as f64 {
            SpendAction::Block
        } else if limit.degrade_pct > 0 && pct >= limit.degrade_pct as f64 {
            SpendAction::Degrade
        } else if pct >= limit.warn_pct as f64 {
            SpendAction::Warn
        } else {
            SpendAction::Allow
        };

        enforcement.set_spend_action(app_id, action);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct MockMetering {
        data: HashMap<String, HashMap<String, u64>>,
    }
    impl MeteringSnapshot for MockMetering {
        fn snapshot(&self, app_id: &str) -> Option<HashMap<String, u64>> {
            self.data.get(app_id).cloned()
        }
        fn active_apps(&self) -> Vec<String> {
            self.data.keys().cloned().collect()
        }
    }

    struct MockEnforcement {
        actions: Mutex<HashMap<String, SpendAction>>,
    }
    impl MockEnforcement {
        fn new() -> Self {
            Self { actions: Mutex::new(HashMap::new()) }
        }
    }
    impl SpendEnforcement for MockEnforcement {
        fn set_spend_action(&self, app_id: &str, action: SpendAction) {
            self.actions.lock().unwrap().insert(app_id.to_string(), action);
        }
        fn get_spend_action(&self, app_id: &str) -> SpendAction {
            self.actions.lock().unwrap().get(app_id).copied().unwrap_or(SpendAction::Allow)
        }
    }

    #[test]
    fn under_limit_allows() {
        let pricing = Arc::new(PricingTable::cloudflare_comparable());
        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), 100_000); // tiny usage
        let mut data = HashMap::new();
        data.insert("app1".to_string(), usage);

        let metering = MockMetering { data };
        let enforcement = MockEnforcement::new();

        let mut limits = HashMap::new();
        limits.insert("app1".to_string(), SpendingLimit {
            limit_millicents: Some(500_000), // $5.00
            ..Default::default()
        });

        let config = ReconcilerConfig {
            interval: Duration::from_secs(10),
            pricing,
            limits,
        };

        reconcile(&config, &metering, &enforcement);
        assert_eq!(enforcement.get_spend_action("app1"), SpendAction::Allow);
    }

    #[test]
    fn over_limit_blocks() {
        let pricing = Arc::new(PricingTable::cloudflare_comparable());
        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), 100_000_000); // 100M requests
        let mut data = HashMap::new();
        data.insert("app1".to_string(), usage);

        let metering = MockMetering { data };
        let enforcement = MockEnforcement::new();

        let mut limits = HashMap::new();
        limits.insert("app1".to_string(), SpendingLimit {
            limit_millicents: Some(10_000), // $0.10 limit, way under cost
            ..Default::default()
        });

        let config = ReconcilerConfig {
            interval: Duration::from_secs(10),
            pricing,
            limits,
        };

        reconcile(&config, &metering, &enforcement);
        assert_eq!(enforcement.get_spend_action("app1"), SpendAction::Block);
    }

    #[test]
    fn warn_threshold() {
        let mut table = PricingTable::new();
        table.add_flat("requests", 1000, 1_000_000); // $1/million
        let pricing = Arc::new(table);

        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), 850_000); // 850 millicents cost
        let mut data = HashMap::new();
        data.insert("app1".to_string(), usage);

        let metering = MockMetering { data };
        let enforcement = MockEnforcement::new();

        let mut limits = HashMap::new();
        limits.insert("app1".to_string(), SpendingLimit {
            limit_millicents: Some(1000), // 1000 millicents limit
            warn_pct: 80,
            degrade_pct: 0,
            block_pct: 100,
        });

        let config = ReconcilerConfig {
            interval: Duration::from_secs(10),
            pricing,
            limits,
        };

        reconcile(&config, &metering, &enforcement);
        assert_eq!(enforcement.get_spend_action("app1"), SpendAction::Warn);
    }

    #[test]
    fn no_limit_always_allows() {
        let pricing = Arc::new(PricingTable::cloudflare_comparable());
        let metering = MockMetering { data: HashMap::new() };
        let enforcement = MockEnforcement::new();

        let mut limits = HashMap::new();
        limits.insert("app1".to_string(), SpendingLimit {
            limit_millicents: None,
            ..Default::default()
        });

        let config = ReconcilerConfig {
            interval: Duration::from_secs(10),
            pricing,
            limits,
        };

        reconcile(&config, &metering, &enforcement);
        assert_eq!(enforcement.get_spend_action("app1"), SpendAction::Allow);
    }
}
