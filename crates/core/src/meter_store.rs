//! MeterStore trait — pluggable warm-tier storage for metering counters.
//!
//! Implementations: InMemoryStore (dev), SqliteStore, RedisStore, PostgresStore, MmapStore.
//! Each behind a feature flag in the metering crate.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A single resource delta from a flush cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceDelta {
    pub resource: String,
    pub delta: u64,
}

/// Snapshot of an app's counters at a period boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeriodSnapshot {
    pub app_id: String,
    pub period: String,
    pub counters: HashMap<String, u64>,
}

/// Warm-tier storage for metering counters.
///
/// The hot tier (atomic counters) flushes deltas here every N seconds.
/// The admin API reads usage data from here.
/// Billing integration exports data from here.
pub trait MeterStore: Send + Sync {
    /// Flush counter deltas from hot tier. Additive (UPSERT with +=).
    fn flush(&self, app_id: &str, deltas: &[ResourceDelta]) -> Result<(), String>;

    /// Load current counters for an app (startup or cache miss).
    fn load(&self, app_id: &str) -> Result<HashMap<String, u64>, String>;

    /// Snapshot current period counters and reset for new period.
    /// Returns the snapshot of the completed period.
    fn rollover(&self, app_id: &str) -> Result<PeriodSnapshot, String>;

    /// Query usage history (past N periods) for billing.
    fn history(&self, app_id: &str, periods: u32) -> Result<Vec<PeriodSnapshot>, String>;

    /// Graceful cleanup (close connections, flush buffers).
    fn close(&self) -> Result<(), String>;
}
