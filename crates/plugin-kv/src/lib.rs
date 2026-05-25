//! Key-value plugin — `env.kv.*` native primitives.
//!
//! `env.kv` is a `#[v8_class]` instance (`Kv`) minted once per isolate by
//! [`KvPlugin::build_instance`]. The instance carries the backend handle
//! and the app_id, so callbacks never read a thread-local for the
//! backend and never re-derive the app_id per call (mirrors `env.db`).
//!
//! Pluggable `Backend` dispatches to either:
//! - `RedbBackend` — single-process embedded persistent store (the
//!   self-host tier and the test backend)
//! - `Redis` — strongly consistent, atomic INCR / set-if-absent, TTL,
//!   network-backed
//!
//! Contract: **strong consistency + atomic ops** forever (see
//! `backend/mod.rs` for the canonical incr contract). If a future
//! backend can't keep that promise, it ships under a different SDK name.
//!
//! Native API surface (wrapped by the `@zeroship/kv` SDK):
//! - `env.kv.get(key)` → Promise<string | null>
//! - `env.kv.set(key, value, {ttlMs?})` → Promise<{ ok: true }>
//! - `env.kv.delete(key)` → Promise<{ deleted: boolean }>
//! - `env.kv.incr(key, {by?, ttlMs?})` → Promise<number | bigint>
//! - `env.kv.setIfAbsent(key, value, {ttlMs?})` → Promise<{ stored: boolean }>
//! - `env.kv.expire(key, ttlMs)` → Promise<{ updated: boolean }>
//! - `env.kv.ttl(key)` → Promise<{ ttlMs: number | null } | null>
//! - `env.kv.persist(key)` → Promise<{ updated: boolean }>
//! - `env.kv.list(prefix?, {cursor?, limit?})` → Promise<{ keys, cursor }>

use std::sync::Arc;

use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};

pub mod backend;
pub mod dispatch;
pub mod error;
pub mod limits;
pub mod v8_class;

pub use backend::{Backend, TtlState};
#[cfg(feature = "redb")]
pub use backend::RedbBackend;
#[cfg(feature = "redis")]
pub use backend::Redis;
pub use error::KvError;
pub use v8_class::mint_kv;

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
    /// Construct with a backend. There is no infallible default backend:
    /// `RedbBackend::open` is fallible (it takes an exclusive file lock),
    /// so callers open the backend and pass it here. Use `RedbBackend`
    /// for the embedded/self-host tier or `Redis` for distributed fleets.
    #[must_use]
    pub fn with_backend(backend: Arc<dyn Backend>) -> Self {
        Self { backend }
    }
}

impl NativePlugin for KvPlugin {
    fn namespace(&self) -> &str { "kv" }
    fn name(&self) -> &str { "kv" }

    /// No flat callbacks — the whole surface lives on the `Kv`
    /// v8_class minted by [`Self::build_instance`].
    fn register(&self, _r: &mut NativeRegistrar) {}

    fn build_instance<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        app_id: &str,
    ) -> Option<v8::Local<'s, v8::Object>> {
        mint_kv(scope, Arc::clone(&self.backend), app_id)
    }
}
