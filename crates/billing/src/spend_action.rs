//! SpendAction — the interface between billing and quota enforcement.
//!
//! The billing crate WRITES this flag (via SpendingReconciler).
//! The quota crate READS this flag (single AtomicU8 load, <1ns).

use std::sync::atomic::{AtomicU8, Ordering};

/// Spending enforcement action, stored as AtomicU8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SpendAction {
    /// Normal operation — no spending concern.
    Allow = 0,
    /// Approaching limit — add warning header, allow request.
    Warn = 1,
    /// At limit — apply degraded limits (reduced quotas/rate limits).
    Degrade = 2,
    /// Over limit — reject request.
    Block = 3,
}

impl SpendAction {
    /// Convert from raw u8 (safe: unknown values map to Block).
    pub fn from_u8(val: u8) -> Self {
        match val {
            0 => Self::Allow,
            1 => Self::Warn,
            2 => Self::Degrade,
            3 => Self::Block,
            _ => Self::Block, // defensive: unknown = block
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for action in [SpendAction::Allow, SpendAction::Warn, SpendAction::Degrade, SpendAction::Block] {
            assert_eq!(SpendAction::from_u8(action as u8), action);
        }
    }

    #[test]
    fn unknown_maps_to_block() {
        assert_eq!(SpendAction::from_u8(255), SpendAction::Block);
    }

    #[test]
    fn atomic_load_store() {
        let atom = AtomicU8::new(0);
        assert_eq!(SpendAction::load(&atom), SpendAction::Allow);
        SpendAction::store(&atom, SpendAction::Warn);
        assert_eq!(SpendAction::load(&atom), SpendAction::Warn);
    }
}
