//! Token bucket rate limiter — per-app requests/second limiting.
//!
//! Each app gets a bucket that refills at `rate` tokens/second,
//! with a maximum burst capacity. A request consumes one token.
//! If no tokens available, the request is rejected (429).
//!
//! Implementation packs tokens (32-bit fixed-point x1000) and
//! last_refill (32-bit epoch seconds) into a single AtomicU64.
//! Uses compare_exchange (strong) CAS loop for atomic refill+consume.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

fn pack(tokens: u64, timestamp: u32) -> u64 {
    (tokens << 32) | timestamp as u64
}

fn unpack(state: u64) -> (u64, u32) {
    (state >> 32, state as u32)
}

fn now_secs() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as u32
}

/// Token bucket with packed atomic state — no locks, no races.
/// Upper 32 bits: tokens x 1000 (fixed-point for sub-token precision)
/// Lower 32 bits: last_refill timestamp (epoch seconds)
pub struct TokenBucket {
    state: AtomicU64,
    capacity: u64,    // max tokens x 1000
    refill_rate: u64, // tokens per second x 1000
}

impl TokenBucket {
    pub fn new(rate_per_second: u32, burst: u32) -> Self {
        let capacity = burst as u64 * 1000;
        Self {
            state: AtomicU64::new(pack(capacity, now_secs())),
            capacity,
            refill_rate: rate_per_second as u64 * 1000,
        }
    }

    /// Try to consume one token. Returns true if allowed.
    pub fn try_acquire(&self) -> bool {
        loop {
            let state = self.state.load(Ordering::Acquire);
            let (tokens, last_refill) = unpack(state);
            let now = now_secs();
            let elapsed = now.saturating_sub(last_refill) as u64;
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
pub struct RateLimiter {
    buckets: Mutex<HashMap<String, TokenBucket>>,
    default_rate: u32,
    default_burst: u32,
}

impl RateLimiter {
    /// Create a new rate limiter with default rate/burst for unknown apps.
    pub fn new(default_rate: u32, default_burst: u32) -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
            default_rate,
            default_burst,
        }
    }

    /// Try to allow a request for the given app.
    /// Returns `true` if allowed, `false` if rate-limited.
    pub fn check(&self, app_id: &str) -> bool {
        let mut buckets = self.buckets.lock().unwrap();
        let bucket = buckets
            .entry(app_id.to_string())
            .or_insert_with(|| TokenBucket::new(self.default_rate, self.default_burst));
        bucket.try_acquire()
    }

    /// Set a custom rate for a specific app.
    pub fn set_rate(&self, app_id: &str, rate: u32, burst: u32) {
        let mut buckets = self.buckets.lock().unwrap();
        buckets.insert(app_id.to_string(), TokenBucket::new(rate, burst));
    }

    /// Remove an app's bucket (e.g., on eviction).
    pub fn remove(&self, app_id: &str) {
        let mut buckets = self.buckets.lock().unwrap();
        buckets.remove(app_id);
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
        let ts = 1711843200u32;
        let packed = pack(tokens, ts);
        let (t, s) = unpack(packed);
        assert_eq!(t, tokens);
        assert_eq!(s, ts);
    }
}
