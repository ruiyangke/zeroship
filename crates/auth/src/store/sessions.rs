//! `zeroship.idp_sessions` CRUD — the `IdP` login session at `auth.zeroship.ai`.

use compio_postgres::Client;

use crate::error::{AuthError, Result};

#[derive(Debug, Clone)]
pub struct Session {
    pub id: uuid::Uuid,
    pub user_id: uuid::Uuid,
    pub auth_method: String,
    pub amr: Vec<String>,
    pub acr: Option<String>,
    pub credential_version: i64,
    pub idle_expires_at: chrono::DateTime<chrono::Utc>,
    pub abs_expires_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug)]
pub struct CreateSession<'a> {
    pub user_id: uuid::Uuid,
    pub auth_method: &'a str,
    pub amr: Vec<String>,
    pub acr: Option<&'a str>,
    pub expected_credential_version: Option<i64>,
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
            "INSERT INTO zeroship.idp_sessions \
                (user_id, auth_method, amr, acr, credential_version, idle_expires_at, abs_expires_at) \
             SELECT id, $2, $3, $4, credential_version, \
                    NOW() + ($5::text || ' minutes')::interval, \
                    NOW() + ($6::text || ' hours')::interval \
             FROM zeroship.users \
             WHERE id = $1 \
               AND ($7::BIGINT IS NULL OR credential_version = $7::BIGINT) \
             RETURNING id, user_id, auth_method, amr, acr, credential_version, \
                       idle_expires_at, abs_expires_at",
            &[
                &params.user_id,
                &params.auth_method,
                &params.amr,
                &params.acr,
                &params.idle_minutes.to_string(),
                &params.absolute_hours.to_string(),
                &params.expected_credential_version,
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
        credential_version: row.get("credential_version"),
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
            "UPDATE zeroship.idp_sessions \
             SET idle_expires_at = NOW() + ($2::text || ' minutes')::interval \
             FROM zeroship.users \
             WHERE zeroship.idp_sessions.id = $1 \
               AND zeroship.users.id = zeroship.idp_sessions.user_id \
               AND zeroship.idp_sessions.credential_version = zeroship.users.credential_version \
               AND zeroship.idp_sessions.revoked_at IS NULL \
               AND zeroship.idp_sessions.idle_expires_at > NOW() \
               AND zeroship.idp_sessions.abs_expires_at > NOW() \
             RETURNING zeroship.idp_sessions.id, zeroship.idp_sessions.user_id, auth_method, amr, acr, \
                       zeroship.idp_sessions.credential_version, idle_expires_at, abs_expires_at",
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
        credential_version: row.get("credential_version"),
        idle_expires_at: row.get("idle_expires_at"),
        abs_expires_at: row.get("abs_expires_at"),
    }))
}

/// Which session table a [`SessionSummary`] / revoke targets.
///
/// The IdP login session (`zeroship.idp_sessions`) is the single SSO session
/// at `auth.zeroship.ai`; an app session (`zeroship.gateway_sessions`) is one
/// per hosted app the user has signed into. The two tables have distinct
/// shapes (only the gateway row carries an `app_id`), so the kind is the
/// discriminator the list/revoke API exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionKind {
    /// `zeroship.idp_sessions` — the IdP SSO session.
    Idp,
    /// `zeroship.gateway_sessions` — a per-app cookie session.
    App,
}

/// A user-facing summary of one active session, unioning the two session
/// tables. Only columns that actually exist are surfaced:
///
///   - `created_at`: `idp_sessions.auth_time` (the authenticating-event
///     instant) for IdP rows; `gateway_sessions.issued_at` for app rows.
///     Neither table records device / IP / user-agent, so none are exposed.
///   - `last_seen_at`: `idle_expires_at` slides forward `IDLE_MINUTES` on
///     every successful `validate`, so it is the most-recent-activity proxy
///     both tables share. (Subtracting the idle window would recover the last
///     activity instant; we surface the raw column and let the UI decide.)
///   - `app_id`: present only for [`SessionKind::App`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionSummary {
    pub id: uuid::Uuid,
    pub kind: SessionKind,
    /// The hosted app's id, for [`SessionKind::App`] rows only.
    pub app_id: Option<uuid::Uuid>,
    /// `idp_sessions.auth_time` / `gateway_sessions.issued_at`.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Sliding `idle_expires_at` — the last-activity proxy (see struct docs).
    pub last_seen_at: chrono::DateTime<chrono::Utc>,
    /// Absolute expiry (`abs_expires_at`); the session is dead after this
    /// regardless of activity.
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// List a user's currently-active sessions across BOTH session tables
/// (ISS-10). "Active" means: `revoked_at IS NULL` AND both `idle_expires_at`
/// and `abs_expires_at` are still in the future. Results are ordered
/// newest-first by `created_at`.
///
/// The two `SELECT`s are UNIONed: IdP rows (no `app_id`) and per-app gateway
/// rows (carrying `app_id`). Listing is read-only, so it needs only the
/// `SELECT` privilege the `zeroship_auth` role holds on both tables — and the
/// role is `BYPASSRLS`, so the gateway table's per-tenant policy does not
/// apply (this is a cross-tenant "all your apps" view by design, scoped to the
/// single authenticated `user_id`).
///
/// # Errors
///
/// `AuthError::Db` on PG failure.
pub async fn list_by_user(conn: &Client, user_id: uuid::Uuid) -> Result<Vec<SessionSummary>> {
    let rows = conn
        .query(
            "SELECT id, 'idp' AS kind, NULL::uuid AS app_id, \
                    auth_time AS created_at, idle_expires_at AS last_seen_at, \
                    abs_expires_at AS expires_at \
             FROM zeroship.idp_sessions \
             WHERE user_id = $1 \
               AND revoked_at IS NULL \
               AND idle_expires_at > NOW() \
               AND abs_expires_at > NOW() \
             UNION ALL \
             SELECT id, 'app' AS kind, app_id, \
                    issued_at AS created_at, idle_expires_at AS last_seen_at, \
                    abs_expires_at AS expires_at \
             FROM zeroship.gateway_sessions \
             WHERE user_id = $1 \
               AND revoked_at IS NULL \
               AND idle_expires_at > NOW() \
               AND abs_expires_at > NOW() \
             ORDER BY created_at DESC",
            &[&user_id],
        )
        .await
        .map_err(|e| AuthError::Db(format!("sessions list_by_user: {e}")))?;
    Ok(rows
        .iter()
        .map(|row| {
            let kind = if row.get::<_, &str>("kind") == "app" {
                SessionKind::App
            } else {
                SessionKind::Idp
            };
            SessionSummary {
                id: row.get("id"),
                kind,
                app_id: row.try_get("app_id").ok(),
                created_at: row.get("created_at"),
                last_seen_at: row.get("last_seen_at"),
                expires_at: row.get("expires_at"),
            }
        })
        .collect())
}

/// Revoke ONE session belonging to `user_id` (ISS-10 single-session revoke).
///
/// SECURITY — the IDOR guard: every statement filters on BOTH
/// `id = $session_id AND user_id = $user_id`. Passing a `session_id` that
/// belongs to a different user matches zero rows and revokes nothing, so a
/// caller can never revoke another user's session. Returns `true` iff exactly
/// the caller's own (still-live) session was revoked.
///
/// Table mechanics differ by kind because the `zeroship_auth` role's grants
/// differ (changeset 0025):
///
///   - [`SessionKind::Idp`] — `UPDATE ... SET revoked_at = NOW()` (the role
///     holds `UPDATE` on `idp_sessions`), mirroring [`revoke`].
///   - [`SessionKind::App`] — `DELETE` (the role holds `SELECT, DELETE` but
///     NOT `UPDATE` on `gateway_sessions`; the password-reset cascade in
///     `ui/reset.rs` deletes gateway rows for the same reason). A deleted row
///     is, definitionally, no longer active — same observable effect as a
///     `revoked_at` stamp for the list view. `BYPASSRLS` lets the single
///     statement reach the row without the per-app tenant GUC.
///
/// The `revoked_at IS NULL` filter on the IdP arm makes re-revoking an
/// already-revoked session a no-op (`false`). For the gateway arm the row is
/// simply gone after the first delete, so a second call also returns `false`.
///
/// # Errors
///
/// `AuthError::Db` on PG failure.
pub async fn revoke_one_for_user(
    conn: &Client,
    user_id: uuid::Uuid,
    session_id: uuid::Uuid,
    kind: SessionKind,
) -> Result<bool> {
    let affected = match kind {
        SessionKind::Idp => conn
            .execute(
                "UPDATE zeroship.idp_sessions SET revoked_at = NOW() \
                 WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL",
                &[&session_id, &user_id],
            )
            .await
            .map_err(|e| AuthError::Db(format!("sessions revoke_one_for_user idp: {e}")))?,
        SessionKind::App => conn
            .execute(
                "DELETE FROM zeroship.gateway_sessions \
                 WHERE id = $1 AND user_id = $2",
                &[&session_id, &user_id],
            )
            .await
            .map_err(|e| AuthError::Db(format!("sessions revoke_one_for_user app: {e}")))?,
    };
    Ok(affected > 0)
}

/// Revoke a session (set `revoked_at = NOW()`).
///
/// # Errors
///
/// `AuthError::Db` on PG failure.
pub async fn revoke(conn: &Client, id: uuid::Uuid) -> Result<()> {
    conn.execute(
        "UPDATE zeroship.idp_sessions SET revoked_at = NOW() WHERE id = $1",
        &[&id],
    )
    .await
    .map_err(|e| AuthError::Db(format!("sessions revoke: {e}")))?;
    Ok(())
}
