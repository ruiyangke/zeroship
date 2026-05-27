//! Per-origin app session store. The gateway maintains one row in
//! `auth.gateway_sessions` per authenticated browser session per hosted app.
//!
//! Lifecycle:
//!   - `create(...)` after successful OIDC callback exchange
//!   - `validate(...)` on every authenticated request; slides `idle_expires_at` forward
//!   - `revoke(...)` on /sign-out
//!
//! Hard limits per proposal §9.2:
//!   - 30 min sliding idle
//!   - 12 h absolute
//!
//! Wiring through `GateState` lives in P3-U5; this module is a leaf.

use compio_postgres::Client;
use uuid::Uuid;

use crate::error::{GatewayError, Result};

#[derive(Debug, Clone)]
pub struct AppSession {
    pub id: Uuid,
    pub user_id: String,
    pub app_id: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub email_verified: bool,
    pub idle_expires_at: chrono::DateTime<chrono::Utc>,
    pub abs_expires_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug)]
pub struct NewSession<'a> {
    pub user_id: &'a str,
    pub app_id: &'a str,
    pub email: Option<&'a str>,
    pub name: Option<&'a str>,
    pub avatar_url: Option<&'a str>,
    pub email_verified: bool,
}

/// Sliding idle timeout. After this many minutes of inactivity the
/// cookie stops validating; any successful `validate` call resets
/// `idle_expires_at` to `NOW() + IDLE_MINUTES`.
pub const IDLE_MINUTES: i64 = 30;

/// Hard absolute lifetime. After this many hours the session is dead
/// regardless of activity. Set on `create` and never bumped.
pub const ABSOLUTE_HOURS: i64 = 12;

/// Insert a new session row. Returns the created row (including
/// server-assigned id and timestamps).
///
/// # Errors
///
/// [`GatewayError::Db`] on PG failure or empty return.
pub async fn create(conn: &Client, params: &NewSession<'_>) -> Result<AppSession> {
    let rows = conn
        .query(
            "INSERT INTO auth.gateway_sessions \
                (user_id, app_id, email, name, avatar_url, email_verified, \
                 idle_expires_at, abs_expires_at) \
             VALUES ($1, $2, $3::citext, $4, $5, $6, \
                     NOW() + ($7::text || ' minutes')::interval, \
                     NOW() + ($8::text || ' hours')::interval) \
             RETURNING id, user_id, app_id, email::text AS email, name, avatar_url, \
                       email_verified, idle_expires_at, abs_expires_at",
            &[
                &params.user_id,
                &params.app_id,
                &params.email,
                &params.name,
                &params.avatar_url,
                &params.email_verified,
                &IDLE_MINUTES.to_string(),
                &ABSOLUTE_HOURS.to_string(),
            ],
        )
        .await
        .map_err(|e| GatewayError::Db(format!("gateway_sessions create: {e}")))?;

    let row = rows
        .first()
        .ok_or_else(|| GatewayError::Db("gateway_sessions create: empty return".into()))?;
    Ok(row_to_session(row))
}

/// Validate a session by id+app. Returns the row if valid, `None`
/// otherwise. Slides `idle_expires_at` forward on every successful
/// validation.
///
/// "Valid" means: row exists, `app_id` matches, not revoked, idle and
/// absolute expiries both in the future. The check + slide is one
/// atomic `UPDATE ... RETURNING` so concurrent requests can't race the
/// sliding window.
///
/// # Errors
///
/// [`GatewayError::Db`] on PG failure.
pub async fn validate(conn: &Client, id: Uuid, app_id: &str) -> Result<Option<AppSession>> {
    let rows = conn
        .query(
            "UPDATE auth.gateway_sessions \
             SET idle_expires_at = NOW() + ($3::text || ' minutes')::interval \
             WHERE id = $1 \
               AND app_id = $2 \
               AND revoked_at IS NULL \
               AND idle_expires_at > NOW() \
               AND abs_expires_at > NOW() \
             RETURNING id, user_id, app_id, email::text AS email, name, avatar_url, \
                       email_verified, idle_expires_at, abs_expires_at",
            &[&id, &app_id, &IDLE_MINUTES.to_string()],
        )
        .await
        .map_err(|e| GatewayError::Db(format!("gateway_sessions validate: {e}")))?;

    Ok(rows.first().map(row_to_session))
}

/// Revoke a session. Idempotent — calling on an already-revoked or
/// non-existent id is a no-op (no row update, no error).
///
/// # Errors
///
/// [`GatewayError::Db`] on PG failure.
pub async fn revoke(conn: &Client, id: Uuid) -> Result<()> {
    conn.execute(
        "UPDATE auth.gateway_sessions SET revoked_at = NOW() WHERE id = $1 AND revoked_at IS NULL",
        &[&id],
    )
    .await
    .map_err(|e| GatewayError::Db(format!("gateway_sessions revoke: {e}")))?;
    Ok(())
}

fn row_to_session(row: &compio_postgres::Row) -> AppSession {
    AppSession {
        id: row.get("id"),
        user_id: row.get("user_id"),
        app_id: row.get("app_id"),
        email: row.try_get("email").ok(),
        name: row.try_get("name").ok(),
        avatar_url: row.try_get("avatar_url").ok(),
        email_verified: row.get("email_verified"),
        idle_expires_at: row.get("idle_expires_at"),
        abs_expires_at: row.get("abs_expires_at"),
    }
}
