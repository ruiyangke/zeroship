//! Event log trait — append-only cold-tier storage for billing reconciliation.
//!
//! The event log is the source of truth for billing. If hot-tier deltas are lost
//! during a flusher failure, the event log can reconstruct accurate totals.

use serde::{Deserialize, Serialize};
use std::time::SystemTime;

/// A metering event to be logged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeterEvent {
    /// Monotonic event ID (assigned by the logger).
    pub id: u64,
    /// When the event occurred.
    pub timestamp: SystemTime,
    /// Which app generated the event.
    pub app_id: String,
    /// Event type.
    pub kind: EventKind,
    /// Optional structured payload.
    pub payload: serde_json::Value,
}

/// Types of metering events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventKind {
    /// A request was processed with usage deltas.
    RequestCompleted,
    /// Quota check resulted in a warning.
    QuotaWarning,
    /// Quota check resulted in a denial.
    QuotaDenied,
    /// Rate limit triggered.
    RateLimited,
    /// Period rollover completed.
    PeriodRollover,
    /// Plan was changed.
    PlanChanged,
    /// Spending limit reached.
    SpendingLimitHit,
    /// Spending limit reset.
    SpendingLimitReset,
    /// App created.
    AppCreated,
    /// App deleted.
    AppDeleted,
}

/// Append-only event log for billing reconciliation.
///
/// Implementations must be `Send + Sync`. Events are never modified or deleted
/// (append-only). The log is the source of truth for billing disputes.
pub trait EventLog: Send + Sync {
    /// Append an event. Returns the assigned event ID.
    fn append(&self, event: MeterEvent) -> Result<u64, EventLogError>;

    /// Query events for an app within a time range.
    fn query(
        &self,
        app_id: &str,
        since: SystemTime,
        until: SystemTime,
        limit: usize,
    ) -> Result<Vec<MeterEvent>, EventLogError>;

    /// Get the latest event ID (for cursor-based consumption).
    fn latest_id(&self) -> Result<u64, EventLogError>;

    /// Graceful shutdown — flush any buffered events.
    fn close(&self) -> Result<(), EventLogError>;
}

/// Errors from EventLog operations.
#[derive(Debug)]
pub enum EventLogError {
    /// An I/O error.
    Io(String),
    /// Any other error.
    Other(String),
}

impl std::fmt::Display for EventLogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(msg) => write!(f, "I/O error: {msg}"),
            Self::Other(msg) => write!(f, "{msg}"),
        }
    }
}
