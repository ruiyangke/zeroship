//! Per-origin app session store. The gateway maintains one row in
//! `zeroship.gateway_sessions` per authenticated browser session per hosted app.
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
use crate::rls;

#[derive(Debug, Clone)]
pub struct AppSession {
    pub id: Uuid,
    pub user_id: String,
    /// OIDC OP session id (`sid`) from the ID token, used to correlate
    /// Back-Channel Logout tokens to the local RP session.
    pub sid: Option<String>,
    /// The app's stable UUID (`apps.id`). The `zeroship.gateway_sessions.app_id`
    /// column is UUID and bound natively — the canonical session key is the
    /// immutable app id, never the renameable subdomain slug.
    pub app_id: Uuid,
    pub email: Option<String>,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub email_verified: bool,
    /// OAuth scopes granted to this app for this user at consent (Slice 3,
    /// §1.4). Read off the same row the cookie path already loads, so the
    /// per-request `ZeroShip-User.scopes` needs no `zeroship.oauth_grants` join.
    pub granted_scopes: Vec<String>,
    /// The OIDC `auth_time` claim (the authenticating-event instant) carried
    /// onto the cookie session at create (BFF redesign §2.2 step 5b). Surfaced
    /// to the SPA via the `{ user }` projection (`/session`) and used by the
    /// step-up freshness gate (§5.3). `None` when the id_token omitted it.
    pub auth_time: Option<chrono::DateTime<chrono::Utc>>,
    /// The OIDC `amr` claim (authentication methods, e.g. `["pwd"]`) carried
    /// onto the cookie session at create (BFF redesign §2.2 step 5b). Surfaced
    /// to the SPA via the projection; the `amr ∋ "mfa"` step-up tightening is a
    /// later MFA-enablement slice (§5.3).
    pub amr: Vec<String>,
    pub idle_expires_at: chrono::DateTime<chrono::Utc>,
    pub abs_expires_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug)]
pub struct NewSession<'a> {
    pub user_id: &'a str,
    /// OIDC OP session id (`sid`) from the validated ID token, if present.
    pub sid: Option<&'a str>,
    /// The app's stable UUID (`apps.id`), bound natively into the UUID
    /// `app_id` column. Keyed on the immutable app id (the subdomain slug can
    /// be renamed), matching the live per-request dispatch arm.
    pub app_id: Uuid,
    pub email: Option<&'a str>,
    pub name: Option<&'a str>,
    pub avatar_url: Option<&'a str>,
    pub email_verified: bool,
    /// Granted scope set resolved at session-create from the consent grant.
    pub granted_scopes: &'a [String],
    /// The validated id_token's `auth_time` claim (unix seconds), if present.
    /// Persisted so the SPA projection + step-up gate read it off the gateway
    /// session row (the gateway path never touches `zeroship.users`).
    pub auth_time: Option<i64>,
    /// The validated id_token's `amr` claim (e.g. `["pwd"]`), if present.
    pub amr: &'a [String],
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
pub async fn create(conn: &mut Client, params: &NewSession<'_>) -> Result<AppSession> {
    let user_id = Uuid::parse_str(params.user_id)
        .map_err(|e| GatewayError::Db(format!("gateway_sessions create: invalid user_id: {e}")))?;
    let amr: Vec<String> = params.amr.to_vec();
    let tx = conn
        .transaction()
        .await
        .map_err(|e| GatewayError::Db(format!("gateway_sessions create begin: {e}")))?;
    rls::set_tenant_app(&tx, params.app_id).await?;
    let rows = tx
        .query(
            "INSERT INTO zeroship.gateway_sessions \
                (user_id, app_id, email, name, avatar_url, email_verified, \
                 granted_scopes, auth_time, amr, sid, idle_expires_at, abs_expires_at) \
             VALUES ($1, $2, $3::citext, $4, $5, $6, $9, \
                     CASE WHEN $10::bigint IS NULL THEN NULL \
                          ELSE to_timestamp($10::bigint) END, \
                     $11, $12, \
                     NOW() + ($7::text || ' minutes')::interval, \
                     NOW() + ($8::text || ' hours')::interval) \
             RETURNING id, user_id, app_id, email::text AS email, name, avatar_url, \
                       email_verified, granted_scopes, auth_time, amr, sid, \
                       idle_expires_at, abs_expires_at",
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
                &params.auth_time,
                &amr,
                &params.sid,
            ],
        )
        .await
        .map_err(|e| GatewayError::Db(format!("gateway_sessions create: {e}")))?;

    let session = {
        let row = rows
            .first()
            .ok_or_else(|| GatewayError::Db("gateway_sessions create: empty return".into()))?;
        row_to_session(row)
    };
    tx.commit()
        .await
        .map_err(|e| GatewayError::Db(format!("gateway_sessions create commit: {e}")))?;
    Ok(session)
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
pub async fn validate(conn: &mut Client, id: Uuid, app_id: Uuid) -> Result<Option<AppSession>> {
    let tx = conn
        .transaction()
        .await
        .map_err(|e| GatewayError::Db(format!("gateway_sessions validate begin: {e}")))?;
    rls::set_tenant_app(&tx, app_id).await?;
    let rows = tx
        .query(
            "UPDATE zeroship.gateway_sessions \
             SET idle_expires_at = NOW() + ($3::text || ' minutes')::interval \
             WHERE id = $1 \
               AND app_id = $2 \
               AND revoked_at IS NULL \
               AND idle_expires_at > NOW() \
               AND abs_expires_at > NOW() \
             RETURNING id, user_id, app_id, email::text AS email, name, avatar_url, \
                       email_verified, granted_scopes, auth_time, amr, sid, \
                       idle_expires_at, abs_expires_at",
            &[&id, &app_id, &IDLE_MINUTES.to_string()],
        )
        .await
        .map_err(|e| GatewayError::Db(format!("gateway_sessions validate: {e}")))?;

    let session = rows.first().map(row_to_session);
    tx.commit()
        .await
        .map_err(|e| GatewayError::Db(format!("gateway_sessions validate commit: {e}")))?;
    Ok(session)
}

// NOTE (RLS, changeset 0025): the former `revoke(conn, id)` (revoke-one by id,
// no app scope) and `revoke_all_for_user(conn, user_id)` (CROSS-TENANT
// `WHERE user_id=$1` across every app) were removed. Both are incompatible with
// the gateway's non-bypass `zeroship_gateway` role under FORCE-RLS:
//   - `revoke` had ZERO callers (dead code) and carried no `app_id` to set the
//     `zeroship.tenant_app` GUC, so it could never resolve a row.
//   - `revoke_all_for_user` was the legacy shared-`gateway`-client BCL path's
//     "all apps" fan-out. A single statement cannot span tenants under RLS, and
//     every real BCL now arrives with a per-app `aud` (→ `app_id`), so the
//     back-channel-logout handler's no-per-app-match branch is a logged no-op
//     rather than a cross-tenant nuke. See `backchannel_logout.rs`.

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
    conn: &mut Client,
    app_id: Uuid,
    user_id: &str,
) -> Result<u64> {
    let user_id = Uuid::parse_str(user_id).map_err(|e| {
        GatewayError::Db(format!(
            "gateway_sessions revoke_app_sessions_for_user: invalid user_id: {e}"
        ))
    })?;
    let tx = conn.transaction().await.map_err(|e| {
        GatewayError::Db(format!(
            "gateway_sessions revoke_app_sessions_for_user begin: {e}"
        ))
    })?;
    rls::set_tenant_app(&tx, app_id).await?;
    let affected = tx
        .execute(
            "UPDATE zeroship.gateway_sessions SET revoked_at = NOW() \
             WHERE user_id = $1 AND app_id = $2 AND revoked_at IS NULL",
            &[&user_id, &app_id],
        )
        .await
        .map_err(|e| {
            GatewayError::Db(format!("gateway_sessions revoke_app_sessions_for_user: {e}"))
        })?;
    tx.commit().await.map_err(|e| {
        GatewayError::Db(format!(
            "gateway_sessions revoke_app_sessions_for_user commit: {e}"
        ))
    })?;
    Ok(affected)
}

/// Revoke every live session for one OP `sid` at one app. When `sub` is
/// provided, it must match `gateway_sessions.user_id`; this prevents a malformed
/// token containing a valid sid plus a contradictory subject from killing a
/// different user's rows.
pub async fn revoke_app_sessions_for_sid(
    conn: &mut Client,
    app_id: Uuid,
    sid: &str,
    sub: Option<&str>,
) -> Result<Vec<Uuid>> {
    let parsed_sub = match sub {
        Some(sub) => Some(Uuid::parse_str(sub).map_err(|e| {
            GatewayError::Db(format!(
                "gateway_sessions revoke_app_sessions_for_sid: invalid user_id: {e}"
            ))
        })?),
        None => None,
    };
    let tx = conn.transaction().await.map_err(|e| {
        GatewayError::Db(format!(
            "gateway_sessions revoke_app_sessions_for_sid begin: {e}"
        ))
    })?;
    rls::set_tenant_app(&tx, app_id).await?;
    let rows = if let Some(user_id) = parsed_sub {
        tx.query(
            "WITH targets AS ( \
                 SELECT DISTINCT user_id \
                 FROM zeroship.gateway_sessions \
                 WHERE app_id = $1 AND sid = $2 AND user_id = $3 \
             ), revoked AS ( \
                 UPDATE zeroship.gateway_sessions \
                 SET revoked_at = NOW() \
                 WHERE app_id = $1 AND sid = $2 AND user_id = $3 AND revoked_at IS NULL \
                 RETURNING user_id \
             ) \
             SELECT user_id FROM targets \
             UNION \
             SELECT user_id FROM revoked",
            &[&app_id, &sid, &user_id],
        )
        .await
    } else {
        tx.query(
            "WITH targets AS ( \
                 SELECT DISTINCT user_id \
                 FROM zeroship.gateway_sessions \
                 WHERE app_id = $1 AND sid = $2 \
             ), revoked AS ( \
                 UPDATE zeroship.gateway_sessions \
                 SET revoked_at = NOW() \
                 WHERE app_id = $1 AND sid = $2 AND revoked_at IS NULL \
                 RETURNING user_id \
             ) \
             SELECT user_id FROM targets \
             UNION \
             SELECT user_id FROM revoked",
            &[&app_id, &sid],
        )
        .await
    }
    .map_err(|e| {
        GatewayError::Db(format!("gateway_sessions revoke_app_sessions_for_sid: {e}"))
    })?;
    tx.commit().await.map_err(|e| {
        GatewayError::Db(format!(
            "gateway_sessions revoke_app_sessions_for_sid commit: {e}"
        ))
    })?;

    let mut users = Vec::new();
    for row in rows {
        let user_id: Uuid = row.get("user_id");
        if !users.contains(&user_id) {
            users.push(user_id);
        }
    }
    Ok(users)
}

fn row_to_session(row: &compio_postgres::Row) -> AppSession {
    let user_id: Uuid = row.get("user_id");
    AppSession {
        id: row.get("id"),
        user_id: user_id.to_string(),
        sid: row.try_get("sid").ok().flatten(),
        app_id: row.get("app_id"),
        email: row.try_get("email").ok(),
        name: row.try_get("name").ok(),
        avatar_url: row.try_get("avatar_url").ok(),
        email_verified: row.get("email_verified"),
        granted_scopes: row.try_get("granted_scopes").unwrap_or_default(),
        auth_time: row.try_get("auth_time").ok(),
        amr: row.try_get("amr").unwrap_or_default(),
        idle_expires_at: row.get("idle_expires_at"),
        abs_expires_at: row.get("abs_expires_at"),
    }
}
