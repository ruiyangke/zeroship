//! Password-reset token primitive.
//!
//! Per proposal §8.3:
//!
//! - **Issue**: generate a 32-byte CSPRNG random token, store its SHA-256
//!   in `zeroship.magic_links` keyed by `(email, purpose='reset')`. The row
//!   also captures the user's IMMUTABLE `user_id` (resolved from the email at
//!   issue time) — `complete` binds on that id, never re-resolving the target
//!   by email. Returns the raw token to the caller,
//!   which embeds it in the `/reset?token=` email link.
//!
//! - **Complete**: SHA-256 the raw token, atomically update the user's
//!   password hash and consume the reset row by `(token_hash,
//!   purpose='reset')` with the predicates `consumed_at IS NULL` AND
//!   `expires_at > NOW()`. The target account is selected by the row's
//!   stored `user_id`, not an email JOIN, so an email reassignment between
//!   issue and complete cannot retarget the reset. Single-use is enforced at
//!   the database layer.
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
//! Reuses `zeroship.magic_links` with `purpose='reset'` rather than
//! introducing yet another single-use-token table — the shape is
//! identical (`token_hash`, email, purpose, expiry, `consumed_at`). The
//! `csrf_nonce` column is required by the table schema but unused for
//! reset tokens (no cross-device flow); we write a sentinel placeholder.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use compio_postgres::{Client, GenericClient};
use rand::RngCore;
use sha2::{Digest, Sha256};
use zeroship_core::UserId;

use crate::advisory_lock::NS_USER;
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
    pub user_id: UserId,
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

        // 3. Insert the new row, binding it to the user's IMMUTABLE id
        //    resolved from the email NOW (security finding L4). `complete`
        //    filters on this id, never re-resolving the target by email — so
        //    a later email reassignment cannot retarget the reset to a
        //    different account. The id is captured via a sub-SELECT in the
        //    SAME statement (and SAME advisory-locked transaction) so it is
        //    consistent with the email the token is keyed on. If the email
        //    has no user, no row is inserted and the token is inert.
        db.execute(
            "INSERT INTO zeroship.magic_links \
                (token_hash, email, csrf_nonce, purpose, expires_at, user_id) \
             SELECT $1, $2::citext, $3, $4, \
                    NOW() + ($5::text || ' minutes')::interval, u.id \
             FROM zeroship.users u WHERE u.email = $2::citext",
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

/// Is there still a live reset row for `raw_token`?
///
/// This is NOT the authority on redemption: [`complete`] is, and it remains
/// the single statement that consumes the token together with the password
/// update. This exists so a caller can decline an already-dead token before
/// spending an Argon2 hash (19 MiB and a blocking-pool slot) on the submitted
/// password.
///
/// The predicates mirror [`complete`]'s candidate CTE exactly, so this never
/// refuses a token [`complete`] would have accepted. It is not a
/// check-then-use race either: a token that passes here and then loses to a
/// concurrent redemption is still rejected by [`complete`], which decides
/// correctness on its own.
///
/// # Errors
///
/// Returns [`AuthError::Db`] on PG failure.
pub async fn is_live(db: &Client, raw_token: &str) -> Result<bool> {
    let token_hash = sha256(raw_token);
    let rows = db
        .query(
            "SELECT 1 AS live \
             FROM zeroship.magic_links ml \
             JOIN zeroship.users u ON u.id = ml.user_id \
             WHERE ml.token_hash = $1 \
               AND ml.purpose = $2 \
               AND ml.user_id IS NOT NULL \
               AND ml.consumed_at IS NULL \
               AND ml.expires_at > NOW()",
            &[&token_hash.as_slice(), &PURPOSE],
        )
        .await
        .map_err(|e| AuthError::Db(format!("password_reset is_live: {e}")))?;
    Ok(!rows.is_empty())
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
///   1. bumps `users.credential_version` for the IdP-session credential gate;
///   2. writes a `(client_id, sub)` family marker for every family the user
///      holds, from the UNION of two sources: `app_user_identities` (the
///      `pairwise_sub` an app-session cookie carries, persisted by the
///      access-token issuer and the gateway session minters) and the live
///      `zeroship.sessions` rows joined to their grant (which also covers the
///      `zeroship-cli` platform session). This rejects every already-live
///      wrapper token for those families from now on; and
///
///      **The UNION is load-bearing, not tidying.** These were two sibling
///      CTEs, each its own `INSERT ... ON CONFLICT (client_id, sub) DO UPDATE`.
///      For a non-brokered app client the two sources carry the SAME pair -
///      `app_user_identities.pairwise_sub` and `zeroship.grants.subject` are
///      both `Issuer::pairwise_subject(user_id, sector_identifier)` - and
///      PostgreSQL refuses to let one command upsert a key twice:
///      `ON CONFLICT DO UPDATE command cannot affect row a second time`. That
///      error aborts the WHOLE statement, so the password change, the token
///      consume, the anchor revoke and both markers all rolled back and the
///      handler returned "internal error". The reset silently did NOTHING and
///      every session survived. It was the DEFAULT path, not an edge: the
///      gateway always requests `offline_access`, so one app login writes both
///      rows. Deduplicating the two sources into ONE insert is what makes the
///      key unique within the command. Regression:
///      `reset_completes_with_an_identity_and_refresh_grant_for_the_same_family`
///      in `tests/password_reset/mod.rs`.
///   3. revokes (`revoked_at = NOW()`) every `app_session_anchors` row for the
///      user, so `anchors::read_live` returns `None` and `?mint=1` fails closed
///      — no fresh cookie can be minted to outrun the family marker.
///
/// Revoking the anchor means the gateway never touches that refresh family
/// again, and the family marker rejects any token it could yield.
///
/// **F4 TOCTOU (gateway-side, closed).** A `?mint=1` rotation that read the
/// anchor BEFORE this reset commits used to re-sign a fresh cookie even though
/// the family was being torn down. The gateway now fails that rotation CLOSED:
/// `anchors::update_rotated_family` reports 0 rows when the anchor was revoked
/// mid-rotation, and `do_refresh` re-reads the `(client_id, pws_)` family
/// marker inside the persist tx and rejects when a marker landed at/after the
/// rotation started. So both signals this statement writes (anchor revoke +
/// family marker) now also stop an in-flight rotation, not just future ones.
///
/// **F2 lockout recovery.** The same UPDATE also zeroes `failed_login_count`
/// and clears `locked_until`. The L5 account-lockout was cleared ONLY by a
/// successful PASSWORD login (`credentials::reset_login_failures`), which is
/// unreachable while locked — so an attacker who knew the victim's email could
/// lock the account out of EVERY method permanently (~1 wrong POST/hr sustains
/// the exponential-backoff lock). A completed password reset is strong
/// owner-present evidence (the reset link was delivered to and redeemed from the
/// verified inbox), so it must restore access. The password-guessing defense is
/// unchanged: only a *successful reset* clears the lock, never a guess.
///
/// # Errors
///
/// Returns [`AuthError::Db`] on PG failure.
pub async fn complete(
    db: &(impl GenericClient + ?Sized),
    raw_token: &str,
    password_hash: &str,
) -> Result<Option<CompletedReset>> {
    let token_hash = sha256(raw_token);
    let rows = db
        .query(
            // Candidate is resolved by the IMMUTABLE `ml.user_id` captured at
            // issue time (security finding L4), NOT by re-joining on email —
            // so an email reassignment between issue and complete cannot
            // retarget the reset. Email is JOINed only as a display value and
            // plays no role in selecting the account. `ml.user_id IS NOT NULL`
            // excludes magic-LINK login rows (which leave it NULL) defensively.
            "WITH candidate AS ( \
                 SELECT ml.user_id AS user_id, u.email::text AS email \
                 FROM zeroship.magic_links ml \
                 JOIN zeroship.users u ON u.id = ml.user_id \
                 WHERE ml.token_hash = $1 \
                   AND ml.purpose = $2 \
                   AND ml.user_id IS NOT NULL \
                   AND ml.consumed_at IS NULL \
                   AND ml.expires_at > NOW() \
             ), locked AS ( \
                 SELECT pg_advisory_xact_lock($4::INT4, hashtext(c.user_id::text)) \
                 FROM candidate c \
             ), updated_user AS ( \
                 UPDATE zeroship.users u \
                 SET password_hash = $3, \
                     credential_version = u.credential_version + 1, \
                     failed_login_count = 0, \
                     locked_until = NULL, \
                     updated_at = NOW() \
                 FROM candidate c, locked l \
                 WHERE u.id = c.user_id \
                 RETURNING u.id, c.email \
             ), consumed AS ( \
                 UPDATE zeroship.magic_links ml \
                 SET consumed_at = NOW() \
                 FROM updated_user u \
                 WHERE ml.token_hash = $1 \
                   AND ml.purpose = $2 \
                   AND ml.user_id = u.id \
                   AND ml.consumed_at IS NULL \
                 RETURNING u.id AS user_id, u.email AS email \
             ), revoked_anchors AS ( \
                 UPDATE zeroship.app_session_anchors a \
                 SET revoked_at = NOW() \
                 FROM consumed c \
                 WHERE a.global_user_id = c.user_id \
                   AND a.revoked_at IS NULL \
             ), family_targets AS ( \
                 SELECT aui.app_client_id AS client_id, aui.pairwise_sub AS sub \
                 FROM zeroship.app_user_identities aui \
                 JOIN consumed c ON c.user_id = aui.global_user_id \
                 UNION \
                 SELECT COALESCE(g.client_id, $5), g.subject \
                 FROM zeroship.sessions s \
                 JOIN zeroship.grants g ON g.id = s.grant_id \
                 JOIN consumed c ON c.user_id = s.person_id \
                 WHERE s.revoked_at IS NULL \
             ), family_markers AS ( \
                 INSERT INTO zeroship.token_revocations (client_id, sub, revoked_after) \
                 SELECT client_id, sub, NOW() FROM family_targets \
                 ON CONFLICT (client_id, sub) \
                   DO UPDATE SET revoked_after = \
                     GREATEST(zeroship.token_revocations.revoked_after, EXCLUDED.revoked_after) \
             ), refresh_revoked AS ( \
                 UPDATE zeroship.sessions s \
                 SET revoked_at = NOW() \
                 FROM consumed c \
                 WHERE s.person_id = c.user_id \
                   AND s.revoked_at IS NULL \
             ) \
             SELECT user_id, email FROM consumed",
            &[
                &token_hash.as_slice(),
                &PURPOSE,
                &password_hash,
                &NS_USER,
                &zeroship_core::device_grant::PLATFORM_CLI_CLIENT_ID,
            ],
        )
        .await
        .map_err(|e| AuthError::Db(format!("password_reset complete: {e}")))?;
    rows.first()
        .map(|row| {
            Ok(CompletedReset {
                user_id: crate::entity_ids::user_id_with_context(
                    row,
                    "user_id",
                    "password reset complete",
                )?,
                email: row.get("email"),
            })
        })
        .transpose()
}

fn sha256(s: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().into()
}
