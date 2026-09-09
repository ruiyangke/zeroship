//! The undo credential for an account-deletion request.
//!
//! # Why a token, and why the session could never have been one
//!
//! `POST /me/delete` is a hard revocation by design: `store::users::request_deletion`
//! stamps `deletion_requested_at`, bumps `credential_version`, revokes every
//! refresh family and app anchor, marks every `idp_sessions` row revoked and
//! deletes every `gateway_sessions` row. That is correct - a person asking to be
//! deleted should not be leaving live credentials behind.
//!
//! It also, MEASURED, made the cancel route unreachable by three independent
//! predicates. `sessions::validate`'s `VALIDATE_SESSION_SQL` requires the
//! session's `credential_version` to equal the user's (the bump breaks it), and
//! `users.deletion_requested_at IS NULL` (the stamp breaks it), and
//! `idp_sessions.revoked_at IS NULL` (the revoke breaks it). Re-login is refused
//! a fourth time by `identity::eligibility::check_user_eligible`. So the handler
//! that existed to reverse the request could not resolve a caller, ever, and the
//! thirty-day window promised in the confirmation email was exercisable through
//! nothing this repository shipped.
//!
//! Relaxing any of those four would trade the revocation away for the undo. This
//! keeps both: the undo authority is a single-use token mailed to the address
//! the account is anchored to, which is the one channel that still exists in the
//! state the request creates, and the one the confirmation email was already
//! pointing at.
//!
//! # Shape
//!
//! 32 CSPRNG bytes, base64url; only the SHA-256 is stored. It reuses
//! `zeroship.magic_links` under `purpose = 'deletion_cancel'`, exactly as
//! `password_reset` reuses it under `'reset'` - same columns, same single-use
//! mechanics, and `magic_links_user_id_fkey` is `ON DELETE CASCADE`, so the
//! token dies with the account it could have saved.
//!
//! The TTL is not a constant here. It is the deletion schedule itself, passed in
//! by the caller, so "the window you were told you have" and "the window the
//! link works in" are the same value rather than two that can drift.
//!
//! [`redeem`] consumes the token and clears the schedule in ONE statement. A
//! token cannot be spent without cancelling, and a cancellation cannot happen
//! without spending one.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use compio_postgres::GenericClient;
use rand::RngCore;
use sha2::{Digest, Sha256};
use zeroship_core::user_id::UserId;

use crate::error::{AuthError, Result};

/// Number of CSPRNG bytes in the raw token. 256 bits.
const TOKEN_LEN_BYTES: usize = 32;

/// `zeroship.magic_links.purpose` for this family. The predicate is on every
/// statement below, so a `'login'` or `'reset'` token can never be spent here
/// and this token can never be spent there.
const PURPOSE: &str = "deletion_cancel";

/// `magic_links.csrf_nonce` is `NOT NULL` and this family has no cross-device
/// fork, so the column takes a deterministic sentinel rather than entropy
/// borrowed from somewhere it means something.
const CSRF_NONCE_SENTINEL: &str = "deletion-cancel-no-csrf-nonce";

/// The raw token, returned exactly once - to the code that puts it in the
/// email. Never logged, never stored.
#[derive(Debug, Clone)]
pub struct IssuedToken {
    pub raw: String,
}

/// Issue the undo token for a deletion request, inside the caller's
/// transaction.
///
/// `expires_at` is the request's `deletion_scheduled_for`. Any previously
/// unconsumed token for this user is superseded first: a repeated
/// `POST /me/delete` re-sends the confirmation, and the older link in an older
/// message must stop working when a newer one is issued.
///
/// # Errors
///
/// Returns [`AuthError::Db`] on PG failure. The caller's transaction is what
/// makes "a scheduled deletion always has a live undo token" true; issuing
/// outside it would make it true only when a handler remembered.
pub async fn issue_in_transaction(
    conn: &(impl GenericClient + ?Sized),
    user_id: &UserId,
    expires_at: chrono::DateTime<chrono::Utc>,
) -> Result<IssuedToken> {
    let mut token_bytes = [0u8; TOKEN_LEN_BYTES];
    rand::thread_rng().fill_bytes(&mut token_bytes);
    let raw = URL_SAFE_NO_PAD.encode(token_bytes);
    let token_hash = sha256(&raw);

    conn.execute(
        "UPDATE zeroship.magic_links SET consumed_at = NOW() \
         WHERE user_id = $1 AND purpose = $2 AND consumed_at IS NULL",
        &[&user_id.as_str(), &PURPOSE],
    )
    .await
    .map_err(|e| AuthError::Db(format!("deletion_cancel supersede previous: {e}")))?;

    conn.execute(
        "INSERT INTO zeroship.magic_links \
            (token_hash, email, csrf_nonce, purpose, expires_at, user_id) \
         SELECT $1, u.email, $2, $3, $4, u.id \
         FROM zeroship.users u WHERE u.id = $5",
        &[
            &token_hash.as_slice(),
            &CSRF_NONCE_SENTINEL,
            &PURPOSE,
            &expires_at,
            &user_id.as_str(),
        ],
    )
    .await
    .map_err(|e| AuthError::Db(format!("deletion_cancel insert: {e}")))?;

    Ok(IssuedToken { raw })
}

/// The account a redeemed token restored.
#[derive(Debug, Clone)]
pub struct CancelledDeletion {
    pub user_id: UserId,
    pub email: String,
}

/// Spend the token and cancel the deletion, in one statement.
///
/// Returns `Ok(None)` when the token matches no unconsumed, unexpired
/// `deletion_cancel` row OR when the user it names has no request in flight.
/// Those two are deliberately one answer: distinguishing them would tell an
/// unauthenticated caller holding a guessed token whether that account is
/// pending deletion.
///
/// A data-modifying CTE always executes, so in the second case the token IS
/// spent even though nothing was cancelled. That is the safe direction: the
/// only state it can reach is "a single-use credential with nothing left to
/// undo is now used", and the alternative - leaving it live - would keep a
/// credential alive across a state it no longer describes.
///
/// The target is the row's stored `user_id`, never a re-resolution by email, so
/// an address reassigned between issue and redeem cannot retarget the undo.
///
/// `credential_version` is NOT rolled back. The person signs in fresh, exactly
/// as after a password reset; the sessions the request revoked stay revoked.
///
/// # Errors
///
/// Returns [`AuthError::Db`] on PG failure.
pub async fn redeem(
    conn: &(impl GenericClient + Sync),
    raw_token: &str,
) -> Result<Option<CancelledDeletion>> {
    let token_hash = sha256(raw_token);
    let rows = conn
        .query(
            "WITH spent AS ( \
                 UPDATE zeroship.magic_links \
                 SET consumed_at = NOW() \
                 WHERE token_hash = $1 \
                   AND purpose = $2 \
                   AND user_id IS NOT NULL \
                   AND consumed_at IS NULL \
                   AND expires_at > NOW() \
                 RETURNING user_id \
             ) \
             UPDATE zeroship.users u \
             SET deletion_requested_at = NULL, \
                 deletion_scheduled_for = NULL, \
                 updated_at = NOW() \
             FROM spent \
             WHERE u.id = spent.user_id \
               AND u.deletion_requested_at IS NOT NULL \
             RETURNING u.id, u.email::text AS email",
            &[&token_hash.as_slice(), &PURPOSE],
        )
        .await
        .map_err(|e| AuthError::Db(format!("deletion_cancel redeem: {e}")))?;
    rows.first()
        .map(|row| {
            Ok(CancelledDeletion {
                user_id: crate::entity_ids::user_id(row, "id")?,
                email: row.get("email"),
            })
        })
        .transpose()
}

fn sha256(input: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The purpose predicate is the whole cross-family fence: without it a
    /// password-reset token would cancel a deletion and vice versa. Assert the
    /// literal rather than that it is non-empty - a value equal to
    /// `password_reset`'s would compile and would be the bug.
    #[test]
    fn the_purpose_is_its_own_and_is_not_the_reset_family() {
        assert_eq!(PURPOSE, "deletion_cancel");
        assert_ne!(PURPOSE, "reset");
        assert_ne!(PURPOSE, "login");
    }

    #[test]
    fn the_token_is_two_hundred_and_fifty_six_bits_of_url_safe_text() {
        assert_eq!(TOKEN_LEN_BYTES, 32);
        let raw = URL_SAFE_NO_PAD.encode([0xABu8; TOKEN_LEN_BYTES]);
        assert!(!raw.contains('+') && !raw.contains('/') && !raw.contains('='));
    }

    #[test]
    fn hashing_is_deterministic_and_separates_tokens() {
        assert_eq!(sha256("abc"), sha256("abc"));
        assert_ne!(sha256("abc"), sha256("abd"));
    }
}
