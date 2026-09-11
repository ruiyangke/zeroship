//! Advisory-lock registry shared by callers of a SQLite backend instance.
//!
//! This registry does not coordinate separate backend instances or processes.
//! Borrow the registry synchronously; release the borrow before awaiting retries.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// Lock state keyed by the app and scope name.
type LockSlots = HashMap<(String, String), Rc<Cell<bool>>>;

/// Per-process advisory-lock registry. One instance per
/// [`super::SqliteBackend`].
#[derive(Default)]
pub(crate) struct InProcessLockRegistry {
    slots: RefCell<LockSlots>,
}

impl InProcessLockRegistry {
    /// Construct an empty registry.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Acquire an unheld slot without waiting. The registry borrow ends on return.
    pub(crate) fn try_acquire(&self, key: (String, String)) -> bool {
        let mut slots = self.slots.borrow_mut();
        let slot = slots
            .entry(key)
            .or_insert_with(|| Rc::new(Cell::new(false)));
        if slot.get() {
            // Already held by another acquirer.
            false
        } else {
            slot.set(true);
            true
        }
    }

    /// Release the slot. An unknown or unheld slot logs a warning and does nothing.
    pub(crate) fn release(&self, key: (String, String)) {
        let mut slots = self.slots.borrow_mut();
        match slots.get(&key).cloned() {
            Some(slot) if slot.get() => {
                slot.set(false);
                // `slot` itself is a cloned `Rc` from the map lookup,
                // so an unshared entry has strong_count == 2 here:
                // one owner in the HashMap, one in this stack frame.
                if Rc::strong_count(&slot) == 2 {
                    slots.remove(&key);
                }
            }
            Some(_) => {
                tracing::warn!(
                    key1 = %key.0,
                    key2 = %key.1,
                    "InProcessLockRegistry::release called on a registered but currently \
                     unheld slot (double-release or release-without-acquire); no-op"
                );
            }
            None => {
                tracing::warn!(
                    key1 = %key.0,
                    key2 = %key.1,
                    "InProcessLockRegistry::release called on an unknown slot \
                     (release-without-acquire); no-op"
                );
            }
        }
    }
}

/// RAII guard for an acquired SQLite in-process advisory lock.
///
/// Unlike the PG lock guards, release is synchronous: the lock lives in
/// an in-memory registry, so `Drop` can free it immediately without any
/// async unlock SQL. The critical failure mode is "acquire succeeded, then an
/// early return / panic / cancellation leaked the slot forever".
#[cfg(test)]
#[must_use = "SqliteLockGuard releases the registry slot on Drop"]
pub(crate) struct SqliteLockGuard {
    registry: Rc<InProcessLockRegistry>,
    key: Option<(String, String)>,
}

#[cfg(test)]
impl Drop for SqliteLockGuard {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            self.registry.release(key);
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the in-process advisory-lock registry. Each test
    //! exercises one of the four `InProcessLockRegistry` state
    //! transitions: first acquire / contended acquire / release-unblocks /
    //! release-on-unheld no-panic. The borrow-across-await discipline is
    //! not testable at unit level (it's a compile-time property of the
    //! consumer); the higher-level integration tests in
    //! `crates/zeroship-data-orm/src/tests/sqlite/locking.rs` exercise the actor-level borrow
    //! discipline end-to-end.

    use super::*;

    fn key(k1: &str, k2: &str) -> (String, String) {
        (k1.to_string(), k2.to_string())
    }

    #[test]
    fn try_acquire_returns_true_on_first() {
        let reg = InProcessLockRegistry::new();
        assert!(
            reg.try_acquire(key("app_demo:snapshot_restore", "snapshot_restore")),
            "first acquire on an empty registry must return true"
        );
    }

    #[test]
    fn try_acquire_returns_false_when_held() {
        let reg = InProcessLockRegistry::new();
        let k = key("app_demo:snapshot_restore", "snapshot_restore");
        assert!(reg.try_acquire(k.clone()));
        assert!(
            !reg.try_acquire(k),
            "second acquire on the same slot must return false (already held)"
        );
    }

    #[test]
    fn release_unblocks() {
        let reg = InProcessLockRegistry::new();
        let k = key("app_demo:snapshot_restore", "snapshot_restore");
        assert!(reg.try_acquire(k.clone()));
        reg.release(k.clone());
        assert!(
            reg.try_acquire(k),
            "re-acquire after release must return true (slot freed)"
        );
    }

    #[test]
    fn release_drops_unshared_slot_from_registry() {
        let reg = InProcessLockRegistry::new();
        let k = key("app_demo:snapshot_restore", "snapshot_restore");
        assert!(reg.try_acquire(k.clone()));
        assert_eq!(reg.slots.borrow().len(), 1, "slot must exist after acquire");
        reg.release(k);
        assert!(
            reg.slots.borrow().is_empty(),
            "release must evict the now-unheld slot so the registry cannot grow unbounded"
        );
    }

    #[test]
    fn release_on_unheld_warns_no_panic() {
        let reg = InProcessLockRegistry::new();
        // Release on an unknown slot — no-op (warns).
        reg.release(key("app_demo:never_acquired", "never_acquired"));

        // Release on a known but currently-unheld slot — no-op (warns).
        let k = key("app_demo:snapshot_restore", "snapshot_restore");
        assert!(reg.try_acquire(k.clone()));
        reg.release(k.clone());
        // Second release on the same key — slot is registered but
        // unheld; must not panic.
        reg.release(k.clone());

        // The slot is still re-acquirable after the redundant release.
        assert!(reg.try_acquire(k));
    }

    #[test]
    fn distinct_keys_do_not_interfere() {
        let reg = InProcessLockRegistry::new();
        let a = key("app_a:scope", "scope");
        let b = key("app_b:scope", "scope");
        assert!(reg.try_acquire(a.clone()));
        // Same `scope` name but a different `app_id` prefix — the
        // (key1, key2) pair differs, so the registry treats them as
        // independent slots.
        assert!(
            reg.try_acquire(b),
            "distinct (key1, key2) pairs must hold independent slots"
        );
        // The first slot is still held.
        assert!(!reg.try_acquire(a));
    }

    #[test]
    fn sqlite_lock_guard_drop_releases_slot() {
        let reg = Rc::new(InProcessLockRegistry::new());
        let k = key("app_demo:snapshot_restore", "snapshot_restore");
        assert!(reg.try_acquire(k.clone()), "precondition: slot acquired");

        let guard = SqliteLockGuard {
            registry: Rc::clone(&reg),
            key: Some(k.clone()),
        };
        drop(guard);

        assert!(
            reg.try_acquire(k),
            "dropping SqliteLockGuard must free the registry slot"
        );
    }

    #[test]
    fn sqlite_lock_guard_releases_slot_on_panic_unwind() {
        let reg = Rc::new(InProcessLockRegistry::new());
        let k = key("app_demo:snapshot_restore", "snapshot_restore");
        assert!(reg.try_acquire(k.clone()), "precondition: slot acquired");

        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe({
            let reg = Rc::clone(&reg);
            let k = k.clone();
            move || {
                let _guard = SqliteLockGuard {
                    registry: reg,
                    key: Some(k),
                };
                panic!("simulated mid-critical-section panic");
            }
        }));
        std::panic::set_hook(prev_hook);
        assert!(result.is_err(), "guard fixture must panic");

        assert!(
            reg.try_acquire(k),
            "unwinding through SqliteLockGuard must still release the slot"
        );
    }
}
