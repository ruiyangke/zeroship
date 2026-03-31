//! CAS-based concurrency guard — RAII pattern for counting active requests.
//!
//! Uses compare_exchange (not fetch_add) to atomically check the limit
//! and increment in one operation. Drop decrements the counter.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// RAII guard — increments gauge on creation, decrements on drop.
pub struct ConcurrencyGuard {
    gauge: Arc<AtomicU32>,
}

impl ConcurrencyGuard {
    /// Try to acquire a slot. Returns None if at capacity.
    /// Uses CAS loop to atomically check limit + increment.
    pub fn try_acquire(gauge: &Arc<AtomicU32>, limit: u32) -> Option<Self> {
        loop {
            let current = gauge.load(Ordering::Acquire);
            if current >= limit {
                return None;
            }
            if gauge
                .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(Self {
                    gauge: gauge.clone(),
                });
            }
            // CAS failed — another thread changed the value, retry
        }
    }

    /// Current count (for monitoring).
    pub fn current(gauge: &AtomicU32) -> u32 {
        gauge.load(Ordering::Acquire)
    }
}

impl Drop for ConcurrencyGuard {
    fn drop(&mut self) {
        self.gauge.fetch_sub(1, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_up_to_limit() {
        let gauge = Arc::new(AtomicU32::new(0));
        let g1 = ConcurrencyGuard::try_acquire(&gauge, 2).unwrap();
        let g2 = ConcurrencyGuard::try_acquire(&gauge, 2).unwrap();
        assert_eq!(ConcurrencyGuard::current(&gauge), 2);
        assert!(ConcurrencyGuard::try_acquire(&gauge, 2).is_none());
        drop(g1);
        assert_eq!(ConcurrencyGuard::current(&gauge), 1);
        let _g3 = ConcurrencyGuard::try_acquire(&gauge, 2).unwrap();
        assert_eq!(ConcurrencyGuard::current(&gauge), 2);
        drop(g2);
    }

    #[test]
    fn drop_decrements() {
        let gauge = Arc::new(AtomicU32::new(0));
        {
            let _g = ConcurrencyGuard::try_acquire(&gauge, 10).unwrap();
            assert_eq!(ConcurrencyGuard::current(&gauge), 1);
        }
        assert_eq!(ConcurrencyGuard::current(&gauge), 0);
    }

    #[test]
    fn deny_at_limit() {
        let gauge = Arc::new(AtomicU32::new(0));
        let _guards: Vec<_> = (0..5)
            .map(|_| ConcurrencyGuard::try_acquire(&gauge, 5).unwrap())
            .collect();
        assert!(ConcurrencyGuard::try_acquire(&gauge, 5).is_none());
    }
}
