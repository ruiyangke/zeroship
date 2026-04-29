//! Edge in-memory LRU cache for blob bytes, keyed by content hash.
//!
//! Phase A of the blob-store zero-copy plan (see
//! `docs/architecture/blob-store.md`). Bytes are still copied once on
//! socket write — `Bytes` is `Arc`-refcounted so concurrent requests
//! for the same hash share the buffer. Phase B layers mmap on top;
//! Phase C swaps in `sendfile(2)`.

use std::sync::Mutex;

use bytes::Bytes;
use lru::LruCache;

/// Bounded-by-bytes LRU cache keyed by blob hash. Insert of a single
/// entry larger than the budget is a no-op so one giant asset can't
/// evict everything.
pub struct BlobCache {
    state: Mutex<CacheState>,
    max_bytes: usize,
}

struct CacheState {
    lru: LruCache<String, Bytes>,
    current_bytes: usize,
}

impl BlobCache {
    /// Create a cache with the given byte budget. The cache is
    /// internally unbounded by entry-count — eviction is byte-driven.
    #[must_use]
    pub fn new(max_bytes: usize) -> Self {
        Self {
            state: Mutex::new(CacheState {
                lru: LruCache::unbounded(),
                current_bytes: 0,
            }),
            max_bytes,
        }
    }

    /// Look up a blob, promoting the entry to most-recently-used on hit.
    pub fn get(&self, hash: &str) -> Option<Bytes> {
        let mut state = self.state.lock().expect("BlobCache mutex poisoned");
        state.lru.get(hash).cloned()
    }

    /// Insert (or replace) a blob. Evicts oldest entries until the new
    /// entry fits inside `max_bytes`. If the new entry alone exceeds
    /// `max_bytes`, this is a silent no-op — one big asset must not
    /// evict everything else.
    pub fn insert(&self, hash: String, bytes: Bytes) {
        let new_size = bytes.len();
        if new_size > self.max_bytes {
            return;
        }
        let mut state = self.state.lock().expect("BlobCache mutex poisoned");
        // Replacing an existing key needs to subtract the old size first
        // so the byte accounting tracks the delta, not the gross sum.
        if let Some(old) = state.lru.pop(&hash) {
            state.current_bytes = state.current_bytes.saturating_sub(old.len());
        }
        while state.current_bytes + new_size > self.max_bytes {
            match state.lru.pop_lru() {
                Some((_, evicted)) => {
                    state.current_bytes = state.current_bytes.saturating_sub(evicted.len());
                }
                None => break,
            }
        }
        state.current_bytes += new_size;
        state.lru.put(hash, bytes);
    }

    /// Total bytes currently held across all entries.
    #[must_use]
    pub fn current_bytes(&self) -> usize {
        self.state.lock().expect("BlobCache mutex poisoned").current_bytes
    }

    /// Number of entries currently cached.
    #[must_use]
    pub fn len(&self) -> usize {
        self.state.lock().expect("BlobCache mutex poisoned").lru.len()
    }

    /// True iff no entries are cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for BlobCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.lock();
        let (len, current) = match &state {
            Ok(s) => (s.lru.len(), s.current_bytes),
            Err(p) => (p.get_ref().lru.len(), p.get_ref().current_bytes),
        };
        f.debug_struct("BlobCache")
            .field("max_bytes", &self.max_bytes)
            .field("current_bytes", &current)
            .field("len", &len)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn b(n: usize) -> Bytes {
        Bytes::from(vec![0u8; n])
    }

    #[test]
    fn miss_returns_none() {
        let cache = BlobCache::new(1024);
        assert!(cache.get("deadbeef").is_none());
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.current_bytes(), 0);
    }

    #[test]
    fn hit_returns_inserted_bytes() {
        let cache = BlobCache::new(1024);
        let payload = Bytes::from_static(b"hello");
        cache.insert("a".into(), payload.clone());
        assert_eq!(cache.get("a"), Some(payload));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.current_bytes(), 5);
    }

    #[test]
    fn evicts_oldest_when_over_budget() {
        let cache = BlobCache::new(10);
        cache.insert("a".into(), b(4));
        cache.insert("b".into(), b(4));
        // Both fit (8 bytes used, 10 budget).
        assert_eq!(cache.len(), 2);
        // Third 4-byte entry pushes us to 12; oldest ("a") is evicted.
        cache.insert("c".into(), b(4));
        assert!(cache.get("a").is_none(), "oldest entry should be evicted");
        assert!(cache.get("b").is_some());
        assert!(cache.get("c").is_some());
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.current_bytes(), 8);
    }

    #[test]
    fn get_promotes_entry_to_most_recent() {
        let cache = BlobCache::new(10);
        cache.insert("a".into(), b(4));
        cache.insert("b".into(), b(4));
        // Touch "a" — now "b" is the LRU.
        assert!(cache.get("a").is_some());
        // Insert "c" — should evict "b", not "a".
        cache.insert("c".into(), b(4));
        assert!(cache.get("a").is_some(), "promoted entry must survive");
        assert!(cache.get("b").is_none(), "untouched entry should be evicted");
        assert!(cache.get("c").is_some());
    }

    #[test]
    fn rejects_entries_larger_than_budget() {
        let cache = BlobCache::new(4);
        cache.insert("big".into(), b(5));
        assert!(cache.get("big").is_none());
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.current_bytes(), 0);
    }

    #[test]
    fn replacing_existing_key_accounts_for_delta() {
        let cache = BlobCache::new(10);
        cache.insert("a".into(), b(3));
        assert_eq!(cache.current_bytes(), 3);
        // Replace with a larger payload — accounting should track the delta,
        // not double-count.
        cache.insert("a".into(), b(7));
        assert_eq!(cache.current_bytes(), 7);
        assert_eq!(cache.len(), 1);
        // And shrink — also tracked.
        cache.insert("a".into(), b(2));
        assert_eq!(cache.current_bytes(), 2);
        assert_eq!(cache.len(), 1);
    }
}
