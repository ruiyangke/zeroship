//! Email-verification token primitive.
//!
//! Per proposal §8.3 (Phase 5):
//!
//! - **Issue**: generate a 32-byte CSPRNG random token, store its SHA-256
//!   in `zeroship.email_verifications` keyed to `(user_id, email)`. Return
//!   the raw token to the caller (embedded in the `/verify?token=` link).
//!
//! - **Redeem**: SHA-256 the raw token, atomically mark the user verified
//!   and consume the token in one statement. Single-use is enforced at
//!   the database layer.
//!
//! - **TTL**: 24 hours. Longer than the magic-link's 15-minute window —
//!   verification is a low-frequency, one-shot operation users may not
//!   complete in the same session.
//!
//! - **One-active-per-user**: at issue time, all unconsumed rows for the
//!   same `user_id` are pre-emptively marked `consumed_at = NOW()`. A
//!   fresh request supersedes any prior outstanding token.
//!
//! Distinct from [`crate::identity::magic_link`] (different TTL, different
//! purpose, distinct `zeroship.email_verifications` table). The schema for the
//! table is owned by zeroship-migrate (`db/migrations`, applied by the compose
//! `migrate` service / `ops/db-migrate.sh`).

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use compio_postgres::Client;
use rand::RngCore;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::{AuthError, Result};

/// Lifetime of a verification token from issue to expiry.
pub const TTL_HOURS: i64 = 24;

/// Number of CSPRNG bytes in the raw token. 256 bits.
const TOKEN_LEN_BYTES: usize = 32;

/// Result of [`issue`] — the raw token the HTTP layer embeds in the
/// outgoing email link.
#[derive(Debug, Clone)]
pub struct IssuedToken {
    /// Raw token (base64url, no padding). Embedded in the email link's
    /// `?token=` parameter. Never logged.
    pub raw: String,
}

/// Result of a successful [`redeem`] — the row's identifying fields.
#[derive(Debug, Clone)]
pub struct RedeemedToken {
    pub user_id: Uuid,
    pub email: String,
}

/// Issue a fresh verification token for `user_id` + `email`.
///
/// Side effects:
///
/// 1. All previous unconsumed rows for `user_id` are marked
///    `consumed_at = NOW()` (one-active-per-user invariant).
/// 2. A fresh row is inserted with `expires_at = NOW() + 24h`.
///
/// # Errors
///
/// [`AuthError::Db`] on PG failure.
pub async fn issue(db: &Client, user_id: Uuid, email: &str) -> Result<IssuedToken> {
    // 1. Generate token.
    let mut token_bytes = [0u8; TOKEN_LEN_BYTES];
    rand::thread_rng().fill_bytes(&mut token_bytes);
    let raw = URL_SAFE_NO_PAD.encode(token_bytes);
    let token_hash = sha256(&raw);

    db.execute("BEGIN", &[])
        .await
        .map_err(|e| AuthError::Db(format!("verification issue begin: {e}")))?;

    let issued = async {
        db.execute(
            "SELECT pg_advisory_xact_lock(hashtext(lower($1::text))::bigint)",
            &[&email],
        )
        .await
        .map_err(|e| AuthError::Db(format!("verification issue advisory lock: {e}")))?;

        // 2. Invalidate any previously unconsumed tokens for this user
        //    so only the most recent token can be redeemed.
        db.execute(
            "UPDATE zeroship.email_verifications SET consumed_at = NOW() \
             WHERE user_id = $1 AND consumed_at IS NULL",
            &[&user_id],
        )
        .await
        .map_err(|e| AuthError::Db(format!("verification supersede previous: {e}")))?;

        // 3. Insert the new row. `email` is CITEXT — cast at the bind site.
        db.execute(
            "INSERT INTO zeroship.email_verifications \
                (token_hash, user_id, email, expires_at) \
             VALUES ($1, $2, $3::citext, NOW() + ($4::text || ' hours')::interval)",
            &[
                &token_hash.as_slice(),
                &user_id,
                &email,
                &TTL_HOURS.to_string(),
            ],
        )
        .await
        .map_err(|e| AuthError::Db(format!("verification insert: {e}")))?;

        Ok(())
    }
    .await;

    if let Err(e) = issued {
        let _ = db.execute("ROLLBACK", &[]).await;
        return Err(e);
    }

    db.execute("COMMIT", &[])
        .await
        .map_err(|e| AuthError::Db(format!("verification issue commit: {e}")))?;

    Ok(IssuedToken { raw })
}

/// Atomically redeem a verification token. Returns `Ok(Some(_))` on a
/// successful one-shot consume, `Ok(None)` if the token doesn't match
/// any unconsumed, unexpired row.
///
/// # Errors
///
/// [`AuthError::Db`] on PG failure.
pub async fn redeem(db: &Client, raw_token: &str) -> Result<Option<RedeemedToken>> {
    let token_hash = sha256(raw_token);
    let rows = db
        .query(
            "UPDATE zeroship.email_verifications SET consumed_at = NOW() \
             WHERE token_hash = $1 \
               AND consumed_at IS NULL \
               AND expires_at > NOW() \
             RETURNING user_id, email::text",
            &[&token_hash.as_slice()],
        )
        .await
        .map_err(|e| AuthError::Db(format!("verification redeem: {e}")))?;
    Ok(rows.first().map(|r| RedeemedToken {
        user_id: r.get("user_id"),
        email: r.get("email"),
    }))
}

/// Atomically redeem a verification token and mark the linked user
/// verified. Returns `Ok(Some(_))` on success, `Ok(None)` if the token is
/// invalid or expired.
///
/// The user update and token consume are one SQL statement. If the user
/// update fails, PostgreSQL rolls back the token consume as part of that
/// same statement.
///
/// # Errors
///
/// [`AuthError::Db`] on PG failure.
pub async fn redeem_and_mark_verified(
    db: &Client,
    raw_token: &str,
) -> Result<Option<RedeemedToken>> {
    let token_hash = sha256(raw_token);
    let rows = db
        .query(
            "WITH candidate AS ( \
                 SELECT ev.user_id, ev.email::text AS email \
                 FROM zeroship.email_verifications ev \
                 JOIN zeroship.users u ON u.id = ev.user_id \
                 WHERE ev.token_hash = $1 \
                   AND ev.consumed_at IS NULL \
                   AND ev.expires_at > NOW() \
             ), updated_user AS ( \
                 UPDATE zeroship.users u \
                 SET email_verified_at = COALESCE(u.email_verified_at, NOW()), \
                     updated_at = NOW() \
                 FROM candidate c \
                 WHERE u.id = c.user_id \
                 RETURNING u.id, c.email \
             ), consumed AS ( \
                 UPDATE zeroship.email_verifications ev \
                 SET consumed_at = NOW() \
                 FROM updated_user u \
                 WHERE ev.token_hash = $1 \
                   AND ev.user_id = u.id \
                   AND ev.consumed_at IS NULL \
                 RETURNING u.id AS user_id, u.email AS email \
             ) \
             SELECT user_id, email FROM consumed",
            &[&token_hash.as_slice()],
        )
        .await
        .map_err(|e| AuthError::Db(format!("verification redeem and mark verified: {e}")))?;
    Ok(rows.first().map(|r| RedeemedToken {
        user_id: r.get("user_id"),
        email: r.get("email"),
    }))
}

fn sha256(s: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().into()
}
