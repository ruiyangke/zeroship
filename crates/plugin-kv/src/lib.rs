//! Key-value plugin — `zeroship.kv.*` native primitives.
//!
//! Pluggable `Backend` dispatches to either:
//! - `InMemory` — dev only, per-worker HashMap
//! - `Redis` — strongly consistent, atomic INCR, network-backed
//!
//! Contract: **strong consistency + atomic ops** forever. If a future
//! backend can't keep that promise, it ships under a different SDK name
//! (see `@zeroship/config` plans). See `backend/mod.rs` for details.
//!
//! Native API surface (wrapped by `@zeroship/kv` SDK):
//! - `zeroship.kv.get(key)` → Promise<string | null>
//! - `zeroship.kv.set(key, value, ttlMs?)` → Promise<{ ok: true }>
//! - `zeroship.kv.delete(key)` → Promise<{ deleted: boolean }>
//! - `zeroship.kv.incr(key, delta?)` → Promise<number>
//! - `zeroship.kv.list(prefix?)` → Promise<string[]>

use std::cell::RefCell;
use std::sync::Arc;

use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};

pub mod backend;
pub mod callbacks;

pub use backend::{Backend, InMemory};
#[cfg(feature = "redis")]
pub use backend::Redis;

// ---------------------------------------------------------------------------
// Thread-local backend handle — every worker has one.
// ---------------------------------------------------------------------------

thread_local! {
    pub(crate) static KV_BACKEND: RefCell<Option<Arc<dyn Backend>>> =
        const { RefCell::new(None) };
}

// ---------------------------------------------------------------------------
// KvPlugin
// ---------------------------------------------------------------------------

pub struct KvPlugin {
    backend: Arc<dyn Backend>,
}

impl std::fmt::Debug for KvPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KvPlugin").field("backend", &self.backend).finish()
    }
}

impl KvPlugin {
    /// In-memory backend (dev only — no cross-worker state).
    #[must_use]
    pub fn in_memory() -> Self {
        Self { backend: Arc::new(InMemory::new()) }
    }

    /// Back-compat shortcut matching the pre-refactor API.
    #[must_use]
    pub fn new() -> Self { Self::in_memory() }

    /// Custom backend — use for `Redis` in production or custom impls.
    #[must_use]
    pub fn with_backend(backend: Arc<dyn Backend>) -> Self {
        Self { backend }
    }
}

impl Default for KvPlugin {
    fn default() -> Self { Self::new() }
}

impl NativePlugin for KvPlugin {
    fn namespace(&self) -> &str { "kv" }
    fn name(&self) -> &str { "kv" }

    fn register(&self, r: &mut NativeRegistrar) {
        KV_BACKEND.with(|cell| {
            *cell.borrow_mut() = Some(Arc::clone(&self.backend));
        });
        r.add("get", callbacks::get);
        r.add("set", callbacks::set);
        r.add("delete", callbacks::delete);
        r.add("incr", callbacks::incr);
        r.add("list", callbacks::list);
    }
}
