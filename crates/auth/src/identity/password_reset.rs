//! Password-reset token primitive.
//!
//! Per proposal §8.3 (Phase 5):
//!
//! - **Issue**: generate a 32-byte CSPRNG random token, store its SHA-256
//!   in `zeroship.magic_links` keyed by `(email, purpose='reset')`. Returns
//!   the raw token to the caller, which embeds it in the `/reset?token=`
//!   email link.
//!
//! - **Complete**: SHA-256 the raw token, atomically update the user's
//!   password hash and consume the reset row by `(token_hash,
//!   purpose='reset')` with the predicates `consumed_at IS NULL` AND
//!   `expires_at > NOW()`. Single-use is enforced at the database layer.
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
//! Reuses [`zeroship.magic_links`] with `purpose='reset'` rather than
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

/// Distinguishing `purpose` written into `zeroship.magic_links.purpose`.
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

/// Result of a successful [`complete`] — the row's linked user.
#[derive(Debug, Clone)]
pub struct CompletedReset {
    pub user_id: uuid::Uuid,
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
            "UPDATE zeroship.magic_links SET consumed_at = NOW() \
             WHERE email = $1::citext AND purpose = $2 AND consumed_at IS NULL",
            &[&email, &PURPOSE],
        )
        .await
        .map_err(|e| AuthError::Db(format!("password_reset supersede previous: {e}")))?;

        // 3. Insert the new row.
        db.execute(
            "INSERT INTO zeroship.magic_links \
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
            "UPDATE zeroship.magic_links SET consumed_at = NOW() \
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

/// Atomically redeem a password-reset token and update the linked user's
/// password hash. Returns `Ok(Some(_))` on success, `Ok(None)` if the
/// token is invalid or expired.
///
/// The user update and token consume are one SQL statement. If the user
/// update fails, PostgreSQL rolls back the token consume as part of that
/// same statement.
///
/// **Gateway app-session teardown (security finding H1).** A password reset
/// exists to evict an intruder who knows the old credential, so it MUST
/// durably terminate the gateway app-session tier — not just the IdP login
/// session. The gateway's `__Host-zeroship_app_session` cookie is validated
/// 100% statelessly; its ONLY revocation gate is the per-app family marker in
/// `zeroship.token_revocations` keyed on `(client_id, pws_)`, and its 30-day
/// `__Host-zeroship_app_anchor` reload-recovery credential re-mints fresh
/// cookies via `?mint=1` as long as the `app_session_anchors` row is live. So
/// this statement, in the SAME transaction as the password change:
///
///   1. bumps `users.credential_version` (mirrors `users::update_password_hash`
///      — defense in depth for the IdP-session credential-version gate);
///   2. writes a `(client_id, pairwise_sub)` family marker for EVERY app the
///      user holds an identity with, drawing the `pairwise_sub` the cookie
///      carries from `app_user_identities` (auth cannot derive `pws_` itself —
///      it holds neither `pairwise_salt` nor the route sector — but the gateway
///      persisted it there at projection time). This rejects every already-live
///      app-session cookie / wrapper token for that family from now on; and
///   3. revokes (`revoked_at = NOW()`) every `app_session_anchors` row for the
///      user, so `anchors::read_live` returns `None` and `?mint=1` fails closed
///      — no fresh cookie can be minted to outrun the family marker.
///
/// The Hydra refresh-grant *itself* is left for the gateway/Hydra side to age
/// out (auth holds no `anchor_enc_key` to decrypt the per-anchor refresh
/// family), but revoking the anchor means the gateway never touches that
/// refresh family again, and the family marker rejects any token it could yield
/// — so the 720h Hydra ceiling is moot.
///
/// # Errors
///
/// Returns [`AuthError::Db`] on PG failure.
pub async fn complete(
    db: &Client,
    raw_token: &str,
    password_hash: &str,
) -> Result<Option<CompletedReset>> {
    let token_hash = sha256(raw_token);
    let rows = db
        .query(
            "WITH candidate AS ( \
                 SELECT u.id AS user_id, u.email::text AS email \
                 FROM zeroship.magic_links ml \
                 JOIN zeroship.users u ON u.email = ml.email \
                 WHERE ml.token_hash = $1 \
                   AND ml.purpose = $2 \
                   AND ml.consumed_at IS NULL \
                   AND ml.expires_at > NOW() \
             ), updated_user AS ( \
                 UPDATE zeroship.users u \
                 SET password_hash = $3, \
                     credential_version = u.credential_version + 1, \
                     updated_at = NOW() \
                 FROM candidate c \
                 WHERE u.id = c.user_id \
                 RETURNING u.id, c.email \
             ), consumed AS ( \
                 UPDATE zeroship.magic_links ml \
                 SET consumed_at = NOW() \
                 FROM updated_user u \
                 WHERE ml.token_hash = $1 \
                   AND ml.purpose = $2 \
                   AND ml.email = u.email::citext \
                   AND ml.consumed_at IS NULL \
                 RETURNING u.id AS user_id, u.email AS email \
             ), revoked_families AS ( \
                 INSERT INTO zeroship.token_revocations (client_id, sub, revoked_after) \
                 SELECT aui.app_client_id, aui.pairwise_sub, NOW() \
                 FROM zeroship.app_user_identities aui \
                 JOIN consumed c ON c.user_id = aui.global_user_id \
                 ON CONFLICT (client_id, sub) \
                   DO UPDATE SET revoked_after = EXCLUDED.revoked_after \
             ), revoked_anchors AS ( \
                 UPDATE zeroship.app_session_anchors a \
                 SET revoked_at = NOW() \
                 FROM consumed c \
                 WHERE a.global_user_id = c.user_id \
                   AND a.revoked_at IS NULL \
             ) \
             SELECT user_id, email FROM consumed",
            &[&token_hash.as_slice(), &PURPOSE, &password_hash],
        )
        .await
        .map_err(|e| AuthError::Db(format!("password_reset complete: {e}")))?;
    Ok(rows.first().map(|r| CompletedReset {
        user_id: r.get("user_id"),
        email: r.get("email"),
    }))
}

fn sha256(s: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().into()
}
