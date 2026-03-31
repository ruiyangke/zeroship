//! Token bucket rate limiter — per-app requests/second limiting.
//!
//! Implementation packs tokens (upper 32 bits, fixed-point ×1000) and
//! last_refill (lower 32 bits, epoch seconds) into a single AtomicU64.
//! Uses compare_exchange CAS loop for lock-free atomic refill+consume.
//!
//! The RateLimiter registry uses Arc<TokenBucket> per app so that
//! the Mutex is only held briefly during bucket lookup, NOT during the
//! CAS loop. This preserves the lock-free property on the hot path.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Max burst value: tokens are stored in upper 32 bits as tokens×1000.
/// u32::MAX / 1000 = 4,294,967 max burst tokens.
const MAX_BURST: u32 = u32::MAX / 1000;

fn pack(tokens: u64, timestamp: u32) -> u64 {
    debug_assert!(tokens <= u32::MAX as u64, "tokens overflow 32-bit field");
    (tokens << 32) | timestamp as u64
}

fn unpack(state: u64) -> (u64, u32) {
    (state >> 32, state as u32)
}

fn now_secs() -> u32 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    debug_assert!(secs <= u64::from(u32::MAX), "timestamp overflow: year 2106+");
    secs as u32
}

/// Token bucket with packed atomic state — lock-free, no races.
/// Upper 32 bits: tokens × 1000 (fixed-point for sub-token precision)
/// Lower 32 bits: last_refill timestamp (epoch seconds)
///
/// Max supported burst: 4,294,967 tokens (u32::MAX / 1000).
pub struct TokenBucket {
    state: AtomicU64,
    capacity: u64,     // max tokens × 1000, clamped to u32::MAX
    refill_rate: u64,  // tokens per second × 1000
}

impl TokenBucket {
    pub fn new(rate_per_second: u32, burst: u32) -> Self {
        let clamped_burst = burst.min(MAX_BURST);
        let capacity = u64::from(clamped_burst) * 1000;
        let refill_rate = u64::from(rate_per_second.min(MAX_BURST)) * 1000;
        Self {
            state: AtomicU64::new(pack(capacity, now_secs())),
            capacity,
            refill_rate,
        }
    }

    /// Try to consume one token. Returns true if allowed. Lock-free.
    pub fn try_acquire(&self) -> bool {
        loop {
            let state = self.state.load(Ordering::Acquire);
            let (tokens, last_refill) = unpack(state);
            let now = now_secs();
            let elapsed = u64::from(now.saturating_sub(last_refill));
            let refilled = (tokens + elapsed * self.refill_rate).min(self.capacity);

            if refilled < 1000 {
                return false; // less than 1 whole token
            }

            let new_state = pack(refilled - 1000, now);
            if self
                .state
                .compare_exchange(state, new_state, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return true;
            }
            // CAS failed — another thread modified state, retry
        }
    }
}

/// Rate limiter registry — one bucket per app.
///
/// Uses RwLock<HashMap<String, Arc<TokenBucket>>> so that:
/// - Bucket creation/removal takes a write lock (rare)
/// - Bucket lookup takes a read lock (common), clones the Arc, releases lock
/// - try_acquire runs OUTSIDE any lock (truly lock-free on hot path)
pub struct RateLimiter {
    buckets: RwLock<HashMap<String, Arc<TokenBucket>>>,
    default_rate: u32,
    default_burst: u32,
    /// Mutex for insert-if-absent (prevents double-creation race)
    insert_lock: Mutex<()>,
}

impl RateLimiter {
    pub fn new(default_rate: u32, default_burst: u32) -> Self {
        Self {
            buckets: RwLock::new(HashMap::new()),
            default_rate,
            default_burst,
            insert_lock: Mutex::new(()),
        }
    }

    /// Try to allow a request. Lock-free on hot path (bucket already exists).
    pub fn check(&self, app_id: &str) -> bool {
        // Fast path: read lock to get existing bucket
        {
            let buckets = self.buckets.read().unwrap();
            if let Some(bucket) = buckets.get(app_id) {
                let bucket = bucket.clone(); // Arc clone, cheap
                drop(buckets); // release read lock BEFORE CAS
                return bucket.try_acquire();
            }
        }

        // Slow path: create bucket (rare, first request per app)
        let _insert = self.insert_lock.lock().unwrap();
        // Double-check after acquiring insert lock
        {
            let buckets = self.buckets.read().unwrap();
            if let Some(bucket) = buckets.get(app_id) {
                let bucket = bucket.clone();
                drop(buckets);
                return bucket.try_acquire();
            }
        }
        let bucket = Arc::new(TokenBucket::new(self.default_rate, self.default_burst));
        let result = bucket.try_acquire();
        self.buckets.write().unwrap().insert(app_id.to_string(), bucket);
        result
    }

    /// Set a custom rate for a specific app.
    pub fn set_rate(&self, app_id: &str, rate: u32, burst: u32) {
        self.buckets.write().unwrap().insert(
            app_id.to_string(),
            Arc::new(TokenBucket::new(rate, burst)),
        );
    }

    /// Remove an app's bucket.
    pub fn remove(&self, app_id: &str) {
        self.buckets.write().unwrap().remove(app_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_within_burst() {
        let limiter = RateLimiter::new(10, 10);
        for _ in 0..10 {
            assert!(limiter.check("app1"));
        }
    }

    #[test]
    fn denies_over_burst() {
        let limiter = RateLimiter::new(10, 5);
        for _ in 0..5 {
            assert!(limiter.check("app1"));
        }
        assert!(!limiter.check("app1"));
    }

    #[test]
    fn separate_apps() {
        let limiter = RateLimiter::new(1, 1);
        assert!(limiter.check("app1"));
        assert!(!limiter.check("app1"));
        assert!(limiter.check("app2"));
    }

    #[test]
    fn pack_unpack_roundtrip() {
        let tokens = 42_000u64;
        let ts = 1_711_843_200u32;
        let packed = pack(tokens, ts);
        let (t, s) = unpack(packed);
        assert_eq!(t, tokens);
        assert_eq!(s, ts);
    }

    #[test]
    fn burst_clamped_to_max() {
        // burst > MAX_BURST should not panic or overflow
        let bucket = TokenBucket::new(10, u32::MAX);
        assert!(bucket.try_acquire()); // should work, not corrupt
    }

    #[test]
    fn capacity_within_32_bits() {
        let bucket = TokenBucket::new(MAX_BURST, MAX_BURST);
        let state = bucket.state.load(Ordering::Relaxed);
        let (tokens, _) = unpack(state);
        assert!(tokens <= u32::MAX as u64, "tokens must fit in 32 bits");
    }
}
