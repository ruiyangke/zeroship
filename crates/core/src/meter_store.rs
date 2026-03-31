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
///
/// # Thread Safety
///
/// Implementations must be `Send + Sync`. All methods take `&self` and must
/// handle internal synchronization.
///
/// # Atomicity Requirements
///
/// - `rollover()` MUST be atomic: snapshot-and-reset in a single transaction
///   or under an exclusive per-app lock. If two callers race on `rollover`
///   for the same app, only one should succeed; the other should return the
///   existing snapshot or a no-op result.
/// - `flush()` is additive and idempotent for the same delta set.
pub trait MeterStore: Send + Sync {
    /// Flush counter deltas from hot tier. Additive (UPSERT with +=).
    fn flush(&self, app_id: &str, deltas: &[ResourceDelta]) -> Result<(), MeterStoreError>;

    /// Load current counters for an app (startup or cache miss).
    fn load(&self, app_id: &str) -> Result<HashMap<String, u64>, MeterStoreError>;

    /// Atomically snapshot current period counters and reset for new period.
    /// Returns the snapshot of the completed period.
    ///
    /// Implementations MUST ensure this is atomic — no concurrent flush can
    /// land between the snapshot and the reset. Use a per-app lock or
    /// database transaction.
    fn rollover(&self, app_id: &str) -> Result<PeriodSnapshot, MeterStoreError>;

    /// Query usage history (past N periods) for billing.
    fn history(&self, app_id: &str, periods: u32) -> Result<Vec<PeriodSnapshot>, MeterStoreError>;

    /// Graceful cleanup (close connections, flush buffers).
    /// Called once during shutdown. Implementations should flush
    /// any pending writes before returning.
    fn close(&self) -> Result<(), MeterStoreError>;
}

/// Errors from MeterStore operations.
#[derive(Debug)]
pub enum MeterStoreError {
    /// The requested app was not found.
    NotFound(String),
    /// An I/O or connection error occurred.
    Io(String),
    /// Any other error.
    Other(String),
}

impl std::fmt::Display for MeterStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(msg) => write!(f, "not found: {msg}"),
            Self::Io(msg) => write!(f, "I/O error: {msg}"),
            Self::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl From<String> for MeterStoreError {
    fn from(s: String) -> Self {
        Self::Other(s)
    }
}
