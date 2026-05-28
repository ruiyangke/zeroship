//! Password-reset token primitive.
//!
//! Per proposal §8.3 (Phase 5):
//!
//! - **Issue**: generate a 32-byte CSPRNG random token, store its SHA-256
//!   in `auth.magic_links` keyed by `(email, purpose='reset')`. Returns
//!   the raw token to the caller, which embeds it in the `/reset?token=`
//!   email link.
//!
//! - **Redeem**: SHA-256 the raw token, atomically `UPDATE … RETURNING`
//!   the row by `(token_hash, purpose='reset')` with the predicates
//!   `consumed_at IS NULL` AND `expires_at > NOW()`. Single-use is
//!   enforced at the database layer.
//!
//! - **TTL**: 60 minutes. Longer than a magic-link's 15-minute window
//!   (resetting a password is a deliberate flow the user may pick back
//!   up after a context switch) but far shorter than the 24-hour
//!   verification window (a leaked reset link grants account takeover,
//!   not just verification).
//!
//! - **One-active-per-(email,reset)**: at issue time, all unconsumed
//!   rows for the same email AND `purpose='reset'` are pre-emptively
//!   marked `consumed_at = NOW()`. Concurrent reset requests for one
//!   address would otherwise let an attacker reuse an earlier token
//!   even after the user has clicked a newer one. The scope is
//!   `(email, 'reset')` not just `email` so that reset issuance doesn't
//!   invalidate a pending magic-link login on the same address (the
//!   magic-link primitive enforces its own per-email superseding for
//!   `'login'` rows).
//!
//! Reuses [`auth.magic_links`] with `purpose='reset'` rather than
//! introducing yet another single-use-token table — the shape is
//! identical (`token_hash`, email, purpose, expiry, `consumed_at`). The
//! `csrf_nonce` column is required by the table schema but unused for
//! reset tokens (no cross-device flow); we write a sentinel placeholder.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use compio_postgres::Client;
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::error::{AuthError, Result};

/// Lifetime of a reset token from issue to expiry.
pub const TTL_MINUTES: i64 = 60;

/// Number of CSPRNG bytes in the raw token. 256 bits.
const TOKEN_LEN_BYTES: usize = 32;

/// Distinguishing `purpose` written into `auth.magic_links.purpose`.
const PURPOSE: &str = "reset";

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
    pub email: String,
}

/// Issue a fresh password-reset token for `email`.
///
/// Side effects:
///
/// 1. All previous unconsumed reset rows for `email` are marked
///    `consumed_at = NOW()` (one-active-per-email-per-reset invariant).
/// 2. A fresh row is inserted with `expires_at = NOW() + 60 min` and
///    `purpose = 'reset'`.
///
/// # Errors
///
/// Returns [`AuthError::Db`] on PG failure.
pub async fn issue(db: &Client, email: &str) -> Result<IssuedToken> {
    // 1. Generate the token.
    let mut token_bytes = [0u8; TOKEN_LEN_BYTES];
    rand::thread_rng().fill_bytes(&mut token_bytes);
    let raw = URL_SAFE_NO_PAD.encode(token_bytes);
    let token_hash = sha256(&raw);

    // The table schema requires csrf_nonce NOT NULL; reset tokens
    // don't use it (no cross-device fork). Write a deterministic
    // sentinel so the column is satisfied without leaking nonce
    // entropy from elsewhere.
    let csrf_nonce_sentinel = "reset-no-csrf-nonce";

    db.execute("BEGIN", &[])
        .await
        .map_err(|e| AuthError::Db(format!("password_reset issue begin: {e}")))?;

    let issued = async {
        db.execute(
            "SELECT pg_advisory_xact_lock(hashtext(lower($1::text))::bigint)",
            &[&email],
        )
        .await
        .map_err(|e| AuthError::Db(format!("password_reset issue advisory lock: {e}")))?;

        // 2. Invalidate any previously unconsumed reset tokens for this
        //    email. Scoped to `purpose = 'reset'` so a pending magic-link
        //    login on the same address is left alone.
        db.execute(
            "UPDATE auth.magic_links SET consumed_at = NOW() \
             WHERE email = $1::citext AND purpose = $2 AND consumed_at IS NULL",
            &[&email, &PURPOSE],
        )
        .await
        .map_err(|e| AuthError::Db(format!("password_reset supersede previous: {e}")))?;

        // 3. Insert the new row.
        db.execute(
            "INSERT INTO auth.magic_links \
                (token_hash, email, csrf_nonce, purpose, expires_at) \
             VALUES ($1, $2::citext, $3, $4, NOW() + ($5::text || ' minutes')::interval)",
            &[
                &token_hash.as_slice(),
                &email,
                &csrf_nonce_sentinel,
                &PURPOSE,
                &TTL_MINUTES.to_string(),
            ],
        )
        .await
        .map_err(|e| AuthError::Db(format!("password_reset insert: {e}")))?;

        Ok(())
    }
    .await;

    if let Err(e) = issued {
        let _ = db.execute("ROLLBACK", &[]).await;
        return Err(e);
    }

    db.execute("COMMIT", &[])
        .await
        .map_err(|e| AuthError::Db(format!("password_reset issue commit: {e}")))?;

    Ok(IssuedToken { raw })
}

/// Atomically redeem a password-reset token. Returns `Ok(Some(_))` on a
/// successful one-shot consume, `Ok(None)` if the token doesn't match
/// any unconsumed, unexpired reset row.
///
/// Implementation: single `UPDATE … RETURNING` keyed by
/// `(token_hash, purpose)`. The purpose predicate prevents a magic-link
/// `'login'` token from being repurposed to reset a password.
///
/// # Errors
///
/// Returns [`AuthError::Db`] on PG failure.
pub async fn redeem(db: &Client, raw_token: &str) -> Result<Option<RedeemedToken>> {
    let token_hash = sha256(raw_token);
    let rows = db
        .query(
            "UPDATE auth.magic_links SET consumed_at = NOW() \
             WHERE token_hash = $1 \
               AND purpose = $2 \
               AND consumed_at IS NULL \
               AND expires_at > NOW() \
             RETURNING email::text",
            &[&token_hash.as_slice(), &PURPOSE],
        )
        .await
        .map_err(|e| AuthError::Db(format!("password_reset redeem: {e}")))?;
    Ok(rows.first().map(|r| RedeemedToken {
        email: r.get("email"),
    }))
}

fn sha256(s: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().into()
}
