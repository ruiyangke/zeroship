//! Shared token-bucket primitives for services that authenticate creators.
//!
//! The database-backed consume path is the production primitive. One
//! `zeroship.rate_limits` row represents one bucket, so replicas settle a
//! concurrent burst through PostgreSQL rather than through per-process state.

use std::collections::HashMap;
use std::error::Error as StdError;
use std::fmt;
use std::net::IpAddr;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use compio_postgres::{Client, GenericClient};

/// Idle process-local bucket TTL.
const IDLE_TTL: Duration = Duration::from_secs(300);

/// Capacity and refill rate for a token bucket.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quota {
    /// Maximum tokens the bucket holds.
    pub capacity: f64,
    /// Tokens refilled per second.
    pub refill_per_sec: f64,
}

impl Quota {
    /// Construct a quota from a burst capacity and steady requests per minute.
    #[must_use]
    pub const fn per_minute(burst: u32, per_minute: u32) -> Self {
        Self {
            capacity: burst as f64,
            refill_per_sec: per_minute as f64 / 60.0,
        }
    }

    /// Per-(email, ip): 5 requests / 15 minutes.
    pub const LOGIN_EIP: Self = Self {
        capacity: 5.0,
        refill_per_sec: 5.0 / 900.0,
    };
    /// Per-account: 10 requests / hour.
    pub const LOGIN_EMAIL: Self = Self {
        capacity: 10.0,
        refill_per_sec: 10.0 / 3600.0,
    };
    /// Per-IP: 60 / hour.
    pub const LOGIN_IP: Self = Self {
        capacity: 60.0,
        refill_per_sec: 60.0 / 3600.0,
    };
    /// Cross-device magic completion: 30 requests burst, refill 5/min.
    pub const MAGIC_COMPLETE: Self = Self {
        capacity: 30.0,
        refill_per_sec: 5.0 / 60.0,
    };
    /// OAuth account-link password confirmation: 4 attempts / 15 minutes.
    pub const LINK_ATTEMPT: Self = Self {
        capacity: 4.0,
        refill_per_sec: 4.0 / 900.0,
    };
    /// Signup per-IP: 5 requests / minute, capacity 10.
    pub const SIGNUP_IP: Self = Self {
        capacity: 10.0,
        refill_per_sec: 5.0 / 60.0,
    };
    /// Password reset per-email: 3 requests / hour, capacity 5.
    pub const FORGOT_EMAIL: Self = Self {
        capacity: 5.0,
        refill_per_sec: 3.0 / 3600.0,
    };
    /// Password reset per-IP: 30 requests / hour.
    pub const FORGOT_IP: Self = Self {
        capacity: 30.0,
        refill_per_sec: 30.0 / 3600.0,
    };
    /// Reset submission per-IP: 30 requests / hour.
    pub const RESET_IP: Self = Self {
        capacity: 30.0,
        refill_per_sec: 30.0 / 3600.0,
    };
    /// TOTP verification per-user: 5 attempts burst, refill 5/15min.
    pub const TOTP_VERIFY: Self = Self {
        capacity: 5.0,
        refill_per_sec: 5.0 / 900.0,
    };
}

/// A denied consume and the time until one token is available.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateLimited {
    pub retry_after_secs: f64,
}

/// Result of attempting to consume one token.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RateLimitDecision {
    Allowed,
    Throttled(RateLimited),
}

/// Raw bucket state after a consume attempt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Consumption {
    pub remaining_tokens: f64,
    pub consumed: bool,
}

/// Failure to settle a bucket in the shared store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitError(String);

impl fmt::Display for RateLimitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl StdError for RateLimitError {}

fn describe(error: &compio_postgres::Error) -> String {
    error.as_db_error().map_or_else(
        || error.to_string(),
        |db| format!("{error}: {} ({})", db.message(), db.code().code()),
    )
}

/// Atomically refill a shared bucket and attempt to consume one token.
///
/// The `INSERT ... ON CONFLICT DO UPDATE` statement serializes concurrent
/// attempts for one key on its row. Callers must use a stable, namespaced key
/// shared by every replica that enforces the same quota.
///
/// # Errors
///
/// Returns [`RateLimitError`] when PostgreSQL cannot settle the bucket or when
/// the statement violates its one-row result contract.
#[allow(clippy::future_not_send)]
pub async fn consume_state(
    conn: &(impl GenericClient + ?Sized),
    key: &str,
    quota: Quota,
) -> Result<Consumption, RateLimitError> {
    // A concurrent first insert can make the conflict UPDATE's WHERE false
    // after this statement took its snapshot. PostgreSQL then returns no row:
    // the losing statement cannot see the just-committed insert in its UNION
    // fallback. One fresh statement snapshot observes that row and classifies
    // the request as throttled instead of misreporting a store outage.
    for attempt in 0..2 {
        let rows = conn
            .query(
            "WITH input AS ( \
                 SELECT $1::TEXT AS bucket_key, \
                        $2::DOUBLE PRECISION AS capacity, \
                        $3::DOUBLE PRECISION AS refill_per_sec, \
                        NOW() AS now_at \
             ), upserted AS ( \
                 INSERT INTO zeroship.rate_limits (bucket_key, tokens, updated_at) \
                 SELECT bucket_key, \
                        CASE WHEN capacity >= 1.0 \
                             THEN (capacity - 1.0)::REAL \
                             ELSE capacity::REAL \
                        END, \
                        now_at \
                 FROM input \
                 ON CONFLICT (bucket_key) DO UPDATE \
                 SET tokens = ( \
                         LEAST( \
                             $2::DOUBLE PRECISION, \
                             zeroship.rate_limits.tokens::DOUBLE PRECISION \
                                 + GREATEST( \
                                     0.0, \
                                     EXTRACT(EPOCH FROM (EXCLUDED.updated_at - zeroship.rate_limits.updated_at)) \
                                         * $3::DOUBLE PRECISION \
                                 ) \
                         ) - 1.0 \
                     )::REAL, \
                     updated_at = EXCLUDED.updated_at \
                 WHERE LEAST( \
                         $2::DOUBLE PRECISION, \
                         zeroship.rate_limits.tokens::DOUBLE PRECISION \
                             + GREATEST( \
                                 0.0, \
                                 EXTRACT(EPOCH FROM (EXCLUDED.updated_at - zeroship.rate_limits.updated_at)) \
                                     * $3::DOUBLE PRECISION \
                             ) \
                     ) >= 1.0 \
                 RETURNING tokens::DOUBLE PRECISION AS tokens, \
                           ($2::DOUBLE PRECISION >= 1.0) AS consumed \
             ) \
             SELECT tokens, consumed FROM upserted \
             UNION ALL \
             SELECT LEAST( \
                        input.capacity, \
                        zeroship.rate_limits.tokens::DOUBLE PRECISION \
                            + GREATEST( \
                                0.0, \
                                EXTRACT(EPOCH FROM (input.now_at - zeroship.rate_limits.updated_at)) \
                                    * input.refill_per_sec \
                            ) \
                    ) AS tokens, \
                    FALSE AS consumed \
             FROM input \
             JOIN zeroship.rate_limits ON zeroship.rate_limits.bucket_key = input.bucket_key \
             WHERE NOT EXISTS (SELECT 1 FROM upserted)",
            &[&key, &quota.capacity, &quota.refill_per_sec],
            )
            .await
            .map_err(|error| {
                RateLimitError(format!(
                    "rate-limit consume {key}: {}",
                    describe(&error)
                ))
            })?;

        if let Some(row) = rows.first() {
            return Ok(Consumption {
                remaining_tokens: row.get("tokens"),
                consumed: row.get("consumed"),
            });
        }
        if attempt == 0 {
            continue;
        }
    }

    Err(RateLimitError(format!(
        "rate-limit consume returned no row for bucket {key} after a fresh snapshot"
    )))
}

/// Consume one token and classify the shared-store result.
///
/// # Errors
///
/// Returns [`RateLimitError`] under the same conditions as [`consume_state`].
#[allow(clippy::future_not_send)]
pub async fn consume(
    conn: &Client,
    key: &str,
    quota: Quota,
) -> Result<RateLimitDecision, RateLimitError> {
    let result = consume_state(conn, key, quota).await?;
    if result.consumed {
        return Ok(RateLimitDecision::Allowed);
    }

    let deficit = (1.0 - result.remaining_tokens).max(0.0);
    let retry_after_secs = if quota.refill_per_sec > 0.0 {
        deficit / quota.refill_per_sec
    } else {
        f64::INFINITY
    };
    Ok(RateLimitDecision::Throttled(RateLimited {
        retry_after_secs,
    }))
}

#[derive(Debug, Clone, Copy)]
struct LocalBucket {
    tokens: f64,
    last_refill: Instant,
    last_access: Instant,
}

/// Process-local limiter for explicitly local accounting.
///
/// This is not a security boundary across replicas. HTTP security gates use
/// [`consume`] and the shared PostgreSQL row instead.
#[allow(missing_debug_implementations)]
pub struct RateLimiter {
    quota: Quota,
    buckets: Mutex<HashMap<IpAddr, LocalBucket>>,
}

impl RateLimiter {
    #[must_use]
    pub fn new(quota: Quota) -> Self {
        Self {
            quota,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    #[must_use]
    pub const fn quota(&self) -> Quota {
        self.quota
    }

    fn lock_buckets(&self) -> MutexGuard<'_, HashMap<IpAddr, LocalBucket>> {
        match self.buckets.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Try to consume one process-local token for `ip`.
    pub fn check(&self, ip: IpAddr) -> bool {
        self.check_at(ip, Instant::now())
    }

    fn check_at(&self, ip: IpAddr, now: Instant) -> bool {
        let mut buckets = self.lock_buckets();
        buckets.retain(|_, bucket| now.duration_since(bucket.last_access) < IDLE_TTL);

        let bucket = buckets.entry(ip).or_insert_with(|| LocalBucket {
            tokens: self.quota.capacity,
            last_refill: now,
            last_access: now,
        });
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens =
            (bucket.tokens + elapsed * self.quota.refill_per_sec).min(self.quota.capacity);
        bucket.last_refill = now;
        bucket.last_access = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    #[cfg(test)]
    fn tokens_for(&self, ip: IpAddr) -> f64 {
        let buckets = self.lock_buckets();
        buckets
            .get(&ip)
            .map(|bucket| bucket.tokens)
            .unwrap_or(self.quota.capacity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(value: &str) -> IpAddr {
        value.parse().unwrap()
    }

    #[test]
    fn per_minute_quota_math() {
        assert_eq!(
            Quota::per_minute(20, 60),
            Quota {
                capacity: 20.0,
                refill_per_sec: 1.0,
            }
        );
    }

    #[test]
    fn process_local_limiter_honors_burst_and_ip_partition() {
        let limiter = RateLimiter::new(Quota::per_minute(1, 1));
        let first = ip("203.0.113.10");
        let second = ip("203.0.113.11");
        assert!(limiter.check(first));
        assert!(!limiter.check(first));
        assert!(limiter.check(second));
    }

    #[test]
    fn process_local_limiter_refills_at_configured_rate() {
        let limiter = RateLimiter::new(Quota::per_minute(1, 60));
        let addr = ip("203.0.113.2");
        let started_at = Instant::now();

        assert!(limiter.check_at(addr, started_at));
        assert!(!limiter.check_at(addr, started_at));
        assert!(!limiter.check_at(addr, started_at + Duration::from_millis(500)));
        assert!(limiter.check_at(addr, started_at + Duration::from_secs(1)));
        assert!(!limiter.check_at(addr, started_at + Duration::from_secs(1)));
    }

    #[test]
    fn process_local_limiter_caps_tokens_after_long_idle() {
        let limiter = RateLimiter::new(Quota::per_minute(3, 60));
        let addr = ip("203.0.113.3");
        let started_at = Instant::now();
        let after_long_idle = started_at + Duration::from_secs(30);

        assert!(limiter.check_at(addr, started_at));
        for request in 1..=3 {
            assert!(
                limiter.check_at(addr, after_long_idle),
                "request {request} within the capacity should pass"
            );
        }
        assert!(
            !limiter.check_at(addr, after_long_idle),
            "an idle bucket must not bank more than its capacity"
        );
    }

    #[test]
    fn process_local_limiter_recovers_after_poison() {
        let limiter = RateLimiter::new(Quota::per_minute(1, 60));
        let poisoned = std::panic::catch_unwind(|| {
            let _guard = limiter.buckets.lock().unwrap();
            panic!("poison rate limiter");
        });
        assert!(poisoned.is_err());

        let addr = ip("203.0.113.4");
        assert!(limiter.check(addr));
        assert_eq!(limiter.tokens_for(addr), 0.0);
    }
}
