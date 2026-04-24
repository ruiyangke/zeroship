//! In-memory backend — dev only. Per-worker HashMap, no cross-worker state.
//!
//! Uses a `Mutex<HashMap>` so the backend is `Send + Sync` (required by
//! the plugin trait). Single-threaded workers don't contend; multi-worker
//! uses the Redis backend in production.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::{scope, Backend};

#[derive(Debug)]
struct Entry {
    value: String,
    expires_at: Option<Instant>,
}

impl Entry {
    fn is_expired(&self) -> bool {
        self.expires_at.map(|t| Instant::now() >= t).unwrap_or(false)
    }
}

#[derive(Debug)]
pub struct InMemory {
    store: Mutex<HashMap<String, Entry>>,
}

impl InMemory {
    pub fn new() -> Self {
        Self { store: Mutex::new(HashMap::new()) }
    }
}

impl Default for InMemory {
    fn default() -> Self { Self::new() }
}

#[async_trait::async_trait(?Send)]
impl Backend for InMemory {
    async fn get(&self, app_id: &str, key: &str) -> Result<Option<String>, String> {
        let scoped = scope(app_id, key);
        let mut map = self.store.lock().unwrap();
        if let Some(entry) = map.get(&scoped) {
            if entry.is_expired() {
                map.remove(&scoped);
                return Ok(None);
            }
            return Ok(Some(entry.value.clone()));
        }
        Ok(None)
    }

    async fn set(
        &self,
        app_id: &str,
        key: &str,
        value: &str,
        ttl_ms: Option<u64>,
    ) -> Result<(), String> {
        let scoped = scope(app_id, key);
        let expires_at = ttl_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
        self.store.lock().unwrap().insert(
            scoped,
            Entry { value: value.to_string(), expires_at },
        );
        Ok(())
    }

    async fn delete(&self, app_id: &str, key: &str) -> Result<bool, String> {
        let scoped = scope(app_id, key);
        Ok(self.store.lock().unwrap().remove(&scoped).is_some())
    }

    async fn incr(&self, app_id: &str, key: &str, delta: i64) -> Result<i64, String> {
        let scoped = scope(app_id, key);
        let mut map = self.store.lock().unwrap();
        let current = map
            .get(&scoped)
            .filter(|e| !e.is_expired())
            .and_then(|e| e.value.parse::<i64>().ok())
            .unwrap_or(0);
        let next = current.saturating_add(delta);
        map.insert(
            scoped,
            Entry { value: next.to_string(), expires_at: None },
        );
        Ok(next)
    }

    async fn list(&self, app_id: &str, prefix: &str) -> Result<Vec<String>, String> {
        let scoped_prefix = scope(app_id, prefix);
        let mut map = self.store.lock().unwrap();
        let expired: Vec<String> = map
            .iter()
            .filter(|(_, e)| e.is_expired())
            .map(|(k, _)| k.clone())
            .collect();
        for k in expired { map.remove(&k); }

        // Strip the literal `{app_id}:` prefix (braces included) from each
        // stored key so the user sees their unscoped names back.
        let strip = format!("{{{app_id}}}:");
        Ok(map.keys()
            .filter(|k| k.starts_with(&scoped_prefix))
            .filter_map(|k| k.strip_prefix(&strip).map(str::to_string))
            .collect())
    }
}
