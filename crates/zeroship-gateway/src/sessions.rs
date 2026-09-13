//! Per-origin app session store. The gateway maintains one row in
//! `zeroship.gateway_sessions` per authenticated browser session per hosted app.
//!
//! Lifecycle:
//!   - `create(...)` after a successful OIDC exchange or anchor refresh
//!   - the revoke helpers on signout and back-channel logout
//!
//! There is no read-and-check entry point: request authentication never used
//! one, and the `validate(...)` that offered it is deleted. See the note beside
//! the revoke helpers for what enforces revocation instead.
//!
//! Stored-row lifetime limits, both stamped at `create` and never bumped:
//!   - 30 min idle
//!   - 12 h absolute
//!
//! This module is a leaf; request handlers own orchestration and pass database
//! clients into these helpers.

use compio_postgres::Client;
use uuid::Uuid;

use zeroship_core::app_id::AppId;
use zeroship_core::user_id::UserId;

use crate::error::{GatewayError, Result};
use crate::rls;

#[derive(Debug, Clone)]
pub struct AppSession {
    pub id: Uuid,
    pub user_id: UserId,
    /// OIDC OP session id (`sid`) from the ID token, used to correlate
    /// Back-Channel Logout tokens to the local RP session.
    pub sid: Option<String>,
    /// The app's stable typed id (`apps.id`). The `zeroship.gateway_sessions
    /// .app_id` column is `text` and holds `app_id.as_str()` — the canonical
    /// session key is the immutable app id, never the renameable subdomain
    /// slug.
    pub app_id: AppId,
    pub email: Option<String>,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub email_verified: bool,
    /// OAuth scopes granted to this app for this user at consent. Persisted in
    /// the audit row and returned at creation so the signed cookie can carry
    /// them; per-request auth needs neither this row nor an OAuth-grants join.
    pub granted_scopes: Vec<String>,
    /// The OIDC `auth_time` claim (the authenticating-event instant). Stored in
    /// the audit row, copied into the signed session cookie after creation, and
    /// preserved across anchor refresh. `None` when the ID token omitted it.
    pub auth_time: Option<chrono::DateTime<chrono::Utc>>,
    /// The OIDC `amr` claim (authentication methods, e.g. `["pwd"]`). Stored in
    /// the audit row, copied into the signed session cookie after creation, and
    /// preserved across anchor refresh.
    pub amr: Vec<String>,
    pub idle_expires_at: chrono::DateTime<chrono::Utc>,
    pub abs_expires_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug)]
pub struct NewSession<'a> {
    pub user_id: &'a UserId,
    /// OIDC OP session id (`sid`) from the validated ID token, if present.
    pub sid: Option<&'a str>,
    /// The app's stable typed id (`apps.id`), bound into the `text` `app_id`
    /// column as `app_id.as_str()`. Keyed on the immutable app id (the
    /// subdomain slug can be renamed), matching the live per-request
    /// dispatch arm.
    pub app_id: &'a AppId,
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

/// Idle window stamped on the audit row at `create`, as `NOW() + IDLE_MINUTES`.
///
/// IT NO LONGER SLIDES. It used to be the sliding half of the pair, reset by
/// each successful `validate` call - and `validate` had no production caller
/// and is deleted, so nothing bumps this. The row simply carries the window it
/// was created with. See the deletion note further down for where revocation is
/// actually enforced; this constant is a record on an audit row, not a fence.
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
                &params.user_id.as_str(),
                &params.app_id.as_str(),
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
        row_to_session(row)?
    };
    tx.commit()
        .await
        .map_err(|e| GatewayError::Db(format!("gateway_sessions create commit: {e}")))?;
    Ok(session)
}

// NOTE (validate, deleted): a `validate(conn, id, app_id)` used to live here.
// It read the row, checked `revoked_at IS NULL` and both expiries, and slid
// `idle_expires_at` forward in the same `UPDATE ... RETURNING`. It had NO
// production caller, and its every caller was a gateway integration test using
// it as an oracle to assert that a revoke had written `revoked_at`. Those tests
// now read the row themselves.
//
// THE REASON THIS NOTE EXISTS is that the deletion invites exactly the wrong
// conclusion. "Valid means not revoked", on a function named `validate`, reads
// like THE revocation gate - so finding it uncalled reads like the gate is
// missing, and the next author rebuilds it. THE ABSENCE OF THIS MECHANISM IS
// NOT THE ABSENCE OF THE PROPERTY.
//
// Where the property actually lives: since slice R1b the per-request identity
// check verifies the signed stateless cookie locally and gates it on the
// per-app family marker - `is_family_revoked_since(client_id, pws_, iat)`, an
// uncached `SELECT EXISTS` on every request, in
// `crate::router::auth::resolve_app_session_user_header_inner`. This table is
// the AUDIT and visibility record; the family marker is the enforcement truth.
// Both revoke paths below (`revoke_app_sessions_for_sid` and
// `revoke_app_sessions_for_user`) feed `teardown_per_app_user`, which records
// the marker the hot path reads.
//
// The idle window is therefore set once, at `create`, and nothing bumps it -
// see [`IDLE_MINUTES`].

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

/// Revoke every live session for `user_id` **at one app**, identified by its
/// stable `app_id`. Returns the count of rows updated.
///
/// For per-app back-channel logout, each per-app
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
/// [`GatewayError::Db`] on PG failure.
pub async fn revoke_app_sessions_for_user(
    conn: &mut Client,
    app_id: &AppId,
    user_id: &UserId,
) -> Result<u64> {
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
            &[&user_id.as_str(), &app_id.as_str()],
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

/// Return the most recent non-null OP `sid` previously recorded for this
/// `(app_id, user_id)` session family.
///
/// Reload-recovery refresh grants do not always return an ID token, and the
/// access JWT does not carry `sid`. The original login session row is therefore
/// the gateway-side durable source of the OP session id when re-writing the
/// audit/revocation row during `?mint=1` rotation.
pub async fn latest_sid_for_user(
    conn: &mut Client,
    app_id: &AppId,
    user_id: &UserId,
) -> Result<Option<String>> {
    let tx = conn.transaction().await.map_err(|e| {
        GatewayError::Db(format!("gateway_sessions latest_sid_for_user begin: {e}"))
    })?;
    rls::set_tenant_app(&tx, app_id).await?;
    let rows = tx
        .query(
            "SELECT sid \
             FROM zeroship.gateway_sessions \
             WHERE user_id = $1 AND app_id = $2 AND sid IS NOT NULL \
             ORDER BY issued_at DESC \
             LIMIT 1",
            &[&user_id.as_str(), &app_id.as_str()],
        )
        .await
        .map_err(|e| GatewayError::Db(format!("gateway_sessions latest_sid_for_user: {e}")))?;
    let sid = rows.first().and_then(|row| row.try_get("sid").ok());
    tx.commit().await.map_err(|e| {
        GatewayError::Db(format!(
            "gateway_sessions latest_sid_for_user commit: {e}"
        ))
    })?;
    Ok(sid)
}

/// Revoke every live session for one OP `sid` at one app. When `sub` is
/// provided, it must match `gateway_sessions.user_id`; this prevents a malformed
/// token containing a valid sid plus a contradictory subject from killing a
/// different user's rows.
pub async fn revoke_app_sessions_for_sid(
    conn: &mut Client,
    app_id: &AppId,
    sid: &str,
    sub: Option<&UserId>,
) -> Result<Vec<UserId>> {
    let tx = conn.transaction().await.map_err(|e| {
        GatewayError::Db(format!(
            "gateway_sessions revoke_app_sessions_for_sid begin: {e}"
        ))
    })?;
    rls::set_tenant_app(&tx, app_id).await?;
    let rows = if let Some(user_id) = sub {
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
            &[&app_id.as_str(), &sid, &user_id.as_str()],
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
            &[&app_id.as_str(), &sid],
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
        let user_id: &str = row.get("user_id");
        let user_id = UserId::parse(user_id).map_err(|e| {
            GatewayError::Db(format!(
                "gateway_sessions revoke_app_sessions_for_sid: invalid stored user_id: {e}"
            ))
        })?;
        if !users.contains(&user_id) {
            users.push(user_id);
        }
    }
    Ok(users)
}

fn row_to_session(row: &compio_postgres::Row) -> Result<AppSession> {
    let user_id: &str = row.get("user_id");
    let user_id = UserId::parse(user_id)
        .map_err(|e| GatewayError::Db(format!("gateway_sessions create: invalid user_id: {e}")))?;
    let app_id: String = row.get("app_id");
    let app_id = AppId::parse(&app_id)
        .map_err(|e| GatewayError::Db(format!("gateway_sessions create: invalid app_id: {e}")))?;
    Ok(AppSession {
        id: row.get("id"),
        user_id,
        sid: row.try_get("sid").ok().flatten(),
        app_id,
        email: row.try_get("email").ok(),
        name: row.try_get("name").ok(),
        avatar_url: row.try_get("avatar_url").ok(),
        email_verified: row.get("email_verified"),
        granted_scopes: row.try_get("granted_scopes").unwrap_or_default(),
        auth_time: row.try_get("auth_time").ok(),
        amr: row.try_get("amr").unwrap_or_default(),
        idle_expires_at: row.get("idle_expires_at"),
        abs_expires_at: row.get("abs_expires_at"),
    })
}
