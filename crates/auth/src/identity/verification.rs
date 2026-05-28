//! Email-verification token primitive.
//!
//! Per proposal §8.3 (Phase 5):
//!
//! - **Issue**: generate a 32-byte CSPRNG random token, store its SHA-256
//!   in `auth.email_verifications` keyed to `(user_id, email)`. Return
//!   the raw token to the caller (embedded in the `/verify?token=` link).
//!
//! - **Redeem**: SHA-256 the raw token, atomically `UPDATE … RETURNING`
//!   keyed by `token_hash` with the predicates `consumed_at IS NULL` AND
//!   `expires_at > NOW()`. Single-use is enforced at the database layer.
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
//! purpose, distinct `auth.email_verifications` table). The schema for the
//! table is created in `store::migrations` (item 5.5).

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
    // 1. Invalidate any previously unconsumed tokens for this user so
    //    only the most recent token can be redeemed.
    db.execute(
        "UPDATE auth.email_verifications SET consumed_at = NOW() \
         WHERE user_id = $1 AND consumed_at IS NULL",
        &[&user_id],
    )
    .await
    .map_err(|e| AuthError::Db(format!("verification supersede previous: {e}")))?;

    // 2. Generate token.
    let mut token_bytes = [0u8; TOKEN_LEN_BYTES];
    rand::thread_rng().fill_bytes(&mut token_bytes);
    let raw = URL_SAFE_NO_PAD.encode(token_bytes);
    let token_hash = sha256(&raw);

    // 3. Insert the new row. `email` is CITEXT — cast at the bind site.
    db.execute(
        "INSERT INTO auth.email_verifications \
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
            "UPDATE auth.email_verifications SET consumed_at = NOW() \
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

fn sha256(s: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().into()
}
