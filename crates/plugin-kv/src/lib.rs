//! Key-value plugin — `zeroship.kv.*` native primitives.
//!
//! In-memory backend for dev; Redis/Upstash backend goes behind the
//! same `Store` trait later. Values are JSON-serializable (strings in
//! the wire protocol — the SDK handles typed serde).
//!
//! Native API:
//! - `zeroship.kv.get(key)` → Promise<string | null>
//! - `zeroship.kv.set(key, value, ttlMs?)` → Promise<{ ok: true }>
//! - `zeroship.kv.delete(key)` → Promise<{ deleted: bool }>
//! - `zeroship.kv.incr(key, delta?)` → Promise<number>
//! - `zeroship.kv.list(prefix?)` → Promise<string[]>
//!
//! Keyspace is per-app: the `<app_id>:` prefix is prepended to every key
//! so multi-tenant workers don't collide.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};

pub mod callbacks;

// ---------------------------------------------------------------------------
// In-memory store
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub value: String,
    /// Absolute expiration time; None means no TTL.
    pub expires_at: Option<Instant>,
}

impl Entry {
    pub fn is_expired(&self) -> bool {
        self.expires_at.map(|t| Instant::now() >= t).unwrap_or(false)
    }
}

thread_local! {
    /// In-memory store, shared across all requests on this thread. Lazy
    /// expiration: expired entries are pruned on access, not via a sweep.
    pub(crate) static STORE: RefCell<HashMap<String, Entry>> =
        RefCell::new(HashMap::new());
}

pub(crate) fn scoped_key(app_id: &str, key: &str) -> String {
    format!("{app_id}:{key}")
}

pub(crate) fn store_get(scoped: &str) -> Option<String> {
    STORE.with(|s| {
        let mut map = s.borrow_mut();
        if let Some(entry) = map.get(scoped) {
            if entry.is_expired() {
                map.remove(scoped);
                return None;
            }
            return Some(entry.value.clone());
        }
        None
    })
}

pub(crate) fn store_set(scoped: String, value: String, ttl_ms: Option<u64>) {
    let expires_at = ttl_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
    STORE.with(|s| {
        s.borrow_mut().insert(scoped, Entry { value, expires_at });
    });
}

pub(crate) fn store_delete(scoped: &str) -> bool {
    STORE.with(|s| s.borrow_mut().remove(scoped).is_some())
}

pub(crate) fn store_list_prefix(scoped_prefix: &str) -> Vec<String> {
    STORE.with(|s| {
        let mut map = s.borrow_mut();
        // Opportunistically evict expired entries during list.
        let expired: Vec<String> = map
            .iter()
            .filter(|(_, e)| e.is_expired())
            .map(|(k, _)| k.clone())
            .collect();
        for k in expired { map.remove(&k); }

        map.keys()
            .filter(|k| k.starts_with(scoped_prefix))
            // Strip the `<app_id>:` prefix before returning to user.
            .filter_map(|k| k.splitn(2, ':').nth(1).map(str::to_string))
            .collect::<Vec<_>>()
    })
}

// ---------------------------------------------------------------------------
// KvPlugin
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct KvPlugin;

impl std::fmt::Debug for KvPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KvPlugin").finish()
    }
}

impl KvPlugin {
    #[must_use]
    pub fn new() -> Self { Self }
}

impl NativePlugin for KvPlugin {
    fn namespace(&self) -> &str { "kv" }
    fn name(&self) -> &str { "kv" }

    fn register(&self, r: &mut NativeRegistrar) {
        r.add("get", callbacks::get);
        r.add("set", callbacks::set);
        r.add("delete", callbacks::delete);
        r.add("incr", callbacks::incr);
        r.add("list", callbacks::list);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_expires() {
        STORE.with(|s| s.borrow_mut().clear());
        store_set(scoped_key("a", "k"), "v".into(), Some(1));
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(store_get(&scoped_key("a", "k")), None);
    }

    #[test]
    fn app_isolation() {
        STORE.with(|s| s.borrow_mut().clear());
        store_set(scoped_key("a", "x"), "1".into(), None);
        store_set(scoped_key("b", "x"), "2".into(), None);
        assert_eq!(store_get(&scoped_key("a", "x")), Some("1".into()));
        assert_eq!(store_get(&scoped_key("b", "x")), Some("2".into()));
        assert_eq!(store_list_prefix("a:").len(), 1);
    }
}
