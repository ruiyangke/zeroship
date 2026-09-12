//! Cross-device magic-login completion persistence and reservation ownership.

use compio_postgres::{Client, GenericClient};

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

/// Atomically reserve a completion row, invalidating it after five
/// failed code attempts. A second correct-code consume within 60
/// seconds of an existing reservation reports `InFlight`; stale
/// reservations can be retried.
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

pub async fn finalize_consume(
    db: &Client,
    csrf_nonce: &str,
    reserved_at: &chrono::DateTime<chrono::Utc>,
) -> crate::error::Result<bool> {
    let updated = db
        .execute(
            "UPDATE zeroship.magic_completions \
             SET consumed_at = NOW() \
             WHERE csrf_nonce = $1 \
               AND consumed_pending_at = $2 \
               AND consumed_at IS NULL",
            &[&csrf_nonce, reserved_at],
        )
        .await
        .map_err(|e| AuthError::Db(format!("magic_completions finalize consume: {e}")))?;
    Ok(updated > 0)
}

pub async fn clear_consume_pending(
    db: &Client,
    csrf_nonce: &str,
    reserved_at: Option<&chrono::DateTime<chrono::Utc>>,
) -> crate::error::Result<bool> {
    let result = if let Some(ts) = reserved_at {
        db.execute(
            "UPDATE zeroship.magic_completions \
             SET consumed_pending_at = NULL \
             WHERE csrf_nonce = $1 \
               AND consumed_pending_at = $2 \
               AND consumed_at IS NULL",
            &[&csrf_nonce, ts],
        )
        .await
    } else {
        db.execute(
            "UPDATE zeroship.magic_completions \
             SET consumed_pending_at = NULL \
             WHERE csrf_nonce = $1 \
               AND consumed_at IS NULL",
            &[&csrf_nonce],
        )
        .await
    };
    let updated = result
        .map_err(|e| AuthError::Db(format!("magic_completions clear consume pending: {e}")))?;
    Ok(updated > 0)
}
