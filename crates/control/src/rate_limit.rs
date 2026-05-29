//! Per-IP token-bucket quota helpers for the control plane.
//!
//! HTTP handlers consume from the DB-backed `auth.rate_limits` table via
//! `http_util`, so buckets are shared across control-plane instances.
//! The in-memory implementation below stays as a small local primitive
//! for unit tests and non-HTTP callers that explicitly want process-local
//! accounting.
//!
//! Buckets are evicted lazily on access if untouched for `IDLE_TTL`,
//! so the map can't grow unbounded under a churning attacker.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Idle bucket TTL — buckets older than this are dropped on next access.
const IDLE_TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy)]
pub struct Quota {
    /// Maximum burst the bucket can hold.
    pub capacity: f64,
    /// Refill rate in tokens per second.
    pub refill_per_sec: f64,
}

impl Quota {
    pub const fn per_minute(burst: u32, per_min: u32) -> Self {
        Self {
            capacity: burst as f64,
            refill_per_sec: per_min as f64 / 60.0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    last_refill: Instant,
    last_access: Instant,
}

#[allow(missing_debug_implementations)]
pub struct RateLimiter {
    quota: Quota,
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
}

impl RateLimiter {
    pub fn new(quota: Quota) -> Self {
        Self {
            quota,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn quota(&self) -> Quota {
        self.quota
    }

    fn lock_buckets(&self) -> MutexGuard<'_, HashMap<IpAddr, Bucket>> {
        match self.buckets.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Try to consume one token for `ip`. Returns true if allowed,
    /// false if the bucket was empty.
    pub fn check(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut buckets = self.lock_buckets();

        // Lazy GC: evict idle buckets so the map can't grow unbounded.
        // O(N) per call but N is bounded by the number of distinct
        // recently-active IPs — small in practice.
        buckets.retain(|_, b| now.duration_since(b.last_access) < IDLE_TTL);

        let bucket = buckets.entry(ip).or_insert_with(|| Bucket {
            tokens: self.quota.capacity,
            last_refill: now,
            last_access: now,
        });

        // Refill since last access.
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.quota.refill_per_sec)
            .min(self.quota.capacity);
        bucket.last_refill = now;
        bucket.last_access = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Test/inspection helper — current token count for `ip`, or
    /// `capacity` if the IP has never been seen.
    #[cfg(test)]
    pub fn tokens_for(&self, ip: IpAddr) -> f64 {
        let buckets = self.lock_buckets();
        buckets.get(&ip).map(|b| b.tokens).unwrap_or(self.quota.capacity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr { s.parse().unwrap() }

    #[test]
    fn per_minute_quota_math() {
        let q = Quota::per_minute(20, 60);
        assert_eq!(q.capacity, 20.0);
        assert_eq!(q.refill_per_sec, 1.0);
    }

    #[test]
    fn first_calls_allowed_up_to_burst() {
        let rl = RateLimiter::new(Quota::per_minute(5, 60));
        let addr = ip("203.0.113.1");
        for _ in 0..5 {
            assert!(rl.check(addr), "first {} calls within burst should pass", 5);
        }
        assert!(!rl.check(addr), "6th call exceeds burst");
    }

    #[test]
    fn refill_restores_tokens_over_time() {
        let rl = RateLimiter::new(Quota::per_minute(2, 600)); // 10/sec
        let addr = ip("203.0.113.2");
        assert!(rl.check(addr));
        assert!(rl.check(addr));
        assert!(!rl.check(addr));
        // Wait long enough to refill at least 1 token (10 tokens/sec → 100ms).
        std::thread::sleep(Duration::from_millis(150));
        assert!(rl.check(addr), "refilled bucket should allow next call");
    }

    #[test]
    fn distinct_ips_get_independent_buckets() {
        let rl = RateLimiter::new(Quota::per_minute(1, 1));
        let a = ip("203.0.113.10");
        let b = ip("203.0.113.11");
        assert!(rl.check(a));
        assert!(!rl.check(a)); // a's bucket empty
        assert!(rl.check(b));  // b's bucket independent
    }

    #[test]
    fn capacity_cap_holds_during_long_idle() {
        let rl = RateLimiter::new(Quota::per_minute(3, 600));
        let addr = ip("203.0.113.3");
        rl.check(addr); // consume 1; bucket goes to 2.0
        std::thread::sleep(Duration::from_secs(1));
        // Refill over 1 second at 10/sec would add 10 tokens, but
        // capacity caps at 3. Next 3 calls succeed; 4th fails.
        assert!(rl.check(addr));
        assert!(rl.check(addr));
        assert!(rl.check(addr));
        assert!(!rl.check(addr));
    }

    #[test]
    fn check_recovers_after_bucket_lock_poison() {
        let rl = RateLimiter::new(Quota::per_minute(1, 60));
        let poisoned = std::panic::catch_unwind(|| {
            let _guard = rl.buckets.lock().unwrap();
            panic!("poison rate limiter");
        });
        assert!(poisoned.is_err());

        assert!(rl.check(ip("203.0.113.4")));
        assert_eq!(rl.tokens_for(ip("203.0.113.4")), 0.0);
    }
}
