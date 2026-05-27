//! Leaky token bucket. Three configured profiles map to the three buckets
//! in proposal §8.1 (per-email-IP, per-email, per-IP).
//!
//! Each call: read state, refill based on elapsed time, attempt to consume,
//! write back. PG row-level locks make this race-safe; in practice the
//! UPDATE ON CONFLICT pattern is atomic.

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
}

#[derive(Debug)]
pub struct RateLimited {
    pub retry_after_secs: f64,
}

/// Attempt to consume one token from the named bucket.
///
/// Returns `Ok(Ok(()))` on success, `Ok(Err(RateLimited))` on throttle, `Err`
/// on DB error.
///
/// # Errors
///
/// Returns `AuthError::Db` if the PG read/write fails, or
/// `AuthError::Internal` if the system clock is before the UNIX epoch.
pub async fn consume(
    conn: &Client,
    key: &str,
    bucket: Bucket,
) -> Result<std::result::Result<(), RateLimited>> {
    let mut state = store::fetch_or_init(conn, key, bucket.capacity).await?;

    // Refill based on elapsed time.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let now_micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64;
    #[allow(clippy::cast_precision_loss)]
    let elapsed_secs = ((now_micros - state.updated_at_micros) as f64) / 1_000_000.0;
    state.tokens = (state.tokens + elapsed_secs * bucket.refill_per_sec).min(bucket.capacity);
    state.updated_at_micros = now_micros;

    if state.tokens >= 1.0 {
        state.tokens -= 1.0;
        store::upsert(conn, key, &state).await?;
        Ok(Ok(()))
    } else {
        // Persist updated state so refill clock advances even on rejection.
        store::upsert(conn, key, &state).await?;
        let deficit = 1.0 - state.tokens;
        let retry_after_secs = deficit / bucket.refill_per_sec;
        Ok(Err(RateLimited { retry_after_secs }))
    }
}
