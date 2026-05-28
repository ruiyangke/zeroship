//! Rate-limit bucket persistence in `auth.rate_limits`.
//!
//! One row per bucket key. State is `(tokens, updated_at)`. Consumption is a
//! single `INSERT ... ON CONFLICT DO UPDATE` statement so concurrent requests
//! serialize on the row instead of racing a read-modify-write window.
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

#[derive(Debug, Clone)]
pub struct ConsumeResult {
    pub state: BucketState,
    pub consumed: bool,
}

/// Atomically refill the bucket and attempt to consume one token.
///
/// # Errors
///
/// Returns `AuthError::Db` if the PG statement fails.
pub async fn consume(
    conn: &Client,
    key: &str,
    capacity: f64,
    refill_per_sec: f64,
) -> Result<ConsumeResult> {
    let rows = conn
        .query(
            "WITH input AS ( \
                 SELECT $1::TEXT AS bucket_key, \
                        $2::DOUBLE PRECISION AS capacity, \
                        $3::DOUBLE PRECISION AS refill_per_sec, \
                        NOW() AS now_at \
             ), upserted AS ( \
                 INSERT INTO auth.rate_limits (bucket_key, tokens, updated_at) \
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
                             auth.rate_limits.tokens::DOUBLE PRECISION \
                                 + GREATEST( \
                                     0.0, \
                                     EXTRACT(EPOCH FROM (EXCLUDED.updated_at - auth.rate_limits.updated_at)) \
                                         * $3::DOUBLE PRECISION \
                                 ) \
                         ) - 1.0 \
                     )::REAL, \
                     updated_at = EXCLUDED.updated_at \
                 WHERE LEAST( \
                         $2::DOUBLE PRECISION, \
                         auth.rate_limits.tokens::DOUBLE PRECISION \
                             + GREATEST( \
                                 0.0, \
                                 EXTRACT(EPOCH FROM (EXCLUDED.updated_at - auth.rate_limits.updated_at)) \
                                     * $3::DOUBLE PRECISION \
                             ) \
                     ) >= 1.0 \
                 RETURNING tokens::DOUBLE PRECISION AS tokens, \
                           EXTRACT(EPOCH FROM updated_at)::DOUBLE PRECISION AS updated_secs, \
                           ($2::DOUBLE PRECISION >= 1.0) AS consumed \
             ) \
             SELECT tokens, updated_secs, consumed FROM upserted \
             UNION ALL \
             SELECT LEAST( \
                        input.capacity, \
                        auth.rate_limits.tokens::DOUBLE PRECISION \
                            + GREATEST( \
                                0.0, \
                                EXTRACT(EPOCH FROM (input.now_at - auth.rate_limits.updated_at)) \
                                    * input.refill_per_sec \
                            ) \
                    ) AS tokens, \
                    EXTRACT(EPOCH FROM input.now_at)::DOUBLE PRECISION AS updated_secs, \
                    FALSE AS consumed \
             FROM input \
             JOIN auth.rate_limits ON auth.rate_limits.bucket_key = input.bucket_key \
             WHERE NOT EXISTS (SELECT 1 FROM upserted)",
            &[&key, &capacity, &refill_per_sec],
        )
        .await
        .map_err(|e| AuthError::Db(format!("ratelimit consume {key}: {e}")))?;

    let row = rows
        .first()
        .ok_or_else(|| AuthError::Internal(format!("ratelimit consume returned no row: {key}")))?;
    let tokens: f64 = row.get("tokens");
    let updated: f64 = row.get("updated_secs");
    let consumed: bool = row.get("consumed");

    Ok(ConsumeResult {
        state: BucketState {
            tokens,
            #[allow(clippy::cast_possible_truncation)]
            updated_at_micros: (updated * 1_000_000.0) as i64,
        },
        consumed,
    })
}
