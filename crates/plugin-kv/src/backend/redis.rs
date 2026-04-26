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

use super::{scope, Backend};

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

    async fn pool(&self) -> Result<Pool, String> {
        // Fast path: already initialized on this thread.
        let cached = POOLS.with(|p| p.borrow().get(&self.url).cloned());
        if let Some(p) = cached {
            return Ok(p);
        }
        // Slow path: open + cache.
        let pool = Pool::connect(&self.url, self.max_size)
            .await
            .map_err(|e| format!("kv: redis connect '{}': {e}", self.url))?;
        POOLS.with(|p| { p.borrow_mut().insert(self.url.clone(), pool.clone()); });
        Ok(pool)
    }

    async fn cluster(&self) -> Result<ClusterClient, String> {
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
            .map_err(|e| format!("kv: cluster connect '{}': {e}", self.url))?;
        CLUSTER_CLIENTS.with(|c| { c.borrow_mut().insert(cache_key, client.clone()); });
        Ok(client)
    }
}

#[async_trait::async_trait(?Send)]
impl Backend for Redis {
    async fn get(&self, app_id: &str, key: &str) -> Result<Option<String>, String> {
        let scoped = scope(app_id, key);
        let bytes = if is_cluster_url(&self.url) {
            self.cluster().await?
                .get(&scoped).await
                .map_err(|e| format!("kv: get: {e}"))?
        } else {
            let pool = self.pool().await?;
            let mut conn = pool.acquire().await.map_err(|e| format!("kv: {e}"))?;
            conn.get(&scoped).await.map_err(|e| format!("kv: get: {e}"))?
        };
        Ok(bytes.map(|b| String::from_utf8_lossy(&b).into_owned()))
    }

    async fn set(
        &self,
        app_id: &str,
        key: &str,
        value: &str,
        ttl_ms: Option<u64>,
    ) -> Result<(), String> {
        let scoped = scope(app_id, key);
        if is_cluster_url(&self.url) {
            self.cluster().await?
                .set(&scoped, value.as_bytes(), ttl_ms).await
                .map_err(|e| format!("kv: set: {e}"))
        } else {
            let pool = self.pool().await?;
            let mut conn = pool.acquire().await.map_err(|e| format!("kv: {e}"))?;
            conn.set(&scoped, value.as_bytes(), ttl_ms).await
                .map_err(|e| format!("kv: set: {e}"))
        }
    }

    async fn delete(&self, app_id: &str, key: &str) -> Result<bool, String> {
        let scoped = scope(app_id, key);
        if is_cluster_url(&self.url) {
            self.cluster().await?
                .del(&scoped).await
                .map_err(|e| format!("kv: del: {e}"))
        } else {
            let pool = self.pool().await?;
            let mut conn = pool.acquire().await.map_err(|e| format!("kv: {e}"))?;
            conn.del(&scoped).await.map_err(|e| format!("kv: del: {e}"))
        }
    }

    async fn incr(&self, app_id: &str, key: &str, delta: i64) -> Result<i64, String> {
        let scoped = scope(app_id, key);
        if is_cluster_url(&self.url) {
            self.cluster().await?
                .incr_by(&scoped, delta).await
                .map_err(|e| format!("kv: incr: {e}"))
        } else {
            let pool = self.pool().await?;
            let mut conn = pool.acquire().await.map_err(|e| format!("kv: {e}"))?;
            conn.incr_by(&scoped, delta).await
                .map_err(|e| format!("kv: incr: {e}"))
        }
    }

    async fn list(&self, app_id: &str, prefix: &str) -> Result<Vec<String>, String> {
        // Hash-tag pattern targets exactly the slot that owns this app.
        // In cluster mode, we pass the app_id as the routing key so SCAN
        // hits that specific node; single-node mode just iterates the
        // whole keyspace.
        let pattern = format!("{{{app_id}}}:{prefix}*");
        let app_prefix = format!("{{{app_id}}}:");

        let mut cursor = String::from("0");
        let mut acc: Vec<String> = Vec::new();

        if is_cluster_url(&self.url) {
            let client = self.cluster().await?;
            loop {
                let (next, batch) = client
                    .scan(app_id, &cursor, &pattern, 500)
                    .await
                    .map_err(|e| format!("kv: scan: {e}"))?;
                for k in batch {
                    if let Some(stripped) = k.strip_prefix(&app_prefix) {
                        acc.push(stripped.to_string());
                    }
                }
                if next == "0" { break; }
                cursor = next;
            }
        } else {
            let pool = self.pool().await?;
            let mut conn = pool.acquire().await.map_err(|e| format!("kv: {e}"))?;
            loop {
                let (next, batch) = conn
                    .scan(&cursor, &pattern, 500)
                    .await
                    .map_err(|e| format!("kv: scan: {e}"))?;
                for k in batch {
                    if let Some(stripped) = k.strip_prefix(&app_prefix) {
                        acc.push(stripped.to_string());
                    }
                }
                if next == "0" { break; }
                cursor = next;
            }
        }

        acc.sort();
        acc.dedup();
        Ok(acc)
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
