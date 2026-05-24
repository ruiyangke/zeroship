//! Backend abstraction for `env.kv.*`.
//!
//! Three impls ship today:
//! - `InMemory` — per-worker HashMap, dev only. No cross-worker state.
//! - `RedbBackend` — single-process embedded persistent store (`redb`),
//!   the self-host / single-worker-process tier. Exclusive file lock.
//! - `Redis` — network-backed, strongly consistent. Production fleets.
//!
//! Commitment (permanent): **every backend is strongly consistent with
//! atomic INCR / set-if-absent semantics.** If a future backend can't
//! uphold that contract, it doesn't ship under this SDK name — it
//! becomes a separate product (`@zeroship/config` for
//! eventual-consistency edge cache, etc.). Creators relying on
//! read-your-writes, atomic counters, and atomic locks never get
//! surprised.
//!
//! ## Canonical `incr` contract (all backends conform)
//!
//! - **Overflow is an error.** `incr` that would push the counter past
//!   the `i64` range returns [`KvError::Overflow`] — it does NOT
//!   saturate. A saturating counter silently lies about the count.
//! - **Non-numeric is an error.** `incr` on a key whose existing value
//!   isn't a base-10 integer returns [`KvError::NonNumeric`].
//! - **TTL is preserved on increment.** `incr` only applies its
//!   `ttl_ms` when it *creates* the key this call (fixed-window
//!   rate-limit semantics). Incrementing an existing key leaves the
//!   key's existing expiry untouched.

pub mod memory;
#[cfg(feature = "redb")]
pub mod redb;
#[cfg(feature = "redis")]
pub mod redis;

pub use memory::InMemory;
#[cfg(feature = "redb")]
pub use redb::RedbBackend;
#[cfg(feature = "redis")]
pub use redis::Redis;

use crate::error::KvError;

/// The TTL state of a key, returned by [`Backend::ttl`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtlState {
    /// The key does not exist.
    Missing,
    /// The key exists but has no expiry.
    NoExpiry,
    /// The key exists and expires in this many milliseconds.
    ExpiresInMs(u64),
}

#[async_trait::async_trait(?Send)]
pub trait Backend: Send + Sync + std::fmt::Debug {
    /// Returns `None` if the key is absent or expired.
    async fn get(&self, app_id: &str, key: &str) -> Result<Option<String>, KvError>;

    /// Set with optional TTL in milliseconds. Existing value (and its
    /// expiry) overwritten.
    async fn set(
        &self,
        app_id: &str,
        key: &str,
        value: &str,
        ttl_ms: Option<u64>,
    ) -> Result<(), KvError>;

    /// Delete — returns true when the key existed.
    async fn delete(&self, app_id: &str, key: &str) -> Result<bool, KvError>;

    /// Atomic increment. Creates the key (value=0) if missing before
    /// applying `delta`; negative deltas decrement.
    ///
    /// `ttl_ms` sets the key's expiry **only when this call creates the
    /// key** — an existing key's TTL is preserved (fixed-window
    /// rate-limit semantics). Overflow → [`KvError::Overflow`];
    /// non-numeric existing value → [`KvError::NonNumeric`].
    async fn incr(
        &self,
        app_id: &str,
        key: &str,
        delta: i64,
        ttl_ms: Option<u64>,
    ) -> Result<i64, KvError>;

    /// Atomic compare-on-absence set. Stores `value` (with optional
    /// `ttl_ms`) only if the key is currently absent. Returns true if
    /// the value was stored, false if the key already existed.
    ///
    /// For ephemeral locks / idempotency keys that auto-release via
    /// TTL — NOT durable "process-once" (use a DB unique index for
    /// that).
    async fn set_if_absent(
        &self,
        app_id: &str,
        key: &str,
        value: &str,
        ttl_ms: Option<u64>,
    ) -> Result<bool, KvError>;

    /// Set/refresh the TTL on an existing key. Returns false when the
    /// key is missing (no TTL could be set).
    async fn expire(&self, app_id: &str, key: &str, ttl_ms: u64) -> Result<bool, KvError>;

    /// Report the TTL state of a key: [`TtlState::Missing`],
    /// [`TtlState::NoExpiry`], or [`TtlState::ExpiresInMs`].
    async fn ttl(&self, app_id: &str, key: &str) -> Result<TtlState, KvError>;

    /// Remove the TTL from a key so it never expires. Returns false
    /// when the key is missing or already had no TTL.
    async fn persist(&self, app_id: &str, key: &str) -> Result<bool, KvError>;

    /// Paginated key listing. Returns the keys under `<app_id>:<prefix>`
    /// (the `<app_id>:` scope stripped) plus an opaque `next_cursor`.
    /// `cursor` of `None` starts iteration; a `next_cursor` of `None`
    /// means the listing is complete. `limit` bounds the page size.
    ///
    /// Backends may return eventually-evicted keys (lazy TTL), but must
    /// not return keys belonging to other apps. The cursor is
    /// backend-specific and opaque to the caller.
    async fn list(
        &self,
        app_id: &str,
        prefix: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<String>, Option<String>), KvError>;
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

/// Classify an `incr`-path server error message into the canonical
/// [`KvError`] variant. Shared by the Redis backend's `INCRBY` / `EVAL`
/// paths so the substring matching lives in one place.
///
/// Redis/Dragonfly return:
/// - `ERR value is not an integer or out of range` for both a
///   non-numeric existing value AND an `i64`-overflowing INCRBY. We
///   can't distinguish the two from the message alone, so we treat the
///   "not an integer" wording as [`KvError::NonNumeric`] and the
///   "out of range" wording as [`KvError::Overflow`] — and when both
///   substrings appear (the canonical message has both), we prefer
///   `Overflow` only if "out of range" is present without "not an
///   integer"… which never happens. The practical rule: presence of
///   "out of range" with INCRBY on a numeric counter is overflow; the
///   non-numeric case is the common one, so it wins the tie.
#[cfg(feature = "redis")]
pub(crate) fn classify_incr_error(msg: &str) -> KvError {
    let lower = msg.to_ascii_lowercase();
    // The canonical Redis message is
    // "ERR value is not an integer or out of range" — it carries BOTH
    // substrings. A genuine i64 overflow on INCRBY ("ERR increment or
    // decrement would overflow") carries "overflow". Disambiguate:
    //   - "overflow"          → Overflow (INCRBY past i64 range)
    //   - "not an integer"    → NonNumeric (existing value isn't numeric)
    //   - bare "out of range" → Overflow
    if lower.contains("overflow") {
        KvError::overflow(msg.to_string())
    } else if lower.contains("not an integer") {
        KvError::non_numeric(msg.to_string())
    } else if lower.contains("out of range") {
        KvError::overflow(msg.to_string())
    } else {
        KvError::backend(msg.to_string())
    }
}
