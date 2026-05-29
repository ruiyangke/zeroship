//! Leaky token bucket. Three configured profiles map to the three buckets
//! in proposal §8.1 (per-email-IP, per-email, per-IP).
//!
//! Each call atomically refills and consumes in one PG statement. Row-level
//! locking in the `ON CONFLICT DO UPDATE` arm serializes concurrent attempts
//! for the same bucket key.

use compio_postgres::Client;

use crate::error::Result;
use crate::store::ratelimit as store;

#[derive(Debug, Clone, Copy)]
pub struct Bucket {
    /// Maximum tokens the bucket holds.
    pub capacity: f64,
    /// Tokens refilled per second.
    pub refill_per_sec: f64,
}

impl Bucket {
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
}

#[derive(Debug)]
pub struct RateLimited {
    pub retry_after_secs: f64,
}

#[derive(Debug)]
pub enum RateLimitDecision {
    Allowed,
    Throttled(RateLimited),
}

/// Attempt to consume one token from the named bucket.
///
/// Returns `Ok(RateLimitDecision::Allowed)` on success,
/// `Ok(RateLimitDecision::Throttled(_))` on throttle, `Err` on DB error.
///
/// # Errors
///
/// Returns `AuthError::Db` if the PG statement fails.
pub async fn consume(conn: &Client, key: &str, bucket: Bucket) -> Result<RateLimitDecision> {
    let result = store::consume(conn, key, bucket.capacity, bucket.refill_per_sec).await?;

    if result.consumed {
        return Ok(RateLimitDecision::Allowed);
    }

    let deficit = (1.0 - result.state.tokens).max(0.0);
    let retry_after_secs = if bucket.refill_per_sec > 0.0 {
        deficit / bucket.refill_per_sec
    } else {
        f64::INFINITY
    };
    Ok(RateLimitDecision::Throttled(RateLimited {
        retry_after_secs,
    }))
}

/// Named wrapper for call sites that need the DB-backed atomic consume path.
pub async fn consume_or_throttle(
    conn: &Client,
    key: &str,
    bucket: Bucket,
) -> Result<RateLimitDecision> {
    consume(conn, key, bucket).await
}
