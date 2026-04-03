//! Billing ↔ Metering interface traits.
//!
//! Defined in core so that both the billing crate and the metering crate
//! can depend on these without creating a cycle. Billing writes SpendAction,
//! metering reads it. Neither imports the other.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};

/// Spending enforcement action, stored as AtomicU8.
/// Set by the billing reconciler, read by the enforcement pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SpendAction {
    /// Normal operation — no spending concern.
    Allow = 0,
    /// Approaching limit — add warning header, allow request.
    Warn = 1,
    /// At limit — apply degraded limits.
    Degrade = 2,
    /// Over limit — reject request.
    Block = 3,
}

impl SpendAction {
    /// Convert from raw u8 (unknown values map to Block).
    pub fn from_u8(val: u8) -> Self {
        match val {
            0 => Self::Allow,
            1 => Self::Warn,
            2 => Self::Degrade,
            3 => Self::Block,
            _ => Self::Block,
        }
    }

    /// Load from an AtomicU8.
    #[inline]
    pub fn load(atom: &AtomicU8) -> Self {
        Self::from_u8(atom.load(Ordering::Acquire))
    }

    /// Store into an AtomicU8.
    #[inline]
    pub fn store(atom: &AtomicU8, action: Self) {
        atom.store(action as u8, Ordering::Release);
    }
}

/// Interface to read metering data. Implemented by the metering crate.
pub trait MeteringSnapshot: Send + Sync {
    /// Get all counter values for an app.
    fn snapshot(&self, app_id: &str) -> Option<HashMap<String, u64>>;
    /// List all active app IDs.
    fn active_apps(&self) -> Vec<String>;
}

/// Interface to set spending enforcement. Implemented by the metering crate.
pub trait SpendEnforcement: Send + Sync {
    fn set_spend_action(&self, app_id: &str, action: SpendAction);
    fn get_spend_action(&self, app_id: &str) -> SpendAction;
}
