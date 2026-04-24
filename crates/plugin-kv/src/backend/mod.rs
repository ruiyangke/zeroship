//! Backend abstraction for `zeroship.kv.*`.
//!
//! Two impls ship today:
//! - `InMemory` — per-worker HashMap, dev only. No cross-worker state.
//! - `Redis` — network-backed, strongly consistent. Production.
//!
//! Commitment (permanent): **every backend is strongly consistent with
//! atomic INCR semantics.** If a future backend can't uphold that
//! contract, it doesn't ship under this SDK name — it becomes a
//! separate product (`@zeroship/config` for eventual-consistency edge
//! cache, etc.). Creators relying on read-your-writes and atomic
//! counters never get surprised.

pub mod memory;
#[cfg(feature = "redis")]
pub mod redis;

pub use memory::InMemory;
#[cfg(feature = "redis")]
pub use redis::Redis;

#[async_trait::async_trait(?Send)]
pub trait Backend: Send + Sync + std::fmt::Debug {
    /// Returns `None` if the key is absent or expired.
    async fn get(&self, app_id: &str, key: &str) -> Result<Option<String>, String>;

    /// Set with optional TTL in milliseconds. Existing value overwritten.
    async fn set(
        &self,
        app_id: &str,
        key: &str,
        value: &str,
        ttl_ms: Option<u64>,
    ) -> Result<(), String>;

    /// Delete — returns true when the key existed.
    async fn delete(&self, app_id: &str, key: &str) -> Result<bool, String>;

    /// Atomic increment. Creates the key (value=0) if missing before applying delta.
    /// Negative deltas decrement. Over/underflow saturates at i64 range.
    async fn incr(&self, app_id: &str, key: &str, delta: i64) -> Result<i64, String>;

    /// List keys matching `<app_id>:<prefix>*`, stripping the app_id prefix.
    /// Backends may return eventually-evicted keys (lazy TTL), but must not
    /// return keys belonging to other apps.
    async fn list(&self, app_id: &str, prefix: &str) -> Result<Vec<String>, String>;
}

/// Common `<app_id>:<key>` scoping helper — every backend uses this to
/// enforce per-app isolation.
pub fn scope(app_id: &str, key: &str) -> String {
    format!("{app_id}:{key}")
}
