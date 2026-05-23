//! In-process advisory-lock registry — stub.
//!
//! **P1 PR 1**: skeleton only — the struct exists so the field shape
//! on [`super::SqliteBackend`] can be pinned at compile time. The
//! HashMap and the `try_acquire` / `release` primitive bodies land in
//! PR 4 alongside the `LockManager` impl.
//!
//! **Design** (`docs/proposals/db-system-design.md` §8.5): SQLite is
//! in-process by definition; both `LockScope::GlobalApp` and
//! `LockScope::LocalApp` route through this registry. The eventual
//! storage is `RefCell<HashMap<(String, String), Rc<Cell<bool>>>>` —
//! the `bool` records "currently held", the outer `Rc<Cell<…>>` lets
//! the eventual `SqliteLockGuard` sibling release on drop without
//! re-walking the map.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Per-process advisory-lock registry. One instance per
/// [`super::SqliteBackend`].
///
/// **P1 PR 1 stub**: empty field set with the eventual shape comment.
/// PR 4 fills in the `try_acquire` / `release` primitive bodies +
/// hooks the [`crate::backend::LockManager`] impl on
/// [`super::SqliteBackend`] through this struct.
#[derive(Default)]
pub(crate) struct InProcessLockRegistry {
    /// Eventual storage shape (commented out until PR 4 wires the
    /// `try_acquire` / `release` primitives that consume it):
    ///
    /// `slots: RefCell<HashMap<(String, String), Rc<std::cell::Cell<bool>>>>`,
    ///
    /// Keyed on the `(key1, key2)` pair the
    /// [`crate::backend::LockScope::to_keys`] derivation produces.
    #[allow(dead_code)]
    slots: RefCell<HashMap<(String, String), Rc<std::cell::Cell<bool>>>>,
}

impl InProcessLockRegistry {
    /// Construct an empty registry.
    #[allow(dead_code)]
    pub(crate) fn new() -> Self {
        Self::default()
    }
}
