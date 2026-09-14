//! `zeroship.users` CRUD.

#![allow(
    clippy::future_not_send,
    reason = "the ORM belongs to its compio runtime"
)]

use compio_postgres::{Client, GenericClient};
use zeroship_core::UserId;
use zeroship_data_orm::orm::{Database, DbError, TimestampExpr};

use super::native::models::users as model;

use crate::advisory_lock::lock_refresh_user_xact;
use crate::error::{AuthError, Result};
use crate::oidc::refresh::revoke_person_sessions_in_transaction;

#[derive(Debug, zeroship_data_orm::orm::Insertable)]
#[orm(entity = super::native::models::users)]
pub struct NewUser<'a> {
    #[orm(encode_with = super::native::user_id_text)]
    pub id: UserId,
    pub email: &'a str,
    pub name: &'a str,
    pub password_hash: Option<&'a str>,
}

#[derive(Debug, Clone, zeroship_data_orm::orm::FromRow)]
#[orm(entity = super::native::models::users)]
pub struct UserRow {
    #[orm(decode_with = super::native::user_id)]
    pub id: UserId,
    pub email: String,
    #[orm(decode_with = super::native::optional_timestamp)]
    pub email_verified_at: Option<chrono::DateTime<chrono::Utc>>,
    pub name: String,
    pub avatar_url: Option<String>,
    pub password_hash: Option<String>,
    pub credential_version: i64,
    #[orm(decode_with = super::native::optional_timestamp)]
    pub locked_until: Option<chrono::DateTime<chrono::Utc>>,
    #[orm(decode_with = super::native::optional_timestamp)]
    pub disabled_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Look up a user using the schema's case-insensitive email column.
///
/// # Errors
/// Returns database or model-conversion errors.
pub async fn find_by_email(
    db: &Database,
    email: &str,
) -> std::result::Result<Option<UserRow>, DbError> {
    db.entity::<model::Entity>()?
        .query()
        .filter(model::email.eq(email)?)
        .first()
        .await
}

/// Look up a user by their typed identity.
///
/// # Errors
/// Returns database or model-conversion errors.
pub async fn find_by_id(
    db: &Database,
    id: &UserId,
) -> std::result::Result<Option<UserRow>, DbError> {
    db.entity::<model::Entity>()?
        .query()
        .filter(model::id.eq(id.as_str())?)
        .first()
        .await
}

/// Create a user with an identity minted by auth.
///
/// # Errors
/// Returns database constraints or model-conversion errors.
pub async fn create(
    db: &Database,
    email: &str,
    name: &str,
    password_hash: Option<&str>,
) -> std::result::Result<UserRow, DbError> {
    db.entity::<model::Entity>()?
        .insert(NewUser {
            id: UserId::mint(),
            email,
            name,
            password_hash,
        })
        .await
}

/// Account-lockout policy (security finding L5). Per-user, conservative on
/// purpose so an attacker can't trivially lock out a victim — the per-email
/// leaky bucket (cap 10/hr) is the broad throttle; this is the escalation arm
/// that turns sustained guessing against ONE account into a hard stop with
/// exponential backoff.
///
/// Lockout engages on the Nth consecutive wrong-password attempt and is
/// cleared on the next success. Backoff grows with the overage past the
/// threshold (1 min → 2 → 4 → … capped) so a brief blip recovers fast while a
/// sustained attack escalates.
pub mod lockout {
    /// Consecutive failures before the account is locked.
    pub const THRESHOLD: i32 = 5;
    /// Initial lock duration once the threshold is first crossed.
    pub const INITIAL_BACKOFF_SECS: i64 = 60;
    /// Upper bound on the (exponentially growing) lock duration.
    pub const MAX_BACKOFF_SECS: i64 = 3600;

    /// Lock duration for a row whose `failed_login_count` has reached `count`
    /// (already including the failure being recorded). Returns `None` while
    /// below `THRESHOLD` (no lock yet).
    #[must_use]
    pub fn backoff_secs(count: i32) -> Option<i64> {
        if count < THRESHOLD {
            return None;
        }
        // Overage past the threshold doubles the window: 0→1×, 1→2×, 2→4×…
        // `min(20)` keeps the shift in range before the cap clamps it.
        let overage = (count - THRESHOLD).min(20);
        let secs = INITIAL_BACKOFF_SECS.saturating_mul(1_i64 << overage);
        Some(secs.min(MAX_BACKOFF_SECS))
    }
}

/// Increment the failure count and apply its lockout deadline atomically.
///
/// # Errors
/// Returns database or model-conversion errors.
pub async fn record_login_failure(db: &Database, id: &UserId) -> Result<i32> {
    db.transaction(|tx| async move {
        let users = tx.entity::<model::Entity>()?;
        let Some(row) = users
            .update::<_, LoginFailures>(
                model::id.eq(id.as_str())?,
                model::failed_login_count.increment(1_i32)?,
            )
            .await?
        else {
            return Ok(0);
        };
        if let Some(seconds) = lockout::backoff_secs(row.failed_login_count) {
            let duration = std::time::Duration::from_secs(seconds.unsigned_abs());
            users
                .update::<_, LoginFailures>(
                    model::id.eq(id.as_str())?,
                    model::locked_until
                        .set_expression(TimestampExpr::database_now().plus(duration)?)?,
                )
                .await?;
        }
        Ok(row.failed_login_count)
    })
    .await
    .map_err(Into::into)
}

#[derive(zeroship_data_orm::orm::FromRow)]
#[orm(entity = super::native::models::users)]
struct LoginFailures {
    failed_login_count: i32,
}

/// Perform the failure write against an unpersisted identity for refused logins.
/// The caller logs errors without changing its credential decision.
///
/// # Errors
/// Returns database or model-conversion errors.
pub async fn record_login_failure_dummy(db: &Database) -> Result<()> {
    record_login_failure(db, &UserId::mint()).await?;
    Ok(())
}

/// Clear accumulated lockout state after a successful login.
/// An already clean account is left untouched.
///
/// # Errors
/// Returns database or model-conversion errors.
pub async fn reset_login_failures(db: &Database, id: &UserId) -> Result<()> {
    db.entity::<model::Entity>()?
        .update::<_, LoginFailures>(
            model::id.eq(id.as_str())?.and(
                model::failed_login_count
                    .ne(0_i32)?
                    .or(model::locked_until.is_not_null()),
            ),
            model::failed_login_count
                .set(0_i32)?
                .and(model::locked_until.set(None::<i64>)?)?,
        )
        .await?;
    Ok(())
}

/// Contact details an [`request_deletion`] returns so the caller can send the
/// confirm/undo email and audit the request. (`UserRow` would also carry these,
/// but a dedicated struct documents exactly what the delete flow needs.)
#[derive(Debug, Clone)]
pub struct DeletionRequest {
    pub user_id: UserId,
    pub email: String,
    pub name: String,
    pub scheduled_for: chrono::DateTime<chrono::Utc>,
    /// Sessions torn down as part of the request (idp + gateway), for audit.
    pub idp_sessions_revoked: u64,
    pub gateway_sessions_revoked: u64,
    /// The single-use undo token, minted inside the same transaction that
    /// opened the window and returned exactly once - to the code that puts it
    /// in the confirmation email.
    ///
    /// It is here, and not left to the handler, so that "a scheduled deletion
    /// always has a live undo token" is a property of this transaction rather
    /// than of whoever remembers to call a second function. Every credential
    /// this transaction revokes is one the person cannot use to change their
    /// mind; this is the one that replaces them.
    pub cancel_token: String,
}

/// Begin an account-deletion request (ISS-12 / GDPR Art. 17), atomically:
///
///   1. stamp `deletion_requested_at = NOW()` and
///      `deletion_scheduled_for = NOW() + grace_days`,
///   2. bump `credential_version` so already-issued IdP sessions that bind the
///      version stop validating (the gateway's pulled lifecycle snapshot
///      separately denies stateless app credentials),
///   3. revoke every refresh family and every mapped app token family,
///   4. revoke every app-session recovery anchor,
///   5. mark every live `idp_sessions` / `gateway_sessions` row revoked, and
///   6. mint the single-use undo token (`identity::deletion_cancel`) that is
///      the ONLY credential able to reverse steps 1-5.
///
/// Step 6 is inside the transaction because steps 2-5 are: this function
/// destroys every credential the person could otherwise have used to change
/// their mind, so the replacement has to be committed by the same statement
/// batch that destroyed them, not by whoever calls this next.
///
/// Idempotent on an already-requested row: it leaves an existing
/// `deletion_requested_at` untouched (the schedule does not slide) but still
/// re-asserts the lifecycle state and revocation. Returns `Ok(None)` if no such
/// user.
///
/// External logout fanout is NOT done here, so this function stays a pure DB
/// transaction.
///
/// # Errors
///
/// Returns `AuthError::Db` on PG failure (the transaction is rolled back).
pub async fn request_deletion(
    conn: &mut Client,
    id: &UserId,
    grace_days: i64,
) -> Result<Option<DeletionRequest>> {
    let tx = conn
        .transaction()
        .await
        .map_err(|e| AuthError::Db(format!("request_deletion begin: {e}")))?;
    let result = request_deletion_tx(&tx, id, grace_days).await;
    match result {
        Ok(value) => {
            tx.commit()
                .await
                .map_err(|e| AuthError::Db(format!("request_deletion commit: {e}")))?;
            Ok(value)
        }
        Err(e) => {
            if let Err(rb) = tx.rollback().await {
                tracing::error!(error = %rb, "request_deletion rollback failed");
            }
            Err(e)
        }
    }
}

async fn request_deletion_tx(
    conn: &(impl GenericClient + ?Sized),
    id: &UserId,
    grace_days: i64,
) -> Result<Option<DeletionRequest>> {
    lock_refresh_user_xact(conn, id)
        .await
        .map_err(|e| AuthError::Db(format!("request_deletion refresh user lock: {e}")))?;
    let rows = conn
        .query(
            "UPDATE zeroship.users \
             SET deletion_requested_at = COALESCE(deletion_requested_at, NOW()), \
                 deletion_scheduled_for = COALESCE( \
                     deletion_scheduled_for, NOW() + make_interval(days => $2::int)), \
                 credential_version = credential_version + 1, \
                 updated_at = NOW() \
             WHERE id = $1 \
               AND anonymized_at IS NULL \
             RETURNING email::text AS email, name, deletion_scheduled_for",
            &[&id.as_str(), &i32::try_from(grace_days).unwrap_or(30)],
        )
        .await
        .map_err(|e| AuthError::Db(format!("request_deletion update users: {e}")))?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    revoke_person_sessions_in_transaction(conn, id, "account_deletion")
        .await
        .map_err(|e| AuthError::Db(format!("request_deletion revoke refresh families: {e}")))?;
    revoke_user_app_credentials_in_transaction(conn, id).await?;
    let email: String = row.get("email");
    let name: String = row.get("name");
    let scheduled_for: chrono::DateTime<chrono::Utc> = row.get("deletion_scheduled_for");

    let idp_sessions_revoked = conn
        .execute(
            "UPDATE zeroship.idp_sessions SET revoked_at = NOW() \
             WHERE user_id = $1 AND revoked_at IS NULL",
            &[&id.as_str()],
        )
        .await
        .map_err(|e| AuthError::Db(format!("request_deletion revoke idp_sessions: {e}")))?;
    // DELETE (not UPDATE revoked_at): the `zeroship_auth` role has only
    // SELECT,DELETE on gateway_sessions (no UPDATE — changeset 0025), so an
    // `UPDATE` here fails permission-denied under the real role (it passed
    // tests only because they run as superuser). DELETE matches the existing
    // password-reset cascade (`ui/reset.rs`) and also clears the rows that
    // would otherwise be orphaned on a later hard-delete (gateway_sessions has
    // no FK to users, so CASCADE never reaches them).
    let gateway_sessions_revoked = conn
        .execute(
            "DELETE FROM zeroship.gateway_sessions WHERE user_id = $1",
            &[&id.as_str()],
        )
        .await
        .map_err(|e| AuthError::Db(format!("request_deletion delete gateway_sessions: {e}")))?;

    // The undo credential, minted last so it carries the schedule this
    // transaction actually committed rather than the one it intended.
    let cancel_token =
        crate::identity::deletion_cancel::issue_in_transaction(conn, id, scheduled_for)
            .await?
            .raw;

    Ok(Some(DeletionRequest {
        user_id: id.clone(),
        email,
        name,
        scheduled_for,
        idp_sessions_revoked,
        gateway_sessions_revoked,
        cancel_token,
    }))
}

async fn revoke_user_app_credentials_in_transaction(
    conn: &(impl GenericClient + ?Sized),
    user_id: &UserId,
) -> Result<()> {
    conn.execute(
        "WITH stamp AS ( \
             SELECT clock_timestamp() AS revoked_after \
         ), revoked_anchors AS ( \
             UPDATE zeroship.app_session_anchors \
             SET revoked_at = (SELECT revoked_after FROM stamp) \
             WHERE global_user_id = $1 AND revoked_at IS NULL \
             RETURNING 1 \
         ), families AS ( \
             SELECT 'zeroship-cli'::text AS client_id, \
                    $1::text AS sub, revoked_after \
             FROM stamp \
             UNION ALL \
             SELECT aui.app_client_id, aui.pairwise_sub, stamp.revoked_after \
             FROM zeroship.app_user_identities aui CROSS JOIN stamp \
             WHERE aui.global_user_id = $1 \
         ) \
         INSERT INTO zeroship.token_revocations (client_id, sub, revoked_after) \
         SELECT client_id, sub, revoked_after FROM families \
         ON CONFLICT (client_id, sub) \
           DO UPDATE SET revoked_after = \
             GREATEST(zeroship.token_revocations.revoked_after, EXCLUDED.revoked_after)",
        &[&user_id.as_str()],
    )
    .await
    .map(|_| ())
    .map_err(|e| AuthError::Db(format!("request_deletion revoke app credentials: {e}")))
}

// There is no `cancel_deletion(user_id)` here any more, and its absence is the
// point. Clearing the schedule by id was authority nobody could present:
// `request_deletion` revokes every credential in the same transaction, so no
// caller could ever prove it was the account's owner. The undo is
// `crate::identity::deletion_cancel::redeem`, which spends the mailed token and
// clears the schedule in ONE statement -- so the authority and the effect are
// the same act, and there is no by-id back door beside it.

/// Stamp the last successful login using the database clock.
///
/// # Errors
/// Returns database or model-conversion errors.
pub async fn touch_last_login(db: &Database, id: &UserId) -> Result<()> {
    db.entity::<model::Entity>()?
        .update::<_, UserRow>(
            model::id.eq(id.as_str())?,
            model::last_login_at.set_expression(TimestampExpr::database_now())?,
        )
        .await?;
    Ok(())
}

/// Record email verification without replacing an existing verification time.
///
/// # Errors
/// Returns database or model-conversion errors.
pub async fn mark_email_verified(db: &Database, id: &UserId) -> Result<()> {
    db.entity::<model::Entity>()?
        .update::<_, UserRow>(
            model::id
                .eq(id.as_str())?
                .and(model::email_verified_at.is_null()),
            model::email_verified_at.set_expression(TimestampExpr::database_now())?,
        )
        .await?;
    Ok(())
}

/// Use a provider avatar only when the account has no avatar.
///
/// # Errors
/// Returns database or model-conversion errors.
pub async fn set_avatar_if_missing(db: &Database, id: &UserId, avatar: &str) -> Result<()> {
    db.entity::<model::Entity>()?
        .update::<_, UserRow>(
            model::id.eq(id.as_str())?.and(model::avatar_url.is_null()),
            model::avatar_url.set(Some(avatar))?,
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod user_id_type_tests {
    use super::UserRow;

    #[test]
    fn user_rows_expose_the_canonical_user_id_type() {
        let project: fn(&UserRow) -> &zeroship_core::UserId = |row| &row.id;
        let _ = project;
    }
}

#[cfg(test)]
mod tests {
    use super::lockout;

    #[test]
    fn no_lock_below_threshold() {
        for count in 0..lockout::THRESHOLD {
            assert_eq!(
                lockout::backoff_secs(count),
                None,
                "count {count} is below threshold and must not lock"
            );
        }
    }

    #[test]
    fn lock_engages_at_threshold_with_initial_backoff() {
        assert_eq!(
            lockout::backoff_secs(lockout::THRESHOLD),
            Some(lockout::INITIAL_BACKOFF_SECS),
            "first lock uses the initial backoff"
        );
    }

    #[test]
    fn backoff_doubles_then_caps() {
        // Each failure past the threshold doubles the window.
        assert_eq!(
            lockout::backoff_secs(lockout::THRESHOLD + 1),
            Some(lockout::INITIAL_BACKOFF_SECS * 2)
        );
        assert_eq!(
            lockout::backoff_secs(lockout::THRESHOLD + 2),
            Some(lockout::INITIAL_BACKOFF_SECS * 4)
        );
        // Far past the threshold the window saturates at the cap, never beyond,
        // and never overflows (large overage is clamped, no panic).
        assert_eq!(
            lockout::backoff_secs(lockout::THRESHOLD + 100),
            Some(lockout::MAX_BACKOFF_SECS)
        );
        assert_eq!(
            lockout::backoff_secs(i32::MAX),
            Some(lockout::MAX_BACKOFF_SECS)
        );
    }
}
