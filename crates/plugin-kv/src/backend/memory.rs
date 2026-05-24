//! In-memory backend — dev only. Per-worker HashMap, no cross-worker state.
//!
//! Uses a `Mutex<HashMap>` so the backend is `Send + Sync` (required by
//! the plugin trait). Single-threaded workers don't contend; multi-worker
//! uses the Redis backend in production.
//!
//! Conforms to the canonical `incr` contract (overflow → error,
//! non-numeric → error, preserve existing TTL) and the full expanded
//! surface (`set_if_absent` / `expire` / `ttl` / `persist` / paginated
//! `list`). The lock is taken with `unwrap_or_else(|e|
//! e.into_inner())` (non-poisoning) — a panic in one op must not wedge
//! the whole worker's KV.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::{scope, Backend, TtlState};
use crate::error::KvError;

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

    /// Lock the store without poisoning: a panic in one op leaves the
    /// data intact, so we recover the guard rather than propagate the
    /// poison (which would wedge every subsequent KV op on this worker).
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Default for InMemory {
    fn default() -> Self { Self::new() }
}

/// Convert a TTL in milliseconds into an `Instant` deadline.
fn deadline(ttl_ms: u64) -> Instant {
    Instant::now() + Duration::from_millis(ttl_ms)
}

#[async_trait::async_trait(?Send)]
impl Backend for InMemory {
    async fn get(&self, app_id: &str, key: &str) -> Result<Option<String>, KvError> {
        let scoped = scope(app_id, key);
        let mut map = self.lock();
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
    ) -> Result<(), KvError> {
        let scoped = scope(app_id, key);
        let expires_at = ttl_ms.map(deadline);
        self.lock()
            .insert(scoped, Entry { value: value.to_string(), expires_at });
        Ok(())
    }

    async fn delete(&self, app_id: &str, key: &str) -> Result<bool, KvError> {
        let scoped = scope(app_id, key);
        Ok(self.lock().remove(&scoped).is_some())
    }

    async fn incr(
        &self,
        app_id: &str,
        key: &str,
        delta: i64,
        ttl_ms: Option<u64>,
    ) -> Result<i64, KvError> {
        let scoped = scope(app_id, key);
        let mut map = self.lock();

        // Treat an expired entry as absent (and reap it).
        let live = match map.get(&scoped) {
            Some(e) if e.is_expired() => {
                map.remove(&scoped);
                None
            }
            Some(_) => map.get(&scoped),
            None => None,
        };

        match live {
            Some(entry) => {
                // Existing key: parse, add (checked), preserve TTL.
                let current = entry.value.parse::<i64>().map_err(|_| {
                    KvError::non_numeric(format!(
                        "kv: incr on non-numeric value for key '{key}'"
                    ))
                })?;
                let next = current.checked_add(delta).ok_or_else(|| {
                    KvError::overflow(format!(
                        "kv: incr overflowed i64 for key '{key}'"
                    ))
                })?;
                let expires_at = entry.expires_at; // preserved
                map.insert(scoped, Entry { value: next.to_string(), expires_at });
                Ok(next)
            }
            None => {
                // Created this call: apply ttl_ms (fixed-window).
                let next = 0_i64.checked_add(delta).ok_or_else(|| {
                    KvError::overflow(format!(
                        "kv: incr overflowed i64 for key '{key}'"
                    ))
                })?;
                let expires_at = ttl_ms.map(deadline);
                map.insert(scoped, Entry { value: next.to_string(), expires_at });
                Ok(next)
            }
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
        let mut map = self.lock();
        // An expired entry counts as absent.
        let occupied = map.get(&scoped).is_some_and(|e| !e.is_expired());
        if occupied {
            return Ok(false);
        }
        let expires_at = ttl_ms.map(deadline);
        map.insert(scoped, Entry { value: value.to_string(), expires_at });
        Ok(true)
    }

    async fn expire(&self, app_id: &str, key: &str, ttl_ms: u64) -> Result<bool, KvError> {
        let scoped = scope(app_id, key);
        let mut map = self.lock();
        match map.get_mut(&scoped) {
            Some(entry) if !entry.is_expired() => {
                entry.expires_at = Some(deadline(ttl_ms));
                Ok(true)
            }
            Some(_) => {
                // Expired — reap and report missing.
                map.remove(&scoped);
                Ok(false)
            }
            None => Ok(false),
        }
    }

    async fn ttl(&self, app_id: &str, key: &str) -> Result<TtlState, KvError> {
        let scoped = scope(app_id, key);
        let mut map = self.lock();
        match map.get(&scoped) {
            Some(entry) if entry.is_expired() => {
                map.remove(&scoped);
                Ok(TtlState::Missing)
            }
            Some(entry) => match entry.expires_at {
                None => Ok(TtlState::NoExpiry),
                Some(at) => {
                    let now = Instant::now();
                    let remaining = at.saturating_duration_since(now);
                    Ok(TtlState::ExpiresInMs(remaining.as_millis() as u64))
                }
            },
            None => Ok(TtlState::Missing),
        }
    }

    async fn persist(&self, app_id: &str, key: &str) -> Result<bool, KvError> {
        let scoped = scope(app_id, key);
        let mut map = self.lock();
        match map.get_mut(&scoped) {
            Some(entry) if !entry.is_expired() => {
                if entry.expires_at.is_some() {
                    entry.expires_at = None;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Some(_) => {
                map.remove(&scoped);
                Ok(false)
            }
            None => Ok(false),
        }
    }

    async fn list(
        &self,
        app_id: &str,
        prefix: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<String>, Option<String>), KvError> {
        let scoped_prefix = scope(app_id, prefix);
        let strip = format!("{{{app_id}}}:");
        let mut map = self.lock();

        // Reap expired entries so they don't appear in the listing.
        let expired: Vec<String> = map
            .iter()
            .filter(|(_, e)| e.is_expired())
            .map(|(k, _)| k.clone())
            .collect();
        for k in expired {
            map.remove(&k);
        }

        // Sorted view of matching unscoped keys; the cursor is the last
        // unscoped key returned in the previous page, so we resume
        // strictly after it.
        let mut keys: Vec<String> = map
            .keys()
            .filter(|k| k.starts_with(&scoped_prefix))
            .filter_map(|k| k.strip_prefix(&strip).map(str::to_string))
            .collect();
        keys.sort();

        let start = match cursor {
            Some(c) => keys.partition_point(|k| k.as_str() <= c),
            None => 0,
        };

        let page: Vec<String> = keys.iter().skip(start).take(limit).cloned().collect();
        // More keys remain iff we returned a full page AND there's at
        // least one key beyond it.
        let next_cursor = if start + page.len() < keys.len() {
            page.last().cloned()
        } else {
            None
        };
        Ok((page, next_cursor))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP: &str = "test-app";

    #[compio::test]
    async fn get_set_delete_roundtrip() {
        let b = InMemory::new();
        assert!(b.get(APP, "k").await.unwrap().is_none());
        b.set(APP, "k", "v", None).await.unwrap();
        assert_eq!(b.get(APP, "k").await.unwrap().as_deref(), Some("v"));
        assert!(b.delete(APP, "k").await.unwrap());
        assert!(!b.delete(APP, "k").await.unwrap());
        assert!(b.get(APP, "k").await.unwrap().is_none());
    }

    #[compio::test]
    async fn set_overwrites_value_and_ttl() {
        let b = InMemory::new();
        b.set(APP, "k", "first", Some(100_000)).await.unwrap();
        b.set(APP, "k", "second", None).await.unwrap();
        assert_eq!(b.get(APP, "k").await.unwrap().as_deref(), Some("second"));
        assert_eq!(b.ttl(APP, "k").await.unwrap(), TtlState::NoExpiry);
    }

    #[compio::test]
    async fn empty_value_is_allowed() {
        let b = InMemory::new();
        b.set(APP, "k", "", None).await.unwrap();
        assert_eq!(b.get(APP, "k").await.unwrap().as_deref(), Some(""));
    }

    #[compio::test]
    async fn incr_creates_and_accumulates() {
        let b = InMemory::new();
        assert_eq!(b.incr(APP, "c", 5, None).await.unwrap(), 5);
        assert_eq!(b.incr(APP, "c", -3, None).await.unwrap(), 2);
        assert_eq!(b.incr(APP, "c", 1, None).await.unwrap(), 3);
    }

    #[compio::test]
    async fn incr_on_seeded_numeric_value() {
        let b = InMemory::new();
        b.set(APP, "c", "100", None).await.unwrap();
        assert_eq!(b.incr(APP, "c", 5, None).await.unwrap(), 105);
    }

    #[compio::test]
    async fn incr_non_numeric_is_error() {
        let b = InMemory::new();
        b.set(APP, "c", "notanumber", None).await.unwrap();
        match b.incr(APP, "c", 1, None).await {
            Err(KvError::NonNumeric { .. }) => {}
            other => panic!("expected NonNumeric, got {other:?}"),
        }
    }

    #[compio::test]
    async fn incr_overflow_is_error() {
        let b = InMemory::new();
        b.set(APP, "c", &i64::MAX.to_string(), None).await.unwrap();
        match b.incr(APP, "c", 1, None).await {
            Err(KvError::Overflow { .. }) => {}
            other => panic!("expected Overflow, got {other:?}"),
        }
    }

    #[compio::test]
    async fn incr_preserves_existing_ttl() {
        let b = InMemory::new();
        // Create with a TTL via incr (key created this call).
        assert_eq!(b.incr(APP, "c", 1, Some(100_000)).await.unwrap(), 1);
        let before = b.ttl(APP, "c").await.unwrap();
        assert!(matches!(before, TtlState::ExpiresInMs(_)));
        // Subsequent incr must NOT reset/clear the TTL.
        b.incr(APP, "c", 1, Some(50)).await.unwrap();
        let after = b.ttl(APP, "c").await.unwrap();
        assert!(
            matches!(after, TtlState::ExpiresInMs(ms) if ms > 1000),
            "TTL should be preserved (~100s), got {after:?}"
        );
    }

    #[compio::test]
    async fn incr_ttl_only_applies_on_create() {
        let b = InMemory::new();
        // Existing key with no TTL.
        b.set(APP, "c", "10", None).await.unwrap();
        b.incr(APP, "c", 1, Some(100_000)).await.unwrap();
        // incr's ttl_ms must NOT apply to a pre-existing key.
        assert_eq!(b.ttl(APP, "c").await.unwrap(), TtlState::NoExpiry);
    }

    #[compio::test]
    async fn set_if_absent_stores_only_when_absent() {
        let b = InMemory::new();
        assert!(b.set_if_absent(APP, "lock", "1", None).await.unwrap());
        assert!(!b.set_if_absent(APP, "lock", "2", None).await.unwrap());
        assert_eq!(b.get(APP, "lock").await.unwrap().as_deref(), Some("1"));
    }

    #[compio::test]
    async fn set_if_absent_treats_expired_as_absent() {
        let b = InMemory::new();
        b.set(APP, "lock", "old", Some(1)).await.unwrap();
        compio::time::sleep(Duration::from_millis(10)).await;
        assert!(b.set_if_absent(APP, "lock", "new", None).await.unwrap());
        assert_eq!(b.get(APP, "lock").await.unwrap().as_deref(), Some("new"));
    }

    #[compio::test]
    async fn expire_sets_ttl_and_reports_missing() {
        let b = InMemory::new();
        assert!(!b.expire(APP, "k", 1000).await.unwrap()); // missing
        b.set(APP, "k", "v", None).await.unwrap();
        assert!(b.expire(APP, "k", 100_000).await.unwrap());
        assert!(matches!(b.ttl(APP, "k").await.unwrap(), TtlState::ExpiresInMs(_)));
    }

    #[compio::test]
    async fn ttl_distinguishes_missing_no_expiry_and_expiring() {
        let b = InMemory::new();
        assert_eq!(b.ttl(APP, "k").await.unwrap(), TtlState::Missing);
        b.set(APP, "k", "v", None).await.unwrap();
        assert_eq!(b.ttl(APP, "k").await.unwrap(), TtlState::NoExpiry);
        b.set(APP, "k", "v", Some(100_000)).await.unwrap();
        assert!(matches!(b.ttl(APP, "k").await.unwrap(), TtlState::ExpiresInMs(_)));
    }

    #[compio::test]
    async fn persist_removes_ttl() {
        let b = InMemory::new();
        b.set(APP, "k", "v", Some(100_000)).await.unwrap();
        assert!(b.persist(APP, "k").await.unwrap()); // had a TTL
        assert_eq!(b.ttl(APP, "k").await.unwrap(), TtlState::NoExpiry);
        assert!(!b.persist(APP, "k").await.unwrap()); // already no TTL
        assert!(!b.persist(APP, "missing").await.unwrap()); // missing
    }

    #[compio::test]
    async fn expired_keys_are_reaped_on_access() {
        let b = InMemory::new();
        b.set(APP, "k", "v", Some(1)).await.unwrap();
        compio::time::sleep(Duration::from_millis(10)).await;
        assert!(b.get(APP, "k").await.unwrap().is_none());
        assert_eq!(b.ttl(APP, "k").await.unwrap(), TtlState::Missing);
    }

    #[compio::test]
    async fn list_filters_by_prefix_and_strips_scope() {
        let b = InMemory::new();
        for k in ["user:1", "user:2", "post:1"] {
            b.set(APP, k, "x", None).await.unwrap();
        }
        let (mut users, cursor) = b.list(APP, "user:", None, 100).await.unwrap();
        users.sort();
        assert_eq!(users, vec!["user:1", "user:2"]);
        assert!(cursor.is_none());
    }

    #[compio::test]
    async fn list_isolates_apps() {
        let b = InMemory::new();
        b.set("app-a", "k", "a", None).await.unwrap();
        b.set("app-b", "k", "b", None).await.unwrap();
        let (a_keys, _) = b.list("app-a", "", None, 100).await.unwrap();
        let (b_keys, _) = b.list("app-b", "", None, 100).await.unwrap();
        assert_eq!(a_keys, vec!["k"]);
        assert_eq!(b_keys, vec!["k"]);
    }

    #[compio::test]
    async fn list_paginates_with_cursor() {
        let b = InMemory::new();
        for i in 0..5 {
            b.set(APP, &format!("k{i}"), "x", None).await.unwrap();
        }
        // Page 1: limit 2.
        let (page1, c1) = b.list(APP, "k", None, 2).await.unwrap();
        assert_eq!(page1, vec!["k0", "k1"]);
        let c1 = c1.expect("more pages remain");

        // Page 2.
        let (page2, c2) = b.list(APP, "k", Some(&c1), 2).await.unwrap();
        assert_eq!(page2, vec!["k2", "k3"]);
        let c2 = c2.expect("more pages remain");

        // Page 3 (final).
        let (page3, c3) = b.list(APP, "k", Some(&c2), 2).await.unwrap();
        assert_eq!(page3, vec!["k4"]);
        assert!(c3.is_none(), "last page must return cursor None");
    }

    #[compio::test]
    async fn list_full_page_at_exact_boundary_ends() {
        let b = InMemory::new();
        for i in 0..2 {
            b.set(APP, &format!("k{i}"), "x", None).await.unwrap();
        }
        // A page that exactly consumes all keys must report no more.
        let (page, cursor) = b.list(APP, "k", None, 2).await.unwrap();
        assert_eq!(page, vec!["k0", "k1"]);
        assert!(cursor.is_none());
    }

    #[compio::test]
    async fn list_empty_returns_empty() {
        let b = InMemory::new();
        let (keys, cursor) = b.list(APP, "none:", None, 100).await.unwrap();
        assert!(keys.is_empty());
        assert!(cursor.is_none());
    }
}
