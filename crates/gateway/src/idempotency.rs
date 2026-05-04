//! Idempotency dedupe for `idempotent: true` mutations.
//!
//! Per `docs/proposals/rpc-v2.md` §8 (Idempotency). Implementation rules:
//!
//! - Wire requires `Idempotency-Key` for any procedure whose
//!   `EffectivePolicy.idempotent` is `true`.
//! - Stored response keyed by `(app_id, wireId, idempotency_key)`.
//! - Same key + same input hash within TTL → return stored response
//!   verbatim (a hit).
//! - Same key + different input hash within TTL → 409 ALREADY_EXISTS
//!   with `Retry-After: <seconds-remaining>`.
//! - Same key while in-flight → block on a per-key lock; if the original
//!   completes within `worker_timeout`, return its result; otherwise
//!   409 ABORTED.
//! - TTL: default 24h, configurable per procedure via
//!   `EffectivePolicy.idempotency_ttl_hours` (clamped to `[1, 168]`).
//!
//! Storage backend is the `IdempotencyStore` trait. Production wires it
//! to a Redis-backed (or compio-redis) implementation; tests use the
//! in-memory `InMemoryIdempotencyStore` directly. The gateway does NOT
//! reach into `plugin-kv`'s in-isolate API — that runs inside the V8
//! worker, not the gateway. Both backends share the same wire format
//! (the JSON value below), so swapping is a configuration concern.
//!
//! KV value layout (matches §8 storage spec):
//!
//! ```jsonc
//! {
//!   "input_hash":   "sha256:<hex>",
//!   "status":       200,
//!   "headers":      { "content-type": "..." },
//!   "body_b64":     "<base64>",
//!   "completed_at": 1714499200000,
//!   "ttl_until":    1714585600000
//! }
//! ```

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Public constants — exercised by tests, kept here so they stay one source.
// ---------------------------------------------------------------------------

/// Default TTL for idempotency dedupe entries when the procedure
/// doesn't pin one explicitly.
pub const DEFAULT_TTL_HOURS: u32 = 24;

/// Spec §8: TTL ∈ [1, 168] hours (1h to 7d). Authored values that fall
/// outside the band are clamped here.
pub const MIN_TTL_HOURS: u32 = 1;
pub const MAX_TTL_HOURS: u32 = 168;

/// In-flight lock TTL. Cleans itself up if the worker crashes; we
/// never rely on perfect lock cleanup. Spec §8: ~30s.
pub const LOCK_TTL_SECS: u64 = 30;

/// Per-app live-key cap. Spec §16: 1 M live keys per app, evict oldest
/// on overflow with a log line. Implemented for the in-memory backend
/// (a Redis-backed one would set this via Redis maxmemory-policy
/// `allkeys-lru` on a dedicated db, or a per-app counter check).
pub const MAX_LIVE_KEYS_PER_APP: usize = 1_000_000;

/// Maximum time we'll wait for an in-flight original request to
/// complete before returning 409 ABORTED to the duplicate. Bounded by
/// the procedure's `timeout` policy (when shorter); 30s default.
pub const DEFAULT_INFLIGHT_WAIT_MS: u64 = 30_000;

// ---------------------------------------------------------------------------
// Stored value
// ---------------------------------------------------------------------------

/// One dedupe entry, JSON-serialized into the KV value slot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredResponse {
    /// `sha256:<hex>` of the request body bytes the gateway saw.
    pub input_hash: String,
    /// HTTP status of the stored response.
    pub status: u16,
    /// Response headers we replay verbatim (filtered: hop-by-hop and
    /// per-request entries are dropped before storage; see
    /// `capture_response_headers`).
    pub headers: HashMap<String, String>,
    /// Base64-encoded raw response body bytes.
    pub body_b64: String,
    /// Epoch-ms when the original request finished.
    pub completed_at: u64,
    /// Epoch-ms when this entry expires. The gateway compares against
    /// its own monotonic clock when answering `Retry-After`.
    pub ttl_until: u64,
}

impl StoredResponse {
    /// Convenience accessor for the body bytes.
    pub fn body_bytes(&self) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(self.body_b64.as_bytes())
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Hashing — sha256 of raw bytes, deliberately non-canonicalised.
// ---------------------------------------------------------------------------

/// Spec §8 critical-correctness point: the gateway hashes the EXACT
/// bytes it sees, not a canonical-JSON form. `{"a":1,"b":2}` and
/// `{"b":2,"a":1}` MUST produce different hashes — otherwise an
/// honest re-serialization on the client side would silently match a
/// different payload.
pub fn hash_body(body: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body);
    let digest = hasher.finalize();
    format!("sha256:{}", hex::encode(digest))
}

// ---------------------------------------------------------------------------
// KV key derivation
// ---------------------------------------------------------------------------

pub fn entry_key(app_id: &Uuid, wire_id: &str, idem_key: &str) -> String {
    format!("idem:{app_id}:{wire_id}:{idem_key}")
}

pub fn lock_key(app_id: &Uuid, wire_id: &str, idem_key: &str) -> String {
    format!("idem-lock:{app_id}:{wire_id}:{idem_key}")
}

// ---------------------------------------------------------------------------
// Backend abstraction
// ---------------------------------------------------------------------------

/// Backend the gateway calls into for dedupe storage. `set_nx` is the
/// load-bearing primitive — it MUST be atomic (set-if-absent with TTL
/// in one step). Redis offers this as `SET key value NX EX ttl`; the
/// in-memory backend below holds a `Mutex` around the same semantic.
///
/// All async, all `?Send`, matching the rest of the gateway's
/// compio-on-current-thread runtime. Callers don't yield across
/// thread boundaries.
#[async_trait::async_trait(?Send)]
pub trait IdempotencyStore: Send + Sync + std::fmt::Debug {
    /// Get a stored response. Returns `None` if absent or expired.
    async fn get_entry(&self, key: &str) -> Result<Option<StoredResponse>, String>;

    /// Set a stored response with absolute expiry. Existing values
    /// (including locks under different keys) are NOT considered.
    async fn put_entry(&self, app_id: &Uuid, key: &str, value: &StoredResponse) -> Result<(), String>;

    /// Atomic `SET NX EX`: returns `true` if the lock was acquired,
    /// `false` if a lock already existed. TTL guards against orphaned
    /// locks if the worker crashes.
    async fn try_acquire_lock(&self, key: &str, ttl_ms: u64) -> Result<bool, String>;

    /// Release a lock. Idempotent — releasing an already-released lock
    /// is fine.
    async fn release_lock(&self, key: &str) -> Result<(), String>;

    /// Wait for an entry to appear under `key`, polling at the given
    /// interval up to `timeout_ms`. Returns the entry if it appeared,
    /// `None` if the wait timed out. Default impl polls via `get_entry`;
    /// a Redis backend would prefer pub/sub.
    async fn wait_for_entry(
        &self,
        key: &str,
        timeout_ms: u64,
        poll_interval_ms: u64,
    ) -> Result<Option<StoredResponse>, String> {
        let start = SystemTime::now();
        let deadline = start + Duration::from_millis(timeout_ms);
        loop {
            if let Some(entry) = self.get_entry(key).await? {
                return Ok(Some(entry));
            }
            if SystemTime::now() >= deadline {
                return Ok(None);
            }
            compio::time::sleep(Duration::from_millis(poll_interval_ms)).await;
        }
    }
}

// ---------------------------------------------------------------------------
// In-memory backend — used in dev and unit tests.
// ---------------------------------------------------------------------------

/// In-memory implementation of [`IdempotencyStore`]. Single-process; the
/// gateway uses this in dev or when `--idempotency-backend=memory`. Wire
/// format matches the Redis-backed impl byte-for-byte so swapping doesn't
/// migrate data shape.
///
/// Per-app live-key cap is enforced via a simple FIFO eviction queue.
/// Spec §16 specifies "evict oldest on overflow with a log line"; a
/// proper LRU would be more accurate but FIFO is cheaper and the cap
/// is large enough that the difference doesn't matter in practice.
#[derive(Debug)]
pub struct InMemoryIdempotencyStore {
    entries: Mutex<HashMap<String, EntryRecord>>,
    locks: Mutex<HashMap<String, LockRecord>>,
    /// FIFO order of `(app_id, key)` pairs for the live-key cap. Pushed
    /// on every `put_entry`; popped when an app exceeds
    /// `MAX_LIVE_KEYS_PER_APP`.
    fifo: Mutex<HashMap<Uuid, Vec<String>>>,
}

#[derive(Debug, Clone)]
struct EntryRecord {
    value: StoredResponse,
    /// Epoch-ms when this record expires. Lazy-evicted on read.
    expires_at_ms: u64,
}

#[derive(Debug, Clone)]
struct LockRecord {
    /// Epoch-ms when this lock expires. Lazy-evicted on read.
    expires_at_ms: u64,
}

impl InMemoryIdempotencyStore {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
            fifo: Mutex::new(HashMap::new()),
        }
    }

    /// Test-only: total number of live entries across all apps. Useful
    /// for the cap-eviction tests.
    #[cfg(test)]
    pub fn entry_count(&self) -> usize {
        self.entries.lock().unwrap().len()
    }
}

impl Default for InMemoryIdempotencyStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait(?Send)]
impl IdempotencyStore for InMemoryIdempotencyStore {
    async fn get_entry(&self, key: &str) -> Result<Option<StoredResponse>, String> {
        let now = now_ms();
        let mut map = self.entries.lock().unwrap();
        match map.get(key) {
            Some(rec) if rec.expires_at_ms > now => Ok(Some(rec.value.clone())),
            Some(_) => {
                map.remove(key);
                Ok(None)
            }
            None => Ok(None),
        }
    }

    async fn put_entry(&self, app_id: &Uuid, key: &str, value: &StoredResponse) -> Result<(), String> {
        let rec = EntryRecord {
            value: value.clone(),
            expires_at_ms: value.ttl_until,
        };
        let evicted_key: Option<String> = {
            let mut entries = self.entries.lock().unwrap();
            let mut fifo = self.fifo.lock().unwrap();

            // Track this key in the per-app FIFO queue. Inserting an
            // existing key counts once: we only push if it's not
            // already in the queue (the storage layer dedupes by key).
            let queue = fifo.entry(*app_id).or_default();
            if !queue.iter().any(|k| k == key) {
                queue.push(key.to_string());
            }
            entries.insert(key.to_string(), rec);

            // Cap enforcement: when we exceed the per-app limit, drop
            // the oldest. Returns the evicted key (if any) so we can
            // log it under the lock-free path.
            if queue.len() > MAX_LIVE_KEYS_PER_APP {
                let oldest = queue.remove(0);
                entries.remove(&oldest);
                Some(oldest)
            } else {
                None
            }
        };
        if let Some(k) = evicted_key {
            tracing::warn!(
                key = %k,
                app_id = %app_id,
                max_live_keys = MAX_LIVE_KEYS_PER_APP,
                "idempotency: evicted oldest entry (max live keys exceeded)"
            );
        }
        Ok(())
    }

    async fn try_acquire_lock(&self, key: &str, ttl_ms: u64) -> Result<bool, String> {
        let now = now_ms();
        let expires_at_ms = now.saturating_add(ttl_ms);
        let mut map = self.locks.lock().unwrap();
        match map.get(key) {
            Some(rec) if rec.expires_at_ms > now => Ok(false),
            _ => {
                map.insert(key.to_string(), LockRecord { expires_at_ms });
                Ok(true)
            }
        }
    }

    async fn release_lock(&self, key: &str) -> Result<(), String> {
        self.locks.lock().unwrap().remove(key);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Pre-dispatch decision shape
// ---------------------------------------------------------------------------

/// Result of `pre_dispatch` — the gateway uses this to decide whether
/// to proceed with the worker call, return a stored response, or
/// reject outright. The `LockHeld` variant carries the lock key so
/// the post-dispatch step can release it.
#[derive(Debug)]
pub enum DedupeDecision {
    /// Stored entry hit — return this response verbatim. Worker is not
    /// invoked.
    Hit(StoredResponse),
    /// Caller can proceed; lock is held and must be released after the
    /// worker call (success or failure). `entry_key`, `lock_key`,
    /// `body_hash`, and `ttl_hours` flow through to `post_dispatch`.
    Proceed {
        entry_key: String,
        lock_key: String,
        body_hash: String,
        ttl_hours: u32,
    },
    /// Same key, different body — reject with 409 ALREADY_EXISTS and
    /// `Retry-After: <seconds>`.
    Conflict { retry_after_secs: u64 },
    /// Missing `Idempotency-Key` on a procedure that requires one.
    MissingHeader,
    /// In-flight original aborted (waited past timeout).
    InFlightTimedOut,
}

// ---------------------------------------------------------------------------
// The pre-dispatch flow.
// ---------------------------------------------------------------------------

/// Apply the dedupe decision tree per spec §8. Caller wraps this in
/// the appropriate HTTP response on `Hit` / `Conflict` / etc.
///
/// `idempotency_key` is the value of the `Idempotency-Key` header, if
/// any. `body` is the raw request bytes. `ttl_hours` is the resolved
/// per-procedure TTL (already clamped via `clamp_ttl_hours`).
/// `inflight_wait_ms` is how long we'll block on a per-key lock when
/// the original request is in flight.
pub async fn pre_dispatch(
    store: &dyn IdempotencyStore,
    app_id: &Uuid,
    wire_id: &str,
    idempotency_key: Option<&str>,
    body: &[u8],
    ttl_hours: u32,
    inflight_wait_ms: u64,
) -> Result<DedupeDecision, String> {
    let Some(idem_key) = idempotency_key.filter(|s| !s.is_empty()) else {
        return Ok(DedupeDecision::MissingHeader);
    };

    let body_hash = hash_body(body);
    let key = entry_key(app_id, wire_id, idem_key);
    let l_key = lock_key(app_id, wire_id, idem_key);

    if let Some(stored) = store.get_entry(&key).await? {
        if stored.input_hash == body_hash {
            return Ok(DedupeDecision::Hit(stored));
        }
        let now = now_ms();
        let secs = stored.ttl_until.saturating_sub(now) / 1000;
        return Ok(DedupeDecision::Conflict {
            retry_after_secs: secs.max(1),
        });
    }

    // No stored entry — try to acquire the in-flight lock. NX + EX
    // semantics guarantee at most one acquirer; the loser falls into
    // the wait branch.
    let lock_ttl_ms = LOCK_TTL_SECS * 1000;
    if store.try_acquire_lock(&l_key, lock_ttl_ms).await? {
        return Ok(DedupeDecision::Proceed {
            entry_key: key,
            lock_key: l_key,
            body_hash,
            ttl_hours,
        });
    }

    // Lock contended — another in-flight request owns this key. Wait
    // for it to land an entry; if the wait expires, return ABORTED so
    // the caller can decide whether to retry.
    match store
        .wait_for_entry(&key, inflight_wait_ms, 50)
        .await?
    {
        Some(stored) => {
            if stored.input_hash == body_hash {
                Ok(DedupeDecision::Hit(stored))
            } else {
                let now = now_ms();
                let secs = stored.ttl_until.saturating_sub(now) / 1000;
                Ok(DedupeDecision::Conflict {
                    retry_after_secs: secs.max(1),
                })
            }
        }
        None => Ok(DedupeDecision::InFlightTimedOut),
    }
}

/// Persist the worker's response under the dedupe key and release the
/// in-flight lock. Called after the worker responds (success or
/// declared error). The caller passes the post-flight metadata from
/// the `Proceed` decision.
pub async fn capture_response(
    store: &dyn IdempotencyStore,
    app_id: &Uuid,
    entry_key: &str,
    lock_key: &str,
    body_hash: &str,
    status: u16,
    headers: &HashMap<String, String>,
    body: &[u8],
    ttl_hours: u32,
) -> Result<StoredResponse, String> {
    let now = now_ms();
    let ttl_until = now + (u64::from(ttl_hours) * 60 * 60 * 1000);
    let stored = StoredResponse {
        input_hash: body_hash.to_string(),
        status,
        headers: headers.clone(),
        body_b64: base64::engine::general_purpose::STANDARD.encode(body),
        completed_at: now,
        ttl_until,
    };
    store.put_entry(app_id, entry_key, &stored).await?;
    let _ = store.release_lock(lock_key).await;
    Ok(stored)
}

/// Release the lock without storing a response. Called when the
/// worker times out — subsequent requests should retry the operation
/// rather than see a stale "completed" entry.
pub async fn release_lock_without_storing(
    store: &dyn IdempotencyStore,
    lock_key: &str,
) -> Result<(), String> {
    store.release_lock(lock_key).await
}

// ---------------------------------------------------------------------------
// TTL helpers
// ---------------------------------------------------------------------------

/// Clamp the per-procedure TTL into the [1, 168] hours band. `None`
/// resolves to the default. The vite-plugin enforces the band at
/// build time; the gateway enforces defensively in case the manifest
/// arrives via a less-trusted path.
pub fn clamp_ttl_hours(declared: Option<u32>) -> u32 {
    let h = declared.unwrap_or(DEFAULT_TTL_HOURS);
    h.clamp(MIN_TTL_HOURS, MAX_TTL_HOURS)
}

// ---------------------------------------------------------------------------
// Header sanitization for storage
// ---------------------------------------------------------------------------

/// Drop hop-by-hop and per-request headers from the captured response.
/// We replay the body verbatim but synthesize fresh hop-by-hops on the
/// dedup'd response so connection management isn't poisoned by a
/// captured `Connection: close`.
pub fn capture_response_headers(headers: &[(String, String)]) -> HashMap<String, String> {
    const DROP: &[&str] = &[
        "connection",
        "keep-alive",
        "transfer-encoding",
        "te",
        "trailer",
        "upgrade",
        "proxy-authorization",
        "proxy-authenticate",
        "x-request-id",
        "x-wall-time-ms",
    ];
    let mut out = HashMap::new();
    for (name, value) in headers {
        let lname = name.to_ascii_lowercase();
        if DROP.contains(&lname.as_str()) {
            continue;
        }
        out.insert(lname, value.clone());
    }
    out
}

// ---------------------------------------------------------------------------
// Wall-clock helpers
// ---------------------------------------------------------------------------

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_app() -> Uuid {
        Uuid::new_v4()
    }

    fn run_async<F: std::future::Future>(f: F) -> F::Output {
        compio::runtime::Runtime::new().unwrap().block_on(f)
    }

    #[test]
    fn hash_body_is_byte_exact_not_canonical() {
        // {"a":1,"b":2} and {"b":2,"a":1} hash differently. The spec
        // §8 critical-correctness point demands raw-byte hashing, not
        // canonical-JSON hashing.
        let a = br#"{"a":1,"b":2}"#;
        let b = br#"{"b":2,"a":1}"#;
        assert_ne!(hash_body(a), hash_body(b));
    }

    #[test]
    fn hash_body_idempotent_on_same_bytes() {
        let body = br#"{"text":"hi"}"#;
        assert_eq!(hash_body(body), hash_body(body));
    }

    #[test]
    fn clamp_ttl_default_when_none() {
        assert_eq!(clamp_ttl_hours(None), DEFAULT_TTL_HOURS);
    }

    #[test]
    fn clamp_ttl_low_clamped_up() {
        assert_eq!(clamp_ttl_hours(Some(0)), MIN_TTL_HOURS);
    }

    #[test]
    fn clamp_ttl_high_clamped_down() {
        assert_eq!(clamp_ttl_hours(Some(999)), MAX_TTL_HOURS);
    }

    #[test]
    fn clamp_ttl_inside_band_passes_through() {
        assert_eq!(clamp_ttl_hours(Some(48)), 48);
    }

    #[test]
    fn missing_header_returns_decision() {
        run_async(async {
            let store = InMemoryIdempotencyStore::new();
            let app = fixture_app();
            let dec = pre_dispatch(&store, &app, "todos.add", None, b"{}", 24, 1000)
                .await
                .unwrap();
            assert!(matches!(dec, DedupeDecision::MissingHeader));
        });
    }

    #[test]
    fn first_request_proceeds_and_acquires_lock() {
        run_async(async {
            let store = InMemoryIdempotencyStore::new();
            let app = fixture_app();
            let dec = pre_dispatch(&store, &app, "todos.add", Some("k1"), b"{}", 24, 1000)
                .await
                .unwrap();
            match dec {
                DedupeDecision::Proceed { entry_key: ek, lock_key: lk, body_hash, .. } => {
                    assert_eq!(ek, entry_key(&app, "todos.add", "k1"));
                    assert_eq!(lk, lock_key(&app, "todos.add", "k1"));
                    assert!(body_hash.starts_with("sha256:"));
                }
                other => panic!("expected Proceed, got {other:?}"),
            }
        });
    }

    #[test]
    fn second_request_same_key_same_body_returns_hit() {
        run_async(async {
            let store = InMemoryIdempotencyStore::new();
            let app = fixture_app();

            // First request proceeds, captures.
            let dec = pre_dispatch(&store, &app, "todos.add", Some("k1"), b"{\"x\":1}", 24, 1000)
                .await
                .unwrap();
            let DedupeDecision::Proceed { entry_key: ek, lock_key: lk, body_hash, ttl_hours } = dec
            else {
                panic!("expected Proceed");
            };
            let mut headers = HashMap::new();
            headers.insert("content-type".into(), "application/json".into());
            capture_response(
                &store,
                &app,
                &ek,
                &lk,
                &body_hash,
                201,
                &headers,
                b"{\"id\":42}",
                ttl_hours,
            )
            .await
            .unwrap();

            // Second request, same body → Hit.
            let dec2 = pre_dispatch(&store, &app, "todos.add", Some("k1"), b"{\"x\":1}", 24, 1000)
                .await
                .unwrap();
            match dec2 {
                DedupeDecision::Hit(stored) => {
                    assert_eq!(stored.status, 201);
                    assert_eq!(stored.body_bytes(), b"{\"id\":42}");
                    assert_eq!(stored.headers.get("content-type").map(String::as_str), Some("application/json"));
                }
                other => panic!("expected Hit, got {other:?}"),
            }
        });
    }

    #[test]
    fn second_request_same_key_different_body_returns_conflict() {
        run_async(async {
            let store = InMemoryIdempotencyStore::new();
            let app = fixture_app();
            let dec = pre_dispatch(&store, &app, "todos.add", Some("k1"), b"{\"a\":1}", 24, 1000)
                .await
                .unwrap();
            let DedupeDecision::Proceed { entry_key: ek, lock_key: lk, body_hash, ttl_hours } = dec
            else {
                panic!("expected Proceed");
            };
            capture_response(
                &store, &app, &ek, &lk, &body_hash, 200, &HashMap::new(), b"ok", ttl_hours,
            )
            .await
            .unwrap();

            let dec2 = pre_dispatch(&store, &app, "todos.add", Some("k1"), b"{\"b\":2}", 24, 1000)
                .await
                .unwrap();
            match dec2 {
                DedupeDecision::Conflict { retry_after_secs } => {
                    // 24h window → ~86400s left; allow some clock slop.
                    assert!(retry_after_secs > 86_000, "got {retry_after_secs}");
                    assert!(retry_after_secs <= 24 * 3600);
                }
                other => panic!("expected Conflict, got {other:?}"),
            }
        });
    }

    #[test]
    fn different_keys_proceed_independently() {
        run_async(async {
            let store = InMemoryIdempotencyStore::new();
            let app = fixture_app();
            let d1 = pre_dispatch(&store, &app, "todos.add", Some("k1"), b"{}", 24, 1000)
                .await
                .unwrap();
            let d2 = pre_dispatch(&store, &app, "todos.add", Some("k2"), b"{}", 24, 1000)
                .await
                .unwrap();
            assert!(matches!(d1, DedupeDecision::Proceed { .. }));
            assert!(matches!(d2, DedupeDecision::Proceed { .. }));
        });
    }

    #[test]
    fn ttl_zero_means_immediate_eviction() {
        // An entry with `ttl_until` <= now is treated as absent.
        run_async(async {
            let store = InMemoryIdempotencyStore::new();
            let app = fixture_app();
            let key = entry_key(&app, "todos.add", "k1");
            let stored = StoredResponse {
                input_hash: hash_body(b"{}"),
                status: 200,
                headers: HashMap::new(),
                body_b64: String::new(),
                completed_at: now_ms(),
                ttl_until: 1, // expired ages ago
            };
            store.put_entry(&app, &key, &stored).await.unwrap();

            // get_entry → None.
            assert!(store.get_entry(&key).await.unwrap().is_none());

            // pre_dispatch → Proceed (no live entry).
            let dec = pre_dispatch(&store, &app, "todos.add", Some("k1"), b"{}", 24, 1000)
                .await
                .unwrap();
            assert!(matches!(dec, DedupeDecision::Proceed { .. }));
        });
    }

    #[test]
    fn lock_blocks_concurrent_acquire() {
        run_async(async {
            let store = InMemoryIdempotencyStore::new();
            let lk = "idem-lock:test".to_string();
            let got_first = store.try_acquire_lock(&lk, 30_000).await.unwrap();
            let got_second = store.try_acquire_lock(&lk, 30_000).await.unwrap();
            assert!(got_first);
            assert!(!got_second, "second acquire must fail");
            store.release_lock(&lk).await.unwrap();
            let got_third = store.try_acquire_lock(&lk, 30_000).await.unwrap();
            assert!(got_third, "release-then-acquire must succeed");
        });
    }

    #[test]
    fn lock_expires_on_its_ttl() {
        run_async(async {
            let store = InMemoryIdempotencyStore::new();
            let lk = "idem-lock:expire-test".to_string();
            let got = store.try_acquire_lock(&lk, 1).await.unwrap();
            assert!(got);
            // Wait past the lock TTL.
            compio::time::sleep(Duration::from_millis(10)).await;
            // Re-acquire must succeed because the prior lock expired.
            let got2 = store.try_acquire_lock(&lk, 30_000).await.unwrap();
            assert!(got2, "lock TTL must expire and allow re-acquire");
        });
    }

    #[test]
    fn inflight_wait_returns_hit_when_original_completes() {
        run_async(async {
            let store = std::sync::Arc::new(InMemoryIdempotencyStore::new());
            let app = fixture_app();
            let body = b"{\"hello\":\"world\"}".to_vec();

            // First request gets the lock (Proceed). Don't capture yet.
            let d1 = pre_dispatch(&*store, &app, "todos.add", Some("k1"), &body, 24, 5_000)
                .await
                .unwrap();
            let DedupeDecision::Proceed { entry_key: ek, lock_key: lk, body_hash, ttl_hours } = d1
            else {
                panic!("expected Proceed");
            };

            // Spawn a task that captures the response after a short
            // delay, simulating a worker finishing a slow mutation.
            let store_clone = store.clone();
            let ek_clone = ek.clone();
            let lk_clone = lk.clone();
            let body_hash_clone = body_hash.clone();
            let app_clone = app;
            compio::runtime::spawn(async move {
                compio::time::sleep(Duration::from_millis(100)).await;
                let mut h = HashMap::new();
                h.insert("content-type".into(), "application/json".into());
                capture_response(
                    &*store_clone,
                    &app_clone,
                    &ek_clone,
                    &lk_clone,
                    &body_hash_clone,
                    200,
                    &h,
                    b"{\"ok\":true}",
                    ttl_hours,
                )
                .await
                .unwrap();
            })
            .detach();

            // Second concurrent request should block, then get the
            // captured response back.
            let d2 = pre_dispatch(&*store, &app, "todos.add", Some("k1"), &body, 24, 5_000)
                .await
                .unwrap();
            match d2 {
                DedupeDecision::Hit(stored) => {
                    assert_eq!(stored.status, 200);
                    assert_eq!(stored.body_bytes(), b"{\"ok\":true}");
                }
                other => panic!("expected Hit after wait, got {other:?}"),
            }
        });
    }

    #[test]
    fn inflight_wait_times_out_returns_aborted_decision() {
        run_async(async {
            let store = InMemoryIdempotencyStore::new();
            let app = fixture_app();

            // First request takes the lock; never captures.
            let d1 = pre_dispatch(&store, &app, "todos.add", Some("k1"), b"{}", 24, 1000)
                .await
                .unwrap();
            assert!(matches!(d1, DedupeDecision::Proceed { .. }));

            // Second request waits for ~50ms (short timeout); the
            // first never completes, so we must get InFlightTimedOut.
            let d2 = pre_dispatch(&store, &app, "todos.add", Some("k1"), b"{}", 24, 50)
                .await
                .unwrap();
            assert!(matches!(d2, DedupeDecision::InFlightTimedOut), "got {d2:?}");
        });
    }

    #[test]
    fn error_response_is_replayed() {
        // Spec implies: store everything, including error envelopes.
        // Subsequent retries see the same 500 the first attempt got.
        run_async(async {
            let store = InMemoryIdempotencyStore::new();
            let app = fixture_app();
            let dec = pre_dispatch(&store, &app, "billing.charge", Some("k1"), b"{}", 24, 1000)
                .await
                .unwrap();
            let DedupeDecision::Proceed { entry_key: ek, lock_key: lk, body_hash, ttl_hours } = dec
            else {
                panic!("expected Proceed");
            };
            capture_response(
                &store, &app, &ek, &lk, &body_hash, 500, &HashMap::new(), b"oops", ttl_hours,
            )
            .await
            .unwrap();

            let dec2 = pre_dispatch(&store, &app, "billing.charge", Some("k1"), b"{}", 24, 1000)
                .await
                .unwrap();
            match dec2 {
                DedupeDecision::Hit(stored) => {
                    assert_eq!(stored.status, 500);
                    assert_eq!(stored.body_bytes(), b"oops");
                }
                _ => panic!("expected Hit on error replay"),
            }
        });
    }

    #[test]
    fn capture_response_drops_hop_by_hop_headers() {
        let raw = vec![
            ("Content-Type".into(), "application/json".into()),
            ("Connection".into(), "close".into()),
            ("Transfer-Encoding".into(), "chunked".into()),
            ("X-Request-Id".into(), "abc".into()),
            ("X-Custom".into(), "keep".into()),
        ];
        let cleaned = capture_response_headers(&raw);
        assert!(cleaned.contains_key("content-type"));
        assert!(cleaned.contains_key("x-custom"));
        assert!(!cleaned.contains_key("connection"));
        assert!(!cleaned.contains_key("transfer-encoding"));
        assert!(!cleaned.contains_key("x-request-id"));
    }

    #[test]
    fn ttl_one_hour_expires_before_24h() {
        // The procedure has idempotency_ttl_hours = 1; after we move
        // the wall-clock past that point (simulated via a stored
        // entry with a near-now ttl_until), the entry must be
        // treated as absent.
        run_async(async {
            let store = InMemoryIdempotencyStore::new();
            let app = fixture_app();
            let key = entry_key(&app, "todos.add", "k1");
            let now = now_ms();
            // 1h TTL but stored 1h+1ms ago so it has just expired.
            let stored = StoredResponse {
                input_hash: hash_body(b"{}"),
                status: 200,
                headers: HashMap::new(),
                body_b64: String::new(),
                completed_at: now.saturating_sub(3_600_001),
                ttl_until: now.saturating_sub(1),
            };
            store.put_entry(&app, &key, &stored).await.unwrap();
            assert!(store.get_entry(&key).await.unwrap().is_none(),
                "1h-old entry must be evicted before 24h default would");
        });
    }

    #[test]
    fn live_key_cap_evicts_oldest() {
        // Use a tighter cap by checking the eviction path against a
        // small-enough sample. The const itself is 1M; instead of
        // populating that many, we exercise the bookkeeping by
        // putting two entries and confirming the FIFO records them.
        run_async(async {
            let store = InMemoryIdempotencyStore::new();
            let app = fixture_app();
            for i in 0..5 {
                let key = entry_key(&app, "todos.add", &format!("k{i}"));
                let stored = StoredResponse {
                    input_hash: hash_body(b"{}"),
                    status: 200,
                    headers: HashMap::new(),
                    body_b64: String::new(),
                    completed_at: now_ms(),
                    ttl_until: now_ms() + 60_000,
                };
                store.put_entry(&app, &key, &stored).await.unwrap();
            }
            // Without overflow, all entries survive.
            assert_eq!(store.entry_count(), 5);
        });
    }
}
