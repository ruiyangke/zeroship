//! In-memory MeterStore adapter — for development and testing.
//!
//! Stores counters in a HashMap behind a Mutex. No persistence — data is
//! lost on restart. Fast, zero external dependencies.

use appbase_core::meter_store::{MeterStore, MeterStoreError, PeriodSnapshot, ResourceDelta};
use std::collections::HashMap;
use std::sync::Mutex;

/// In-memory store: counters + history in HashMaps.
pub struct InMemoryStore {
    /// Current period counters: app_id → (resource → value)
    counters: Mutex<HashMap<String, HashMap<String, u64>>>,
    /// Historical snapshots: app_id → [PeriodSnapshot]
    history: Mutex<HashMap<String, Vec<PeriodSnapshot>>>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self {
            counters: Mutex::new(HashMap::new()),
            history: Mutex::new(HashMap::new()),
        }
    }
}

impl MeterStore for InMemoryStore {
    fn flush(&self, app_id: &str, deltas: &[ResourceDelta]) -> Result<(), MeterStoreError> {
        let mut counters = self.counters.lock().unwrap();
        let app = counters.entry(app_id.to_string()).or_default();
        for delta in deltas {
            *app.entry(delta.resource.clone()).or_insert(0) += delta.delta;
        }
        Ok(())
    }

    fn load(&self, app_id: &str) -> Result<HashMap<String, u64>, MeterStoreError> {
        let counters = self.counters.lock().unwrap();
        Ok(counters.get(app_id).cloned().unwrap_or_default())
    }

    fn rollover(&self, app_id: &str) -> Result<PeriodSnapshot, MeterStoreError> {
        let mut counters = self.counters.lock().unwrap();
        let current = counters.remove(app_id).unwrap_or_default();

        let period = chrono_period();
        let snapshot = PeriodSnapshot {
            app_id: app_id.to_string(),
            period: period.clone(),
            counters: current,
        };

        // Store in history
        let mut history = self.history.lock().unwrap();
        history
            .entry(app_id.to_string())
            .or_default()
            .push(snapshot.clone());

        Ok(snapshot)
    }

    fn history(&self, app_id: &str, periods: u32) -> Result<Vec<PeriodSnapshot>, MeterStoreError> {
        let history = self.history.lock().unwrap();
        let all = history.get(app_id).cloned().unwrap_or_default();
        let start = all.len().saturating_sub(periods as usize);
        Ok(all[start..].to_vec())
    }

    fn close(&self) -> Result<(), MeterStoreError> {
        Ok(()) // nothing to clean up
    }
}

/// Generate a period string like "2026-03".
fn chrono_period() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!("{}-{:02}", now.year(), now.month() as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flush_and_load() {
        let store = InMemoryStore::new();
        store
            .flush(
                "app1",
                &[
                    ResourceDelta {
                        resource: "requests".into(),
                        delta: 100,
                    },
                    ResourceDelta {
                        resource: "cpu_ms".into(),
                        delta: 50,
                    },
                ],
            )
            .unwrap();

        let counters = store.load("app1").unwrap();
        assert_eq!(counters["requests"], 100);
        assert_eq!(counters["cpu_ms"], 50);
    }

    #[test]
    fn flush_is_additive() {
        let store = InMemoryStore::new();
        store
            .flush(
                "app1",
                &[ResourceDelta {
                    resource: "requests".into(),
                    delta: 10,
                }],
            )
            .unwrap();
        store
            .flush(
                "app1",
                &[ResourceDelta {
                    resource: "requests".into(),
                    delta: 20,
                }],
            )
            .unwrap();

        let counters = store.load("app1").unwrap();
        assert_eq!(counters["requests"], 30);
    }

    #[test]
    fn rollover_resets_and_stores_history() {
        let store = InMemoryStore::new();
        store
            .flush(
                "app1",
                &[ResourceDelta {
                    resource: "requests".into(),
                    delta: 42,
                }],
            )
            .unwrap();

        let snapshot = store.rollover("app1").unwrap();
        assert_eq!(snapshot.counters["requests"], 42);

        // Counters should be reset
        let counters = store.load("app1").unwrap();
        assert!(counters.is_empty());

        // History should have 1 entry
        let hist = store.history("app1", 10).unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].counters["requests"], 42);
    }

    #[test]
    fn load_unknown_app_returns_empty() {
        let store = InMemoryStore::new();
        let counters = store.load("nonexistent").unwrap();
        assert!(counters.is_empty());
    }
}
