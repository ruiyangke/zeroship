//! Redis backend — the distributed-correctness impl.
//!
//! Strongly consistent, atomic INCR, TTL in milliseconds via SET PX.
//! Uses our compio-native Redis client + pool; zero tokio.
//!
//! Per-worker pool: the `Redis` struct holds just the URL (Send+Sync)
//! so the plugin type satisfies `Backend: Send + Sync`. The actual
//! connection pool is lazily created per worker thread on first use
//! — compio's executor is thread-local, so sharing a pool across
//! threads would force an Arc<Mutex<_>> dance for no gain. Each worker
//! owns its own pool + connections to Redis.

use std::cell::RefCell;

use compio_redis::Pool;

use super::{scope, Backend};

#[derive(Debug)]
pub struct Redis {
    url: String,
    max_size: usize,
}

thread_local! {
    /// Per-thread pool cache keyed by URL. Worker threads hit this on
    /// every kv op; the first op on a thread opens the pool, later ops
    /// reuse it. Multiple `Redis` backends with different URLs on the
    /// same thread each get their own entry.
    static POOLS: RefCell<std::collections::HashMap<String, Pool>> =
        RefCell::new(std::collections::HashMap::new());
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
}

#[async_trait::async_trait(?Send)]
impl Backend for Redis {
    async fn get(&self, app_id: &str, key: &str) -> Result<Option<String>, String> {
        let pool = self.pool().await?;
        let mut conn = pool.acquire().await.map_err(|e| format!("kv: {e}"))?;
        let bytes = conn
            .get(&scope(app_id, key))
            .await
            .map_err(|e| format!("kv: get: {e}"))?;
        Ok(bytes.map(|b| String::from_utf8_lossy(&b).into_owned()))
    }

    async fn set(
        &self,
        app_id: &str,
        key: &str,
        value: &str,
        ttl_ms: Option<u64>,
    ) -> Result<(), String> {
        let pool = self.pool().await?;
        let mut conn = pool.acquire().await.map_err(|e| format!("kv: {e}"))?;
        conn.set(&scope(app_id, key), value.as_bytes(), ttl_ms)
            .await
            .map_err(|e| format!("kv: set: {e}"))
    }

    async fn delete(&self, app_id: &str, key: &str) -> Result<bool, String> {
        let pool = self.pool().await?;
        let mut conn = pool.acquire().await.map_err(|e| format!("kv: {e}"))?;
        conn.del(&scope(app_id, key))
            .await
            .map_err(|e| format!("kv: del: {e}"))
    }

    async fn incr(&self, app_id: &str, key: &str, delta: i64) -> Result<i64, String> {
        let pool = self.pool().await?;
        let mut conn = pool.acquire().await.map_err(|e| format!("kv: {e}"))?;
        conn.incr_by(&scope(app_id, key), delta)
            .await
            .map_err(|e| format!("kv: incr: {e}"))
    }

    async fn list(&self, app_id: &str, prefix: &str) -> Result<Vec<String>, String> {
        let pool = self.pool().await?;
        let mut conn = pool.acquire().await.map_err(|e| format!("kv: {e}"))?;
        let pattern = format!("{}:*", scope(app_id, prefix).trim_end_matches(':'));

        let mut cursor = String::from("0");
        let mut acc: Vec<String> = Vec::new();
        let app_prefix = format!("{app_id}:");
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
        acc.sort();
        acc.dedup();
        Ok(acc)
    }
}
