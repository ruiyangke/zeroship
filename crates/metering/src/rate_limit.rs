//! Token bucket rate limiter — per-app requests/second limiting.
//!
//! Each app gets a bucket that refills at `rate` tokens/second,
//! with a maximum burst capacity. A request consumes one token.
//! If no tokens available, the request is rejected (429).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

/// Per-app token bucket.
struct Bucket {
    tokens: f64,
    last_refill: Instant,
    rate: f64,      // tokens per second
    capacity: f64,  // max burst
}

impl Bucket {
    fn new(rate: u32, capacity: u32) -> Self {
        Self {
            tokens: capacity as f64,
            last_refill: Instant::now(),
            rate: rate as f64,
            capacity: capacity as f64,
        }
    }

    /// Try to consume one token. Returns true if allowed.
    fn try_acquire(&mut self) -> bool {
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Refill tokens based on elapsed time since last refill.
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.capacity);
        self.last_refill = now;
    }
}

/// Rate limiter registry — one bucket per app.
pub struct RateLimiter {
    buckets: Mutex<HashMap<String, Bucket>>,
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
            .or_insert_with(|| Bucket::new(self.default_rate, self.default_burst));
        bucket.try_acquire()
    }

    /// Set a custom rate for a specific app.
    pub fn set_rate(&self, app_id: &str, rate: u32, burst: u32) {
        let mut buckets = self.buckets.lock().unwrap();
        buckets.insert(app_id.to_string(), Bucket::new(rate, burst));
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
    fn allows_within_rate() {
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
        // Burst exhausted
        assert!(!limiter.check("app1"));
    }

    #[test]
    fn separate_apps() {
        let limiter = RateLimiter::new(1, 1);
        assert!(limiter.check("app1"));
        assert!(!limiter.check("app1")); // exhausted
        assert!(limiter.check("app2"));  // separate bucket
    }
}
