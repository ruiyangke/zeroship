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

/// Common per-app scoping helper. Every backend uses this to enforce
/// isolation between tenants on a shared Redis/Dragonfly.
///
/// Wire format: `{<app_id>}:<key>`.
///
/// The `{...}` is a Redis cluster **hash tag**: every key sharing the same
/// tag hashes to the same slot, and therefore the same shard. That means:
///
/// - One app's keyspace always lives on exactly one shard (fast SCAN,
///   no cross-shard fan-out on prefix list).
/// - Multi-key ops within an app (MULTI/EXEC, LUA) work even on a sharded
///   cluster — they all stay local to one node.
/// - When we later shard out, no data migration for existing apps.
///
/// Trade-off: a single *huge* app can't scale past one shard's capacity.
/// For our "millions of small apps" model that's the correct default;
/// whale-app handling is a v2 feature.
///
/// The InMemory backend doesn't care about the braces — they're just
/// extra bytes in the map key. The Redis/Dragonfly backend is where the
/// hash tag does real work.
pub fn scope(app_id: &str, key: &str) -> String {
    format!("{{{app_id}}}:{key}")
}
