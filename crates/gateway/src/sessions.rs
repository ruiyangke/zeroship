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
    /// OAuth scopes granted to this app for this user at consent (Slice 3,
    /// §1.4). Read off the same row the cookie path already loads, so the
    /// per-request `ZeroShip-User.scopes` needs no `control.oauth_grants` join.
    pub granted_scopes: Vec<String>,
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
    /// Granted scope set resolved at session-create from the consent grant.
    pub granted_scopes: &'a [String],
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
    let user_id = Uuid::parse_str(params.user_id)
        .map_err(|e| GatewayError::Db(format!("gateway_sessions create: invalid user_id: {e}")))?;
    let rows = conn
        .query(
            "INSERT INTO auth.gateway_sessions \
                (user_id, app_id, email, name, avatar_url, email_verified, \
                 granted_scopes, idle_expires_at, abs_expires_at) \
             VALUES ($1, $2, $3::citext, $4, $5, $6, $9, \
                     NOW() + ($7::text || ' minutes')::interval, \
                     NOW() + ($8::text || ' hours')::interval) \
             RETURNING id, user_id, app_id, email::text AS email, name, avatar_url, \
                       email_verified, granted_scopes, idle_expires_at, abs_expires_at",
            &[
                &user_id,
                &params.app_id,
                &params.email,
                &params.name,
                &params.avatar_url,
                &params.email_verified,
                &IDLE_MINUTES.to_string(),
                &ABSOLUTE_HOURS.to_string(),
                &params.granted_scopes,
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
                       email_verified, granted_scopes, idle_expires_at, abs_expires_at",
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

/// Revoke every still-active session belonging to `user_id`.
///
/// Touches every row across every per-origin app session. Returns the
/// count of rows updated — useful for log/debug visibility ("BCL
/// logout revoked N sessions for user X").
///
/// Used by the back-channel logout handler (`POST /oidc/backchannel-logout`,
/// Phase 7 U1.2): when hydra notifies us that a user signed out we revoke
/// every gateway session the user has at any hosted app. Coarser than
/// per-session revocation but safer — hydra emits a session id (`sid`) but
/// we don't currently correlate hydra's sid to our `gateway_sessions.id`,
/// so revoke-by-user is the minimum-viable semantics.
///
/// Idempotent — already-revoked rows are skipped via the
/// `revoked_at IS NULL` filter.
///
/// # Errors
///
/// [`GatewayError::Db`] on PG failure.
pub async fn revoke_all_for_user(conn: &Client, user_id: &str) -> Result<u64> {
    let user_id = Uuid::parse_str(user_id).map_err(|e| {
        GatewayError::Db(format!("gateway_sessions revoke_all_for_user: invalid user_id: {e}"))
    })?;
    let affected = conn
        .execute(
            "UPDATE auth.gateway_sessions SET revoked_at = NOW() \
             WHERE user_id = $1 AND revoked_at IS NULL",
            &[&user_id],
        )
        .await
        .map_err(|e| GatewayError::Db(format!("gateway_sessions revoke_all_for_user: {e}")))?;
    Ok(affected)
}

/// Revoke every live session for `user_id` **at one app** (`app_id`, the app
/// subdomain). Returns the count of rows updated.
///
/// Per-app back-channel logout (auth-sdk Slice 1d, spec §1.2): each per-app
/// OAuth client registers its own `backchannel_logout_uri` with its own `aud`
/// (= the per-app `client_id`). The BCL handler resolves the `app_id` from that
/// `aud` and revokes only **that app's** sessions for the subject — not every
/// app the subject is signed into. A true platform-wide "log out of every app"
/// is a separate, explicit control-plane action.
///
/// Idempotent — already-revoked rows are skipped via the `revoked_at IS NULL`
/// filter.
///
/// # Errors
/// [`GatewayError::Db`] on PG failure (including an unparseable `user_id`).
pub async fn revoke_app_sessions_for_user(
    conn: &Client,
    app_id: &str,
    user_id: &str,
) -> Result<u64> {
    let user_id = Uuid::parse_str(user_id).map_err(|e| {
        GatewayError::Db(format!(
            "gateway_sessions revoke_app_sessions_for_user: invalid user_id: {e}"
        ))
    })?;
    let affected = conn
        .execute(
            "UPDATE auth.gateway_sessions SET revoked_at = NOW() \
             WHERE user_id = $1 AND app_id = $2 AND revoked_at IS NULL",
            &[&user_id, &app_id],
        )
        .await
        .map_err(|e| {
            GatewayError::Db(format!("gateway_sessions revoke_app_sessions_for_user: {e}"))
        })?;
    Ok(affected)
}

fn row_to_session(row: &compio_postgres::Row) -> AppSession {
    let user_id: Uuid = row.get("user_id");
    AppSession {
        id: row.get("id"),
        user_id: user_id.to_string(),
        app_id: row.get("app_id"),
        email: row.try_get("email").ok(),
        name: row.try_get("name").ok(),
        avatar_url: row.try_get("avatar_url").ok(),
        email_verified: row.get("email_verified"),
        granted_scopes: row.try_get("granted_scopes").unwrap_or_default(),
        idle_expires_at: row.get("idle_expires_at"),
        abs_expires_at: row.get("abs_expires_at"),
    }
}
