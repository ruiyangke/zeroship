//! In-process advisory-lock registry.
//!
//! **P1 PR 4** wires the body of the [`InProcessLockRegistry`] —
//! the single-process HashMap [`super::SqliteBackend`]'s
//! [`crate::backend::LockManager`] impl routes through. Per design §8.5,
//! SQLite is in-process by definition; both
//! [`crate::backend::LockScope::GlobalApp`] and
//! [`crate::backend::LockScope::LocalApp`] route through this registry —
//! cross-process serialisation (BEGIN IMMEDIATE / sentinel table) is a
//! P5+ concern.
//!
//! **Storage shape** (design §8.5):
//! `RefCell<HashMap<(String, String), Rc<Cell<bool>>>>` — the
//! `(key1, key2)` pair is the [`crate::backend::LockScope::to_keys`]
//! derivation; the inner `Cell<bool>` tracks "currently held". The
//! outer `Rc<Cell<…>>` is deliberate: a later `SqliteLockGuard` sibling
//! can clone the `Rc` at acquisition time and release on drop without
//! re-walking the map. PR 4 only exercises the primitive surface
//! (`try_acquire` / `release`); the guard sibling lands separately.
//!
//! **Cell-borrow discipline**: every `try_acquire` / `release` call
//! must complete the `RefCell::borrow_mut()` scope synchronously —
//! NEVER hold the borrow across an `.await`. The [`super::SqliteBackend`]
//! [`crate::backend::LockManager`] impl calls these primitives inside
//! a single statement so the borrow lifetime is the statement scope,
//! and any sleep / backoff happens *outside* the borrow.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// Per-process advisory-lock registry. One instance per
/// [`super::SqliteBackend`].
#[derive(Default)]
pub(crate) struct InProcessLockRegistry {
    /// `(key1, key2) -> Rc<Cell<bool>>` — keyed on the
    /// [`crate::backend::LockScope::to_keys`] derivation; the bool
    /// records "currently held". `Rc<Cell<…>>` so a future
    /// `SqliteLockGuard` can clone the slot at acquire time and release
    /// on drop without re-walking the map.
    slots: RefCell<HashMap<(String, String), Rc<Cell<bool>>>>,
}

impl InProcessLockRegistry {
    /// Construct an empty registry.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Try to acquire the slot keyed by `(key1, key2)`.
    ///
    /// Returns `true` on the first acquire (and on every re-acquire
    /// after a [`Self::release`]); returns `false` if the slot is
    /// currently held. **Synchronous** — borrows the `RefCell` for the
    /// scope of the call only, never holds the borrow across an
    /// `.await`.
    ///
    /// Idempotency note: the registry creates a slot on first sight of
    /// a key pair. Calls that find an absent slot insert a fresh
    /// `Rc<Cell<true>>` and return `true`; calls that find a present
    /// slot read its `bool` and flip false→true (returning `true`) or
    /// leave it true (returning `false`).
    pub(crate) fn try_acquire(&self, key: (String, String)) -> bool {
        let mut slots = self.slots.borrow_mut();
        let slot = slots.entry(key).or_insert_with(|| Rc::new(Cell::new(false)));
        if slot.get() {
            // Already held by another acquirer.
            false
        } else {
            slot.set(true);
            true
        }
    }

    /// Release the slot keyed by `(key1, key2)`.
    ///
    /// Releasing an unheld slot is a no-op + a `tracing::warn` (same
    /// shape as the F1-family release-on-unheld trace on the PG side
    /// — see the PG `LockManager::release_advisory_lock`'s logging).
    /// Never panics; the design accepts a redundant release at the
    /// observability layer because the alternative (a typed error)
    /// would force every `release` call site to handle a "wasn't held
    /// anyway" branch with no semantic difference.
    pub(crate) fn release(&self, key: (String, String)) {
        // `borrow()` suffices — we mutate the inner `Cell<bool>`, not
        // the outer `HashMap`. The `Cell` mutation goes through
        // interior mutability, so an immutable borrow of the map is
        // enough to read the slot pointer and flip its bool.
        let slots = self.slots.borrow();
        match slots.get(&key) {
            Some(slot) if slot.get() => {
                slot.set(false);
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

#[cfg(test)]
mod tests {
    //! Unit tests for the in-process advisory-lock registry. Each test
    //! exercises one of the four [`InProcessLockRegistry`] state
    //! transitions: first acquire / contended acquire / release-unblocks /
    //! release-on-unheld no-panic. The borrow-across-await discipline is
    //! not testable at unit level (it's a compile-time property of the
    //! consumer); the higher-level integration tests in
    //! `tests/sqlite_integration.rs` exercise the actor-level borrow
    //! discipline end-to-end.

    use super::*;

    fn key(k1: &str, k2: &str) -> (String, String) {
        (k1.to_string(), k2.to_string())
    }

    #[test]
    fn try_acquire_returns_true_on_first() {
        let reg = InProcessLockRegistry::new();
        assert!(
            reg.try_acquire(key("app_demo:register_model", "register_model")),
            "first acquire on an empty registry must return true"
        );
    }

    #[test]
    fn try_acquire_returns_false_when_held() {
        let reg = InProcessLockRegistry::new();
        let k = key("app_demo:register_model", "register_model");
        assert!(reg.try_acquire(k.clone()));
        assert!(
            !reg.try_acquire(k),
            "second acquire on the same slot must return false (already held)"
        );
    }

    #[test]
    fn release_unblocks() {
        let reg = InProcessLockRegistry::new();
        let k = key("app_demo:register_model", "register_model");
        assert!(reg.try_acquire(k.clone()));
        reg.release(k.clone());
        assert!(
            reg.try_acquire(k),
            "re-acquire after release must return true (slot freed)"
        );
    }

    #[test]
    fn release_on_unheld_warns_no_panic() {
        let reg = InProcessLockRegistry::new();
        // Release on an unknown slot — no-op (warns).
        reg.release(key("app_demo:never_acquired", "never_acquired"));

        // Release on a known but currently-unheld slot — no-op (warns).
        let k = key("app_demo:register_model", "register_model");
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
}
