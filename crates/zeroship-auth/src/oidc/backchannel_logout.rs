//! OIDC Back-Channel Logout 1.0 OP emission.
//!
//! The self-contained OP records which RP clients received an ID token for an
//! `idp_sessions` row, then POSTs a signed `logout_token` to each registered
//! `backchannel_logout_uri` when that OP session ends.

use std::time::Duration;

use compio_postgres::{Client, GenericClient};
use http::Method;
use uuid::Uuid;
use zeroship_core::app_id::AppId;
use zeroship_core::user_id::UserId;

use crate::error::{AuthError, Result};
use crate::oidc::{Issuer, LogoutTokenMint};

const BCL_POST_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogoutEmissionReport {
    pub attempted: usize,
    pub delivered: usize,
}

#[derive(Debug, Clone)]
struct RelyingPartySession {
    client_id: String,
    backchannel_logout_uri: String,
    /// The OP session id to scope the logout to. `None` when the RP-local
    /// session carries no recorded `sid`; the logout token then names only the
    /// subject and the RP falls back to revoking every session that subject
    /// holds at this client.
    sid: Option<String>,
    sub: String,
}

/// Remember that an RP received an ID token for this OP session.
///
/// Clients without a `backchannel_logout_uri` are intentionally ignored: they
/// did not register BCL support, so there is nowhere compliant to send a logout
/// token.
pub async fn record_rp_participation(
    db: &(impl GenericClient + ?Sized),
    user_id: &UserId,
    sid: &str,
    client_id: &str,
    sub: &str,
    backchannel_logout_uri: Option<&str>,
) -> Result<()> {
    if backchannel_logout_uri
        .map(str::trim)
        .is_none_or(str::is_empty)
    {
        return Ok(());
    }
    let idp_session_id = Uuid::parse_str(sid)
        .map_err(|e| AuthError::Internal(format!("BCL sid is not an idp session UUID: {e}")))?;
    if client_id.trim().is_empty() {
        return Err(AuthError::Internal("BCL client_id is empty".into()));
    }
    if sub.trim().is_empty() {
        return Err(AuthError::Internal("BCL subject is empty".into()));
    }

    db.execute(
        "INSERT INTO zeroship.oidc_session_clients \
            (idp_session_id, user_id, client_id, sid, sub, last_seen_at) \
         VALUES ($1, $2, $3, $4, $5, NOW()) \
         ON CONFLICT (idp_session_id, client_id) DO UPDATE SET \
            user_id = EXCLUDED.user_id, \
            sid = EXCLUDED.sid, \
            sub = EXCLUDED.sub, \
            last_seen_at = NOW()",
        &[&idp_session_id, &user_id.as_str(), &client_id, &sid, &sub],
    )
    .await
    .map_err(|e| AuthError::Db(format!("record BCL RP participation: {e}")))?;

    Ok(())
}

/// Emit BCL logout tokens for every RP remembered for one OP session id.
pub async fn emit_for_session(
    db: &Client,
    issuer: &Issuer,
    idp_session_id: Uuid,
) -> Result<LogoutEmissionReport> {
    let rps = load_rps_for_session(db, idp_session_id).await?;
    emit_to_rps(db, issuer, rps).await
}

/// Emit a BCL logout token to the ONE relying party that serves `app_id`.
///
/// This is the per-app peer of [`emit_for_session`], and the reason it exists
/// is that an app session is not stored where its validity is decided. The OP
/// side of an app session is a `zeroship.gateway_sessions` row, which the
/// gateway keeps as an audit record and does NOT read when authenticating a
/// request: the request path verifies a locally-signed 15-minute cookie against
/// the `(client_id, sub)` family marker, and a 30-day server-held anchor
/// silently re-signs that cookie when it lapses. Deleting the OP-side row
/// therefore ends nothing. The RP's back-channel-logout receiver is what writes
/// the family marker and tears the anchor down, so a per-app revoke has to
/// reach it, exactly as an IdP-session revoke already does.
///
/// The `sub` is rebuilt with the same rule the token endpoint used when it
/// recorded the RP's participation (`user_id` for a brokered client, the
/// sector-pairwise subject otherwise), so the RP sees the subject it knows.
///
/// **The logout token names a subject and NOT a `sid`, and that is exact rather
/// than lossy.** A `sid` would assert that an OP session ended, and none has:
/// the user is still signed in at the OP; only their session at this one app was
/// revoked. It would also promise a narrowing the platform cannot deliver: the
/// RP enforces revocation through a `(client_id, sub)` family marker, which has
/// no per-device dimension, so ending one of a subject's app sessions at a
/// client ends all of them. Say the scope that actually applies.
///
/// # Errors
///
/// `AuthError::Db` on PG failure. A client with no registered
/// `backchannel_logout_uri` is not an error: it reports zero attempts.
pub async fn emit_for_app_session(
    db: &Client,
    issuer: &Issuer,
    app_id: &AppId,
    user_id: &UserId,
) -> Result<LogoutEmissionReport> {
    let rows = db
        .query(
            "SELECT oc.client_id, oc.backchannel_logout_uri, oc.brokered, \
                    COALESCE(aoc.sector_identifier, oc.client_id) AS sector_identifier \
             FROM zeroship.app_oauth_clients aoc \
             JOIN zeroship.oauth_clients oc ON oc.client_id = aoc.client_id \
             WHERE aoc.app_id = $1 \
               AND oc.backchannel_logout_uri IS NOT NULL",
            &[&app_id.as_str()],
        )
        .await
        .map_err(|e| AuthError::Db(format!("load BCL RP for app: {e}")))?;

    let subject = user_id.as_str().to_string();
    let rps: Vec<RelyingPartySession> = rows
        .iter()
        .filter_map(|row| {
            let uri: Option<String> = row.try_get("backchannel_logout_uri").ok().flatten();
            let uri = uri?.trim().to_string();
            if uri.is_empty() {
                return None;
            }
            let brokered: bool = row.try_get("brokered").unwrap_or(false);
            let sector: String = row.get("sector_identifier");
            let sub = if brokered {
                subject.clone()
            } else {
                issuer.pairwise_subject(user_id, &sector)
            };
            Some(RelyingPartySession {
                client_id: row.get("client_id"),
                backchannel_logout_uri: uri,
                sid: None,
                sub,
            })
        })
        .collect();

    emit_to_rps(db, issuer, rps).await
}

/// Emit BCL logout tokens for every RP remembered for all sessions of a user.
pub async fn emit_for_user(
    db: &Client,
    issuer: &Issuer,
    user_id: &UserId,
) -> Result<LogoutEmissionReport> {
    let rps = load_rps_for_user(db, user_id).await?;
    emit_to_rps(db, issuer, rps).await
}

async fn load_rps_for_session(db: &Client, idp_session_id: Uuid) -> Result<Vec<RelyingPartySession>> {
    let rows = db
        .query(
            "SELECT osc.client_id, osc.sid, osc.sub, oc.backchannel_logout_uri \
             FROM zeroship.oidc_session_clients osc \
             JOIN zeroship.oauth_clients oc ON oc.client_id = osc.client_id \
             WHERE osc.idp_session_id = $1 \
               AND oc.backchannel_logout_uri IS NOT NULL",
            &[&idp_session_id],
        )
        .await
        .map_err(|e| AuthError::Db(format!("load BCL RPs for session: {e}")))?;
    Ok(rows
        .iter()
        .filter_map(row_to_rp)
        .collect())
}

async fn load_rps_for_user(db: &Client, user_id: &UserId) -> Result<Vec<RelyingPartySession>> {
    let rows = db
        .query(
            "SELECT osc.client_id, osc.sid, osc.sub, oc.backchannel_logout_uri \
             FROM zeroship.oidc_session_clients osc \
             JOIN zeroship.oauth_clients oc ON oc.client_id = osc.client_id \
             WHERE osc.user_id = $1 \
               AND oc.backchannel_logout_uri IS NOT NULL",
            &[&user_id.as_str()],
        )
        .await
        .map_err(|e| AuthError::Db(format!("load BCL RPs for user: {e}")))?;
    Ok(rows
        .iter()
        .filter_map(row_to_rp)
        .collect())
}

fn row_to_rp(row: &compio_postgres::Row) -> Option<RelyingPartySession> {
    let uri: Option<String> = row.try_get("backchannel_logout_uri").ok().flatten();
    let uri = uri?.trim().to_string();
    if uri.is_empty() {
        return None;
    }
    Some(RelyingPartySession {
        client_id: row.get("client_id"),
        backchannel_logout_uri: uri,
        sid: Some(row.get("sid")),
        sub: row.get("sub"),
    })
}

async fn emit_to_rps(
    db: &Client,
    issuer: &Issuer,
    rps: Vec<RelyingPartySession>,
) -> Result<LogoutEmissionReport> {
    let client = cyper::Client::new();
    let mut report = LogoutEmissionReport {
        attempted: rps.len(),
        delivered: 0,
    };

    for rp in rps {
        let token = match issuer
            .issue_logout_token(
                db,
                &LogoutTokenMint {
                    client_id: &rp.client_id,
                    sub: Some(&rp.sub),
                    sid: rp.sid.as_deref(),
                    ttl_secs: None,
                },
            )
            .await
        {
            Ok(token) => token,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    client_id = %rp.client_id,
                    "BCL logout_token signing failed"
                );
                continue;
            }
        };
        match post_logout_token(&client, &rp.backchannel_logout_uri, &token).await {
            Ok(()) => report.delivered += 1,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    client_id = %rp.client_id,
                    uri = %rp.backchannel_logout_uri,
                    "BCL POST failed"
                );
            }
        }
    }

    Ok(report)
}

#[allow(clippy::future_not_send)]
async fn post_logout_token(client: &cyper::Client, uri: &str, token: &str) -> Result<()> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("logout_token", token)
        .finish();
    let request = client
        .request(Method::POST, uri)
        .map_err(|e| AuthError::Internal(format!("build BCL POST: {e}")))?
        .header("content-type", "application/x-www-form-urlencoded")
        .map_err(|e| AuthError::Internal(format!("BCL POST content-type header: {e}")))?
        .header("cache-control", "no-store")
        .map_err(|e| AuthError::Internal(format!("BCL POST cache-control header: {e}")))?
        .body(body);

    let response = compio::time::timeout(BCL_POST_TIMEOUT, request.send())
        .await
        .map_err(|_| AuthError::Internal("BCL POST timeout".into()))?
        .map_err(|e| AuthError::Internal(format!("BCL POST transport: {e}")))?;
    let status = response.status().as_u16();
    if status == 200 || status == 204 {
        Ok(())
    } else {
        let body = response.text().await.unwrap_or_else(|_| "<no body>".into());
        Err(AuthError::Internal(format!(
            "BCL POST returned HTTP {status}: {body}"
        )))
    }
}
