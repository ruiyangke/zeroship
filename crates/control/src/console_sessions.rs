//! Per-origin console session store. The control plane maintains one
//! row in `auth.console_sessions` per authenticated browser session on
//! `console.zeroship.ai`. Mirror of `crates/gateway/src/sessions.rs` —
//! same sliding/absolute-window discipline, minus `app_id` because
//! there is only one console origin.
//!
//! Lifecycle:
//!   - `create(...)` after successful OIDC callback exchange
//!   - `validate(...)` on every authenticated request; slides
//!     `idle_expires_at` forward
//!   - `revoke(...)` on /sign-out
//!
//! Hard limits per proposal §9.2:
//!   - 30 min sliding idle
//!   - 12 h absolute

use compio_postgres::Client;
use uuid::Uuid;
use zeroship_core::oidc_verify::TokenClaims;

/// Errors surfaced by the console session store.
#[derive(Debug, thiserror::Error)]
pub enum ConsoleSessionError {
    #[error("database: {0}")]
    Db(String),
}

pub type Result<T> = std::result::Result<T, ConsoleSessionError>;

#[derive(Debug, Clone)]
pub struct ConsoleSession {
    pub id: Uuid,
    pub user_id: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub email_verified: bool,
    pub idle_expires_at: chrono::DateTime<chrono::Utc>,
    pub abs_expires_at: chrono::DateTime<chrono::Utc>,
}

/// Sliding idle timeout. After this many minutes of inactivity the
/// cookie stops validating; any successful `validate` call resets
/// `idle_expires_at` to `NOW() + IDLE_MINUTES`.
pub const IDLE_MINUTES: i64 = 30;

/// Hard absolute lifetime. After this many hours the session is dead
/// regardless of activity. Set on `create` and never bumped.
pub const ABSOLUTE_HOURS: i64 = 12;

/// Insert a new session row from a freshly verified ID token.
///
/// # Errors
///
/// [`ConsoleSessionError::Db`] on PG failure or empty return.
pub async fn create(conn: &Client, claims: &TokenClaims) -> Result<ConsoleSession> {
    let user_id = Uuid::parse_str(&claims.sub).map_err(|e| {
        ConsoleSessionError::Db(format!("console_sessions create: invalid user_id: {e}"))
    })?;
    let email: Option<&str> = claims.email.as_deref();
    let name: Option<&str> = claims.name.as_deref();
    let avatar_url: Option<&str> = claims.picture.as_deref();
    let email_verified: bool = claims.email_verified.unwrap_or(false);

    let rows = conn
        .query(
            "INSERT INTO auth.console_sessions \
                (user_id, email, name, avatar_url, email_verified, \
                 idle_expires_at, abs_expires_at) \
             VALUES ($1, $2::citext, $3, $4, $5, \
                     NOW() + ($6::text || ' minutes')::interval, \
                     NOW() + ($7::text || ' hours')::interval) \
             RETURNING id, user_id, email::text AS email, name, avatar_url, \
                       email_verified, idle_expires_at, abs_expires_at",
            &[
                &user_id,
                &email,
                &name,
                &avatar_url,
                &email_verified,
                &IDLE_MINUTES.to_string(),
                &ABSOLUTE_HOURS.to_string(),
            ],
        )
        .await
        .map_err(|e| ConsoleSessionError::Db(format!("console_sessions create: {e}")))?;

    let row = rows
        .first()
        .ok_or_else(|| ConsoleSessionError::Db("console_sessions create: empty return".into()))?;
    Ok(row_to_session(row))
}

/// Validate a session by id. Returns the row if valid, `None`
/// otherwise. Slides `idle_expires_at` forward on every successful
/// validation. The check + slide is one atomic `UPDATE ... RETURNING`
/// so concurrent requests can't race the sliding window.
///
/// "Valid" means: row exists, not revoked, idle and absolute expiries
/// both in the future.
///
/// # Errors
///
/// [`ConsoleSessionError::Db`] on PG failure.
pub async fn validate(conn: &Client, id: Uuid) -> Result<Option<ConsoleSession>> {
    let rows = conn
        .query(
            "UPDATE auth.console_sessions \
             SET idle_expires_at = NOW() + ($2::text || ' minutes')::interval \
             WHERE id = $1 \
               AND revoked_at IS NULL \
               AND idle_expires_at > NOW() \
               AND abs_expires_at > NOW() \
             RETURNING id, user_id, email::text AS email, name, avatar_url, \
                       email_verified, idle_expires_at, abs_expires_at",
            &[&id, &IDLE_MINUTES.to_string()],
        )
        .await
        .map_err(|e| ConsoleSessionError::Db(format!("console_sessions validate: {e}")))?;

    Ok(rows.first().map(row_to_session))
}

/// Revoke a session. Idempotent — calling on an already-revoked or
/// non-existent id is a no-op (no row update, no error).
///
/// # Errors
///
/// [`ConsoleSessionError::Db`] on PG failure.
pub async fn revoke(conn: &Client, id: Uuid) -> Result<()> {
    conn.execute(
        "UPDATE auth.console_sessions SET revoked_at = NOW() WHERE id = $1 AND revoked_at IS NULL",
        &[&id],
    )
    .await
    .map_err(|e| ConsoleSessionError::Db(format!("console_sessions revoke: {e}")))?;
    Ok(())
}

/// Revoke every still-active console session belonging to `user_id`.
///
/// Returns the count of rows updated — useful for log/debug visibility
/// ("BCL logout revoked N console sessions for user X").
///
/// Used by the back-channel logout handler (`POST /oidc/backchannel-logout`,
/// Phase 7 U2): when hydra notifies us that a user signed out we revoke
/// every console session the user holds. Hydra emits a session id
/// (`sid`) but we don't currently correlate hydra's sid to
/// `console_sessions.id`, so revoke-by-user is the minimum-viable
/// semantics — same trade-off as the gateway's `revoke_all_for_user`.
///
/// Idempotent — already-revoked rows are skipped via the
/// `revoked_at IS NULL` filter.
///
/// # Errors
///
/// [`ConsoleSessionError::Db`] on PG failure.
pub async fn revoke_all_for_user(conn: &Client, user_id: &str) -> Result<u64> {
    let user_id = Uuid::parse_str(user_id).map_err(|e| {
        ConsoleSessionError::Db(format!("console_sessions revoke_all_for_user: invalid user_id: {e}"))
    })?;
    let affected = conn
        .execute(
            "UPDATE auth.console_sessions SET revoked_at = NOW() \
             WHERE user_id = $1 AND revoked_at IS NULL",
            &[&user_id],
        )
        .await
        .map_err(|e| ConsoleSessionError::Db(format!("console_sessions revoke_all_for_user: {e}")))?;
    Ok(affected)
}

fn row_to_session(row: &compio_postgres::Row) -> ConsoleSession {
    let user_id: Uuid = row.get("user_id");
    ConsoleSession {
        id: row.get("id"),
        user_id: user_id.to_string(),
        email: row.try_get("email").ok(),
        name: row.try_get("name").ok(),
        avatar_url: row.try_get("avatar_url").ok(),
        email_verified: row.get("email_verified"),
        idle_expires_at: row.get("idle_expires_at"),
        abs_expires_at: row.get("abs_expires_at"),
    }
}
