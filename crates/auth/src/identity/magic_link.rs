//! Magic-link token primitive.
//!
//! Per proposal §8.3 (Phase 5):
//!
//! - **Issue**: generate a 32-byte CSPRNG random token + a 16-byte CSRF
//!   nonce. Store the SHA-256 of the token in `auth.magic_links`. Return
//!   the raw token + the CSRF nonce to the caller (which embeds the raw
//!   token in the email body and sets the nonce in a cookie at the
//!   requesting device).
//!
//! - **Redeem**: SHA-256 the raw token, atomically `UPDATE … RETURNING`
//!   the row by `token_hash` with the predicates `consumed_at IS NULL`
//!   AND `expires_at > NOW()`. Single-use is enforced at the database
//!   layer — the UPDATE only matches once.
//!
//! - **TTL**: 15 minutes. The shorter window limits the attack surface
//!   of a leaked email link far more than the user-experience cost of
//!   re-typing an email if the link expired.
//!
//! - **One-active-per-email**: at issue time, all unconsumed rows with
//!   the same email are pre-emptively marked `consumed_at = NOW()`.
//!   Multiple in-flight magic-link issues for one address would
//!   otherwise let an attacker reuse an earlier (less-defended) token
//!   even after the user has clicked a newer one. Keep the active set
//!   at most one.

use compio_postgres::Client;
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::error::{AuthError, Result};

/// Lifetime of a magic-link token from issue to expiry.
pub const TTL_MINUTES: i64 = 15;

/// Number of CSPRNG bytes in the raw token. 256 bits.
const TOKEN_LEN_BYTES: usize = 32;

/// Number of CSPRNG bytes in the CSRF nonce. 128 bits — matches the
/// `__Host-zsidp_csrf` cookie's entropy.
const CSRF_NONCE_LEN_BYTES: usize = 16;

/// Result of [`issue`] — the values the HTTP layer needs to assemble the
/// email body and the requesting-device cookie.
#[derive(Debug, Clone)]
pub struct IssuedToken {
    /// Raw token (base64url, no padding). Embedded in the email link's
    /// `?token=` parameter. Never logged.
    pub raw: String,
    /// CSRF nonce (base64url, no padding). Set as the
    /// `__Host-zsidp_magic_csrf` cookie on the requesting device.
    pub csrf_nonce: String,
}

/// Result of a successful [`redeem`] — the row's identifying fields.
#[derive(Debug, Clone)]
pub struct RedeemedToken {
    pub email: String,
    pub csrf_nonce: String,
    pub purpose: String,
}

/// Issue a fresh magic-link token for `email` + `purpose`.
///
/// Side effects:
///
/// 1. All previous unconsumed rows for the same `email` are marked
///    `consumed_at = NOW()` (one-active-per-email invariant).
/// 2. A fresh row is inserted with `expires_at = NOW() + 15 min`.
///
/// Both writes happen in the same DB session; PG's per-statement
/// atomicity is enough — there's no cross-row constraint we need a
/// transaction to enforce.
///
/// # Errors
///
/// [`AuthError::Db`] on PG failure.
pub async fn issue(conn: &Client, email: &str, purpose: &str) -> Result<IssuedToken> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

    // 1. Generate token + nonce.
    let mut token_bytes = [0u8; TOKEN_LEN_BYTES];
    let mut nonce_bytes = [0u8; CSRF_NONCE_LEN_BYTES];
    rand::thread_rng().fill_bytes(&mut token_bytes);
    rand::thread_rng().fill_bytes(&mut nonce_bytes);

    let raw = URL_SAFE_NO_PAD.encode(token_bytes);
    let csrf_nonce = URL_SAFE_NO_PAD.encode(nonce_bytes);
    let token_hash = sha256(&raw);

    // 2. Invalidate any previously unconsumed tokens for this email so
    //    only the most recent token can be redeemed.
    conn.execute(
        "UPDATE auth.magic_links SET consumed_at = NOW() \
         WHERE email = $1::citext AND consumed_at IS NULL",
        &[&email],
    )
    .await
    .map_err(|e| AuthError::Db(format!("magic_link supersede previous: {e}")))?;

    // 3. Insert the new row.
    conn.execute(
        "INSERT INTO auth.magic_links \
            (token_hash, email, csrf_nonce, purpose, expires_at) \
         VALUES ($1, $2::citext, $3, $4, NOW() + ($5::text || ' minutes')::interval)",
        &[
            &token_hash.as_slice(),
            &email,
            &csrf_nonce,
            &purpose,
            &TTL_MINUTES.to_string(),
        ],
    )
    .await
    .map_err(|e| AuthError::Db(format!("magic_link insert: {e}")))?;

    Ok(IssuedToken { raw, csrf_nonce })
}

/// Atomically redeem a magic-link token. Returns `Ok(Some(_))` on a
/// successful one-shot consume, `Ok(None)` if the token doesn't match
/// any unconsumed, unexpired row.
///
/// Implementation: single `UPDATE … RETURNING` keyed by
/// `token_hash = SHA-256(raw_token)`. `PostgreSQL` guarantees only one
/// concurrent caller observes the row in the unconsumed state — race-
/// free single-use.
///
/// # Errors
///
/// [`AuthError::Db`] on PG failure.
pub async fn redeem(conn: &Client, raw_token: &str) -> Result<Option<RedeemedToken>> {
    let token_hash = sha256(raw_token);
    let rows = conn
        .query(
            "UPDATE auth.magic_links SET consumed_at = NOW() \
             WHERE token_hash = $1 \
               AND consumed_at IS NULL \
               AND expires_at > NOW() \
             RETURNING email::text, csrf_nonce, purpose",
            &[&token_hash.as_slice()],
        )
        .await
        .map_err(|e| AuthError::Db(format!("magic_link redeem: {e}")))?;
    Ok(rows.first().map(|row| RedeemedToken {
        email: row.get("email"),
        csrf_nonce: row.get("csrf_nonce"),
        purpose: row.get("purpose"),
    }))
}

fn sha256(s: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().into()
}
