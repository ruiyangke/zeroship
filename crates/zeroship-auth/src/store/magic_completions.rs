//! Cross-device magic-login completion persistence and reservation ownership.

use compio_postgres::{Client, GenericClient};
use zeroship_data_orm::orm::{Database, TimestampExpr, UtcInstant};

use super::native::instant_value;
use super::native::models::magic_completions as model;

use crate::error::AuthError;

#[derive(Debug, Clone)]
pub struct Completion {
    pub email: String,
    pub target: String,
    pub reserved_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug)]
pub enum ConsumeError {
    InFlight,
    WrongCode,
    Store(AuthError),
}

/// Publish a completion for a magic-link nonce with an explicit expiry.
/// Replacing an existing completion resets its attempts and reservation state.
///
/// # Errors
/// Returns an error for an invalid email or a database failure.
pub async fn create(
    db: &Client,
    csrf_nonce: &str,
    code: &str,
    email: &str,
    target: &str,
    expires_secs: i64,
) -> crate::error::Result<()> {
    crate::identity::email::validate_email(email)
        .map_err(|_| AuthError::Internal("invalid email".into()))?;

    db.execute(
        "INSERT INTO zeroship.magic_completions \
            (csrf_nonce, code, email, login_challenge, expires_at) \
         VALUES ($1, $2, $3::citext, $4, NOW() + ($5::text || ' seconds')::interval) \
         ON CONFLICT (csrf_nonce) DO UPDATE SET \
            code = EXCLUDED.code, \
            email = EXCLUDED.email, \
            login_challenge = EXCLUDED.login_challenge, \
            expires_at = EXCLUDED.expires_at, \
            attempts = 0, \
            consumed_pending_at = NULL, \
            consumed_at = NULL",
        &[
            &csrf_nonce,
            &code,
            &email,
            &target,
            &expires_secs.to_string(),
        ],
    )
    .await
    .map_err(|e| AuthError::Db(format!("magic_completions insert: {e}")))?;
    Ok(())
}

/// Reserve a completion, or classify why it cannot be reserved.
///
/// Active reservations exclude competing requests. Stale reservations can be
/// retried; exhausting the attempt budget consumes the completion.
///
/// # Errors
/// Returns `InFlight` for an active reservation, `WrongCode` for an invalid
/// code or unavailable completion, and `Store` for a database failure.
#[allow(
    clippy::future_not_send,
    reason = "database operations stay on their owning runtime"
)]
pub async fn consume_pending(
    db: &(impl GenericClient + ?Sized),
    csrf_nonce: &str,
    code: &str,
) -> std::result::Result<Completion, ConsumeError> {
    let rows = db
        .query(
            "UPDATE zeroship.magic_completions \
             SET attempts = (attempts + 1)::SMALLINT, \
                 consumed_pending_at = NOW() \
             WHERE csrf_nonce = $1 \
               AND code = $2 \
               AND attempts < 5 \
               AND consumed_at IS NULL \
               AND (consumed_pending_at IS NULL \
                    OR consumed_pending_at <= NOW() - INTERVAL '60 seconds') \
               AND expires_at > NOW() \
             RETURNING email::text, login_challenge, consumed_pending_at",
            &[&csrf_nonce, &code],
        )
        .await
        .map_err(|e| {
            ConsumeError::Store(AuthError::Db(format!("magic_completions consume: {e}")))
        })?;

    if let Some(row) = rows.first() {
        return Ok(Completion {
            email: row.get("email"),
            target: row.get("login_challenge"),
            reserved_at: row.get("consumed_pending_at"),
        });
    }

    let in_flight = db
        .query(
            "SELECT TRUE AS in_flight \
             FROM zeroship.magic_completions \
             WHERE csrf_nonce = $1 \
               AND consumed_at IS NULL \
               AND consumed_pending_at > NOW() - INTERVAL '60 seconds' \
               AND expires_at > NOW()",
            &[&csrf_nonce],
        )
        .await
        .map_err(|e| {
            ConsumeError::Store(AuthError::Db(format!(
                "magic_completions consume in-flight: {e}"
            )))
        })?;
    if !in_flight.is_empty() {
        return Err(ConsumeError::InFlight);
    }

    let wrong_rows = db
        .query(
            "UPDATE zeroship.magic_completions \
             SET attempts = (attempts + 1)::SMALLINT, \
                 consumed_at = CASE \
                     WHEN attempts + 1 >= 5 THEN NOW() \
                     ELSE consumed_at \
                 END \
             WHERE csrf_nonce = $1 \
               AND code <> $2 \
               AND attempts < 5 \
               AND consumed_at IS NULL \
               AND consumed_pending_at IS NULL \
               AND expires_at > NOW() \
             RETURNING attempts",
            &[&csrf_nonce, &code],
        )
        .await
        .map_err(|e| {
            ConsumeError::Store(AuthError::Db(format!(
                "magic_completions wrong-code consume: {e}"
            )))
        })?;

    if wrong_rows.is_empty() {
        return Err(ConsumeError::WrongCode);
    }

    Err(ConsumeError::WrongCode)
}

/// Finish the reservation identified by its timestamp.
///
/// The stored instant is bound back through the same conversion that decoded it
/// (`native::instant_value`), so the equality matches the reservation the caller
/// took rather than a coarser instant.
///
/// # Errors
/// Returns an error if the database update fails.
#[expect(
    clippy::future_not_send,
    reason = "the ORM belongs to its compio runtime"
)]
pub async fn finalize_consume(
    db: &Database,
    csrf_nonce: &str,
    reserved_at: &chrono::DateTime<chrono::Utc>,
) -> crate::error::Result<bool> {
    let updated = db
        .entity::<model::Entity>()?
        .update_many(
            model::csrf_nonce.eq(csrf_nonce)?.and(
                model::consumed_pending_at
                    .eq(Some(instant_value(reserved_at.to_owned())?))?
                    .and(model::consumed_at.is_null()),
            ),
            model::consumed_at.set_expression(TimestampExpr::database_now())?,
        )
        .await?;
    Ok(updated > 0)
}

/// Release a reservation after a handoff fails.
///
/// A supplied timestamp must match the current reservation, bound back through
/// `native::instant_value`. Without one, any unconsumed reservation for the
/// nonce can be cleared.
///
/// # Errors
/// Returns an error if the database update fails.
#[expect(
    clippy::future_not_send,
    reason = "the ORM belongs to its compio runtime"
)]
pub async fn clear_consume_pending(
    db: &Database,
    csrf_nonce: &str,
    reserved_at: Option<&chrono::DateTime<chrono::Utc>>,
) -> crate::error::Result<bool> {
    let mut filter = model::csrf_nonce
        .eq(csrf_nonce)?
        .and(model::consumed_at.is_null());
    if let Some(ts) = reserved_at {
        filter = filter.and(
            model::consumed_pending_at.eq(Some(instant_value(ts.to_owned())?))?,
        );
    }
    let updated = db
        .entity::<model::Entity>()?
        .update_many(filter, model::consumed_pending_at.set(None::<UtcInstant>)?)
        .await?;
    Ok(updated > 0)
}
