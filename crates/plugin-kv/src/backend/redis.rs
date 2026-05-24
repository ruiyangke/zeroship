//! Redis backend — the distributed-correctness impl.
//!
//! Strongly consistent, atomic INCR, TTL in milliseconds via SET PX.
//! Uses our compio-native Redis client + pool; zero tokio.
//!
//! **Single-node mode** (URL: `redis://host:port`):
//!   Per-worker pool cached in `POOLS`.
//!
//! **Cluster mode** (URL: `redis://seed-host:port/?cluster=true&seeds=...`):
//!   Per-worker `ClusterClient` cached in `CLUSTER_CLIENTS`. Cluster
//!   client handles slot-map + MOVED/ASK redirects transparently.
//!
//! Per-worker caching: the `Redis` struct holds just the URL string
//! (Send+Sync) so the plugin type satisfies `Backend: Send + Sync`. The
//! actual pool / cluster client is lazily created per worker thread on
//! first use — compio's executor is thread-local, so sharing across
//! threads would force an Arc<Mutex<_>> dance for no gain.

use std::cell::RefCell;

use compio_redis::{ClusterClient, Pool};

use super::{classify_incr_error, scope, Backend, TtlState};
use crate::error::{redact_url, KvError};
use crate::limits::escape_glob;

/// Lua for atomic incr-with-TTL-on-create. `KEYS[1]` is the scoped key,
/// `ARGV[1]` the (string) delta, `ARGV[2]` the (string) ttl_ms — empty
/// when no TTL. The script INCRBYs, then PEXPIREs **only when the key
/// did not exist before the increment**, so an existing key's TTL is
/// preserved (fixed-window rate-limit semantics). Returns the new value.
const INCR_TTL_SCRIPT: &str = "local e=redis.call('EXISTS',KEYS[1]); \
local v=redis.call('INCRBY',KEYS[1],ARGV[1]); \
if e==0 and ARGV[2]~='' then redis.call('PEXPIRE',KEYS[1],ARGV[2]) end; \
return v";

/// Map a `compio_redis::Error` raised on the `incr` path into a typed
/// [`KvError`] — server-side numeric complaints become `NonNumeric` /
/// `Overflow`, everything else stays a backend/connection error.
fn map_incr_err(e: compio_redis::Error) -> KvError {
    match e {
        compio_redis::Error::Server(msg) => classify_incr_error(&msg),
        other => map_redis_err("incr", other, None),
    }
}

/// Map a `compio_redis::Error` into a typed [`KvError`]. Connection /
/// transport / pool failures become [`KvError::Connection`] with any
/// URL credentials redacted from the message; the rest become
/// [`KvError::Backend`].
fn map_redis_err(op: &str, e: compio_redis::Error, url: Option<&str>) -> KvError {
    use compio_redis::Error;
    let redacted = |m: &str| match url {
        Some(u) => format!("kv: {op}: {m} ({})", redact_url(u)),
        None => format!("kv: {op}: {m}"),
    };
    match e {
        Error::Io(_)
        | Error::Pool(_)
        | Error::Auth(_)
        | Error::Config(_)
        | Error::ClusterBootstrap(_)
        | Error::NoRoute { .. } => KvError::connection(redacted(&e.to_string())),
        other => KvError::backend(redacted(&other.to_string())),
    }
}

#[derive(Debug)]
pub struct Redis {
    url: String,
    max_size: usize,
}

// Both caches are correctly per-thread: `Pool` and `ClusterClient` are
// `!Send` (compio executors are thread-bound), so a process-wide cache
// would force an `Arc<Mutex<…>>` dance with no benefit. `HashMap::new`
// is not `const` (RandomState seeds at runtime), so the inits stay
// runtime — the first-access guard is one cmpxchg, negligible.
thread_local! {
    /// Per-thread pool cache keyed by URL. Single-node mode only.
    static POOLS: RefCell<std::collections::HashMap<String, Pool>> =
        RefCell::new(std::collections::HashMap::new());

    /// Per-thread cluster-client cache keyed by sorted seed URL list so
    /// two `Redis` backends pointing at the same cluster share a handle.
    static CLUSTER_CLIENTS: RefCell<std::collections::HashMap<String, ClusterClient>> =
        RefCell::new(std::collections::HashMap::new());
}

/// `true` if the URL opts into cluster mode via `?cluster=true`. Also
/// accepts `?cluster=1` / `?cluster=yes` for convenience.
fn is_cluster_url(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.query_pairs()
            .find(|(k, _)| k == "cluster")
            .map(|(_, v)| v.into_owned()))
        .is_some_and(|v| matches!(v.as_str(), "true" | "1" | "yes"))
}

/// Extract seed URLs from a cluster config. Accepts a comma-delimited
/// list under `?seeds=...`; falls back to the base URL as the sole seed.
/// The returned strings are `redis://...` URLs ready for ClusterClient.
fn seeds_from_url(url: &str) -> Vec<String> {
    if let Ok(u) = url::Url::parse(url) {
        if let Some((_, seeds)) = u.query_pairs().find(|(k, _)| k == "seeds") {
            let list: Vec<String> = seeds
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !list.is_empty() { return list; }
        }
    }
    // Fall back to the base URL with the query stripped, so the probe
    // connection doesn't carry `?cluster=true` (ClusterClient parses
    // its own credentials from the URL).
    vec![strip_cluster_query(url)]
}

fn strip_cluster_query(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut u) => {
            u.set_query(None);
            u.into()
        }
        Err(_) => url.to_string(),
    }
}

impl Redis {
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into(), max_size: 16 }
    }

    pub fn with_max_size(url: impl Into<String>, max_size: usize) -> Self {
        Self { url: url.into(), max_size }
    }

    async fn pool(&self) -> Result<Pool, KvError> {
        // Fast path: already initialized on this thread.
        let cached = POOLS.with(|p| p.borrow().get(&self.url).cloned());
        if let Some(p) = cached {
            return Ok(p);
        }
        // Slow path: open + cache. Connect failures redact credentials.
        let pool = Pool::connect(&self.url, self.max_size)
            .await
            .map_err(|e| KvError::connection(format!(
                "kv: redis connect '{}': {e}", redact_url(&self.url)
            )))?;
        POOLS.with(|p| { p.borrow_mut().insert(self.url.clone(), pool.clone()); });
        Ok(pool)
    }

    async fn cluster(&self) -> Result<ClusterClient, KvError> {
        let seeds = seeds_from_url(&self.url);
        let cache_key = {
            let mut s = seeds.clone();
            s.sort();
            s.join(",")
        };
        let cached = CLUSTER_CLIENTS.with(|c| c.borrow().get(&cache_key).cloned());
        if let Some(c) = cached {
            return Ok(c);
        }
        let seed_refs: Vec<&str> = seeds.iter().map(String::as_str).collect();
        let client = ClusterClient::connect(&seed_refs, self.max_size)
            .await
            .map_err(|e| KvError::connection(format!(
                "kv: cluster connect '{}': {e}", redact_url(&self.url)
            )))?;
        CLUSTER_CLIENTS.with(|c| { c.borrow_mut().insert(cache_key, client.clone()); });
        Ok(client)
    }

    /// Acquire a single-node connection, mapping pool-acquire failures
    /// to a typed connection error with credentials redacted.
    async fn conn(&self) -> Result<compio_redis::pool::PooledConn, KvError> {
        let pool = self.pool().await?;
        pool.acquire()
            .await
            .map_err(|e| map_redis_err("acquire", e, Some(&self.url)))
    }
}

#[async_trait::async_trait(?Send)]
impl Backend for Redis {
    async fn get(&self, app_id: &str, key: &str) -> Result<Option<String>, KvError> {
        let scoped = scope(app_id, key);
        let bytes = if is_cluster_url(&self.url) {
            self.cluster().await?
                .get(&scoped).await
                .map_err(|e| map_redis_err("get", e, Some(&self.url)))?
        } else {
            self.conn().await?
                .get(&scoped).await
                .map_err(|e| map_redis_err("get", e, Some(&self.url)))?
        };
        Ok(bytes.map(|b| String::from_utf8_lossy(&b).into_owned()))
    }

    async fn set(
        &self,
        app_id: &str,
        key: &str,
        value: &str,
        ttl_ms: Option<u64>,
    ) -> Result<(), KvError> {
        let scoped = scope(app_id, key);
        if is_cluster_url(&self.url) {
            self.cluster().await?
                .set(&scoped, value.as_bytes(), ttl_ms).await
                .map_err(|e| map_redis_err("set", e, Some(&self.url)))
        } else {
            self.conn().await?
                .set(&scoped, value.as_bytes(), ttl_ms).await
                .map_err(|e| map_redis_err("set", e, Some(&self.url)))
        }
    }

    async fn delete(&self, app_id: &str, key: &str) -> Result<bool, KvError> {
        let scoped = scope(app_id, key);
        if is_cluster_url(&self.url) {
            self.cluster().await?
                .del(&scoped).await
                .map_err(|e| map_redis_err("del", e, Some(&self.url)))
        } else {
            self.conn().await?
                .del(&scoped).await
                .map_err(|e| map_redis_err("del", e, Some(&self.url)))
        }
    }

    async fn incr(
        &self,
        app_id: &str,
        key: &str,
        delta: i64,
        ttl_ms: Option<u64>,
    ) -> Result<i64, KvError> {
        let scoped = scope(app_id, key);
        let delta_s = delta.to_string();
        // ARGV[2] is the ttl_ms string, empty when no TTL — the script
        // only PEXPIREs when the key was created this call.
        let ttl_s = ttl_ms.map(|ms| ms.to_string()).unwrap_or_default();
        let keys = [scoped.as_str()];
        let args = [delta_s.as_str(), ttl_s.as_str()];
        if is_cluster_url(&self.url) {
            self.cluster().await?
                .eval(INCR_TTL_SCRIPT, &keys, &args).await
                .map_err(map_incr_err)
        } else {
            self.conn().await?
                .eval(INCR_TTL_SCRIPT, &keys, &args).await
                .map_err(map_incr_err)
        }
    }

    async fn set_if_absent(
        &self,
        app_id: &str,
        key: &str,
        value: &str,
        ttl_ms: Option<u64>,
    ) -> Result<bool, KvError> {
        let scoped = scope(app_id, key);
        if is_cluster_url(&self.url) {
            self.cluster().await?
                .set_nx(&scoped, value.as_bytes(), ttl_ms).await
                .map_err(|e| map_redis_err("setIfAbsent", e, Some(&self.url)))
        } else {
            self.conn().await?
                .set_nx(&scoped, value.as_bytes(), ttl_ms).await
                .map_err(|e| map_redis_err("setIfAbsent", e, Some(&self.url)))
        }
    }

    async fn expire(&self, app_id: &str, key: &str, ttl_ms: u64) -> Result<bool, KvError> {
        let scoped = scope(app_id, key);
        if is_cluster_url(&self.url) {
            self.cluster().await?
                .pexpire(&scoped, ttl_ms).await
                .map_err(|e| map_redis_err("expire", e, Some(&self.url)))
        } else {
            self.conn().await?
                .pexpire(&scoped, ttl_ms).await
                .map_err(|e| map_redis_err("expire", e, Some(&self.url)))
        }
    }

    async fn ttl(&self, app_id: &str, key: &str) -> Result<TtlState, KvError> {
        let scoped = scope(app_id, key);
        let raw = if is_cluster_url(&self.url) {
            self.cluster().await?
                .pttl(&scoped).await
                .map_err(|e| map_redis_err("ttl", e, Some(&self.url)))?
        } else {
            self.conn().await?
                .pttl(&scoped).await
                .map_err(|e| map_redis_err("ttl", e, Some(&self.url)))?
        };
        // PTTL wire semantics: -2 missing, -1 no expiry, n>=0 ms remaining.
        Ok(match raw {
            -2 => TtlState::Missing,
            -1 => TtlState::NoExpiry,
            n => TtlState::ExpiresInMs(n.max(0) as u64),
        })
    }

    async fn persist(&self, app_id: &str, key: &str) -> Result<bool, KvError> {
        let scoped = scope(app_id, key);
        if is_cluster_url(&self.url) {
            self.cluster().await?
                .persist(&scoped).await
                .map_err(|e| map_redis_err("persist", e, Some(&self.url)))
        } else {
            self.conn().await?
                .persist(&scoped).await
                .map_err(|e| map_redis_err("persist", e, Some(&self.url)))
        }
    }

    async fn list(
        &self,
        app_id: &str,
        prefix: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<String>, Option<String>), KvError> {
        // Hash-tag pattern targets exactly the slot that owns this app.
        // The prefix is glob-escaped so metacharacters in a creator's
        // key prefix can't widen the MATCH. In cluster mode we pass the
        // app_id as the routing key so SCAN hits that specific node.
        let pattern = format!("{{{app_id}}}:{}*", escape_glob(prefix));
        let app_prefix = format!("{{{app_id}}}:");

        // The opaque cursor we hand back to the SDK is the Redis SCAN
        // cursor verbatim; `None` starts a fresh scan at "0". We do ONE
        // SCAN call per `list`, returning whatever batch Redis yields
        // (bounded by COUNT≈limit) plus the next cursor — the SDK pages
        // by passing it straight back. A returned cursor of "0" means
        // the scan is complete (we map that to `None`).
        let scan_cursor = cursor.unwrap_or("0");
        // COUNT is a hint, not a hard cap — Redis may return slightly
        // more or fewer. We pass `limit` as the hint and don't truncate
        // (a truncate would desync the cursor).
        let count = limit.min(u32::MAX as usize) as u32;

        let (next, batch) = if is_cluster_url(&self.url) {
            self.cluster().await?
                .scan(app_id, scan_cursor, &pattern, count).await
                .map_err(|e| map_redis_err("scan", e, Some(&self.url)))?
        } else {
            self.conn().await?
                .scan(scan_cursor, &pattern, count).await
                .map_err(|e| map_redis_err("scan", e, Some(&self.url)))?
        };

        let keys: Vec<String> = batch
            .into_iter()
            .filter_map(|k| k.strip_prefix(&app_prefix).map(str::to_string))
            .collect();
        let next_cursor = if next == "0" { None } else { Some(next) };
        Ok((keys, next_cursor))
    }
}

#[cfg(test)]
mod url_helper_tests {
    use super::*;

    #[test]
    fn is_cluster_url_accepts_true_variants() {
        assert!(is_cluster_url("redis://x:1?cluster=true"));
        assert!(is_cluster_url("redis://x:1?cluster=1"));
        assert!(is_cluster_url("redis://x:1?cluster=yes"));
    }

    #[test]
    fn is_cluster_url_rejects_false_variants() {
        assert!(!is_cluster_url("redis://x:1"));
        assert!(!is_cluster_url("redis://x:1?cluster=false"));
        assert!(!is_cluster_url("redis://x:1?cluster=0"));
        assert!(!is_cluster_url("redis://x:1?cluster=no"));
        assert!(!is_cluster_url("redis://x:1?cluster="));
    }

    #[test]
    fn is_cluster_url_is_case_sensitive() {
        // We're strict by design — the plan spec says lowercase only.
        assert!(!is_cluster_url("redis://x:1?cluster=TRUE"));
        assert!(!is_cluster_url("redis://x:1?cluster=Yes"));
    }

    #[test]
    fn is_cluster_url_ignores_other_query_keys() {
        assert!(!is_cluster_url("redis://x:1?foo=bar"));
        assert!(is_cluster_url("redis://x:1?foo=bar&cluster=true"));
        assert!(is_cluster_url("redis://x:1?cluster=true&extra=x"));
    }

    #[test]
    fn is_cluster_url_handles_unparseable() {
        assert!(!is_cluster_url(""));
        assert!(!is_cluster_url("literal garbage"));
    }

    #[test]
    fn seeds_from_url_with_explicit_seeds() {
        let s = seeds_from_url("redis://a:1?cluster=true&seeds=redis://b:2,redis://c:3");
        assert_eq!(s, vec!["redis://b:2", "redis://c:3"]);
    }

    #[test]
    fn seeds_from_url_whitespace_is_trimmed() {
        let s = seeds_from_url("redis://a:1?cluster=true&seeds=redis://b:2 , redis://c:3 , ");
        assert_eq!(s, vec!["redis://b:2", "redis://c:3"]);
    }

    #[test]
    fn seeds_from_url_empty_seeds_param_falls_back_to_base() {
        // `?seeds=` alone provides no seeds; fall through to base URL
        // with the ?cluster=true query stripped so it's a clean probe URL.
        let s = seeds_from_url("redis://a:1?cluster=true&seeds=");
        assert_eq!(s.len(), 1);
        assert!(!s[0].contains("cluster="));
        assert!(!s[0].contains("seeds="));
    }

    #[test]
    fn seeds_from_url_no_seeds_param_uses_base() {
        let s = seeds_from_url("redis://only-host:7000?cluster=true");
        assert_eq!(s.len(), 1);
        assert!(s[0].starts_with("redis://only-host:7000"));
        assert!(!s[0].contains("cluster="));
    }

    #[test]
    fn seeds_from_url_unparseable_base_passes_through() {
        // If url::Url::parse fails, we don't crash — caller will see
        // the error when ClusterClient::connect rejects the seed.
        let s = seeds_from_url("not a url");
        assert_eq!(s, vec!["not a url"]);
    }

    #[test]
    fn strip_cluster_query_removes_all_query() {
        let out = strip_cluster_query("redis://h:1?cluster=true&x=y");
        assert!(out.starts_with("redis://h:1"));
        assert!(!out.contains('?'), "query not stripped: {out}");
        assert!(!out.contains("cluster="));
        assert!(!out.contains("x=y"));
    }

    #[test]
    fn strip_cluster_query_leaves_query_less_urls_alone() {
        let out = strip_cluster_query("redis://h:1");
        // Roundtripping through url::Url is stable; accept either canonical
        // form since both are valid Client::connect inputs.
        assert!(out == "redis://h:1" || out == "redis://h:1/", "got {out}");
    }

    #[test]
    fn strip_cluster_query_ungarbled_on_invalid_url() {
        // Unparseable → pass through unchanged so downstream can error.
        assert_eq!(strip_cluster_query("not a url"), "not a url");
    }
}
