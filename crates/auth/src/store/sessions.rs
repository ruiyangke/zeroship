//! `auth.sessions` CRUD — the `IdP` login session at `auth.zeroship.ai`.

use compio_postgres::Client;

use crate::error::{AuthError, Result};

#[derive(Debug, Clone)]
pub struct Session {
    pub id: uuid::Uuid,
    pub user_id: uuid::Uuid,
    pub auth_method: String,
    pub amr: Vec<String>,
    pub acr: Option<String>,
    pub idle_expires_at: chrono::DateTime<chrono::Utc>,
    pub abs_expires_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug)]
pub struct CreateSession<'a> {
    pub user_id: uuid::Uuid,
    pub auth_method: &'a str,
    pub amr: Vec<String>,
    pub acr: Option<&'a str>,
    pub idle_minutes: i64,
    pub absolute_hours: i64,
}

/// Insert a new session row.
///
/// # Errors
///
/// `AuthError::Db` on PG failure.
pub async fn create(conn: &Client, params: &CreateSession<'_>) -> Result<Session> {
    let rows = conn
        .query(
            "INSERT INTO auth.sessions \
                (user_id, auth_method, amr, acr, idle_expires_at, abs_expires_at) \
             VALUES ($1, $2, $3, $4, NOW() + ($5::text || ' minutes')::interval, \
                                            NOW() + ($6::text || ' hours')::interval) \
             RETURNING id, user_id, auth_method, amr, acr, idle_expires_at, abs_expires_at",
            &[
                &params.user_id,
                &params.auth_method,
                &params.amr,
                &params.acr,
                &params.idle_minutes.to_string(),
                &params.absolute_hours.to_string(),
            ],
        )
        .await
        .map_err(|e| AuthError::Db(format!("sessions create: {e}")))?;
    let row = rows
        .first()
        .ok_or_else(|| AuthError::Db("sessions create: empty return".into()))?;
    Ok(Session {
        id: row.get("id"),
        user_id: row.get("user_id"),
        auth_method: row.get("auth_method"),
        amr: row.get("amr"),
        acr: row.try_get("acr").ok(),
        idle_expires_at: row.get("idle_expires_at"),
        abs_expires_at: row.get("abs_expires_at"),
    })
}

/// Validate an `IdP` session by id. Returns the row if valid, `None`
/// otherwise. Slides `idle_expires_at` forward on every successful
/// validation (the `IdP` session's sliding 30-min idle window).
///
/// "Valid" means: row exists, not revoked, idle and absolute expiries
/// both in the future. The check + slide is one atomic
/// `UPDATE ... RETURNING` so concurrent requests can't race the sliding
/// window. Mirrors `gateway::sessions::validate`.
///
/// # Errors
///
/// `AuthError::Db` on PG failure.
pub async fn validate(conn: &Client, id: uuid::Uuid) -> Result<Option<Session>> {
    let rows = conn
        .query(
            "UPDATE auth.sessions \
             SET idle_expires_at = NOW() + ($2::text || ' minutes')::interval \
             WHERE id = $1 \
               AND revoked_at IS NULL \
               AND idle_expires_at > NOW() \
               AND abs_expires_at > NOW() \
             RETURNING id, user_id, auth_method, amr, acr, idle_expires_at, abs_expires_at",
            &[
                &id,
                &crate::sessions::login::IDLE_MINUTES.to_string(),
            ],
        )
        .await
        .map_err(|e| AuthError::Db(format!("sessions validate: {e}")))?;
    Ok(rows.first().map(|row| Session {
        id: row.get("id"),
        user_id: row.get("user_id"),
        auth_method: row.get("auth_method"),
        amr: row.get("amr"),
        acr: row.try_get("acr").ok(),
        idle_expires_at: row.get("idle_expires_at"),
        abs_expires_at: row.get("abs_expires_at"),
    }))
}

/// Revoke a session (set `revoked_at = NOW()`).
///
/// # Errors
///
/// `AuthError::Db` on PG failure.
pub async fn revoke(conn: &Client, id: uuid::Uuid) -> Result<()> {
    conn.execute(
        "UPDATE auth.sessions SET revoked_at = NOW() WHERE id = $1",
        &[&id],
    )
    .await
    .map_err(|e| AuthError::Db(format!("sessions revoke: {e}")))?;
    Ok(())
}
