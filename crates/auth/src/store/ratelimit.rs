//! Rate-limit bucket persistence in `auth.rate_limits`.
//!
//! One row per bucket key. State is `(tokens, updated_at)`. Atomic
//! upsert via `INSERT ... ON CONFLICT DO UPDATE` so concurrent requests
//! don't race.
//!
//! The `tokens` column is declared `REAL` (PG f32) in the migration; we
//! cast to/from `DOUBLE PRECISION` at the SQL boundary so callers see
//! `f64` arithmetic, matching the leaky-bucket math in `ratelimit.rs`.

use compio_postgres::Client;

use crate::error::{AuthError, Result};

#[derive(Debug, Clone)]
pub struct BucketState {
    pub tokens: f64,
    pub updated_at_micros: i64,
}

/// Fetch (or initialise) a bucket. Returns the current state. If the row
/// doesn't exist, returns `(capacity, now)` — a fresh full bucket.
///
/// # Errors
///
/// Returns `AuthError::Db` if the PG read fails, or `AuthError::Internal`
/// if the system clock is before the UNIX epoch.
pub async fn fetch_or_init(conn: &Client, key: &str, capacity: f64) -> Result<BucketState> {
    let rows = conn
        .query(
            "SELECT tokens::DOUBLE PRECISION AS tokens, \
                    EXTRACT(EPOCH FROM updated_at)::DOUBLE PRECISION AS updated_secs \
             FROM auth.rate_limits WHERE bucket_key = $1",
            &[&key],
        )
        .await
        .map_err(|e| AuthError::Db(format!("ratelimit fetch {key}: {e}")))?;

    if let Some(row) = rows.first() {
        let tokens: f64 = row.get("tokens");
        let updated: f64 = row.get("updated_secs");
        return Ok(BucketState {
            tokens,
            #[allow(clippy::cast_possible_truncation)]
            updated_at_micros: (updated * 1_000_000.0) as i64,
        });
    }

    // Fresh full bucket; epoch in microseconds.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let now_micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| AuthError::Internal(format!("sys time: {e}")))?
        .as_micros() as i64;

    Ok(BucketState {
        tokens: capacity,
        updated_at_micros: now_micros,
    })
}

/// Atomically write the bucket state back.
///
/// # Errors
///
/// Returns `AuthError::Db` if the PG write fails.
pub async fn upsert(conn: &Client, key: &str, state: &BucketState) -> Result<()> {
    #[allow(clippy::cast_possible_truncation)]
    let tokens_f32 = state.tokens as f32;
    #[allow(clippy::cast_precision_loss)]
    let secs = (state.updated_at_micros as f64) / 1_000_000.0;
    conn.execute(
        "INSERT INTO auth.rate_limits (bucket_key, tokens, updated_at) \
         VALUES ($1, $2, TO_TIMESTAMP($3)) \
         ON CONFLICT (bucket_key) DO UPDATE \
         SET tokens = EXCLUDED.tokens, updated_at = EXCLUDED.updated_at",
        &[&key, &tokens_f32, &secs],
    )
    .await
    .map_err(|e| AuthError::Db(format!("ratelimit upsert {key}: {e}")))?;
    Ok(())
}
