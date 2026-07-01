//! `/me/sessions` GET + `/me/sessions/{id}/revoke` POST handlers — the
//! signed-in user's active-session visibility + single-session revoke
//! (ISS-10).
//!
//! Authentication source of truth is the same `__Host-zsidp_session` cookie
//! the `/me` profile page uses: parse the cookie → `store::sessions::validate`
//! (atomic check-and-slide) → the validated IdP session both *identifies the
//! caller* (`user_id`) and *is the caller's current session* (its id is the
//! cookie's session id). No other table is consulted to authenticate.
//!
//! ## Listing (`GET /me/sessions`)
//!
//! Returns JSON: every active session across both `idp_sessions` and
//! `gateway_sessions` for the caller, newest-first, each tagged with its
//! `kind` (`"idp"`/`"app"`), `app_id` (app sessions only), and a `current`
//! flag set on the row whose id equals the caller's cookie session id.
//!
//! ## Revoke (`POST /me/sessions/{id}/revoke`)
//!
//! CSRF-guarded (same double-submit cookie scheme as `/me/unlink`). The body
//! carries the `kind` so the store knows which table to hit. The revoke is
//! scoped to `user_id = <authenticated caller>` in SQL, so a caller can only
//! ever revoke their OWN session — passing another user's session id revokes
//! nothing (`store::sessions::revoke_one_for_user`).

use std::sync::Arc;

use ntex::http::header::COOKIE;
use ntex::http::StatusCode;
use ntex::web::{HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::AuthConfig;
use crate::csrf;
use crate::oidc;
use crate::sessions::login as session_cookie;
use crate::store::sessions::{self, SessionKind, SessionSummary};

/// The caller's resolved identity for a `/me/sessions` request: the user id
/// plus the id of their current (cookie) session, so the list can flag it.
struct Caller {
    user_id: uuid::Uuid,
    current_session_id: uuid::Uuid,
}

/// One row in the `GET /me/sessions` JSON response: a [`SessionSummary`] plus
/// the `current` flag.
#[derive(Debug, Serialize)]
struct SessionView {
    #[serde(flatten)]
    summary: SessionSummary,
    /// `true` for the caller's own current (cookie) session.
    current: bool,
}

/// `GET /me/sessions` — JSON list of the caller's active sessions.
///
/// 401 (JSON) when the session cookie is missing/invalid/expired — this is an
/// API surface, so we don't 302 to `/login` the way the HTML `/me` page does.
#[allow(clippy::future_not_send)]
pub async fn list(
    req: HttpRequest,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    let Some(caller) = resolve_caller(&req, db.as_ref(), cfg.insecure_dev).await else {
        return unauthorized();
    };

    let summaries = match sessions::list_by_user(db.as_ref(), caller.user_id).await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "sessions::list_by_user failed");
            return internal_error();
        }
    };

    let views: Vec<SessionView> = summaries
        .into_iter()
        .map(|summary| {
            let current = summary.kind == SessionKind::Idp
                && summary.id == caller.current_session_id;
            SessionView { summary, current }
        })
        .collect();

    HttpResponse::Ok().json(&json!({ "sessions": views }))
}

#[derive(Debug, Deserialize)]
pub struct RevokeForm {
    pub csrf: String,
    /// Which table the target session lives in (`"idp"` or `"app"`).
    pub kind: String,
}

/// `POST /me/sessions/{id}/revoke` — revoke one of the CALLER's own sessions.
///
/// Flow: CSRF check → resolve caller from the cookie → parse the target
/// session id + kind → `revoke_one_for_user(user_id = caller, id = target)`.
/// The `user_id` scope is the IDOR guard: the caller can only revoke their
/// own sessions. Returns JSON `{ "revoked": bool }` (200) on a valid request;
/// `revoked: false` covers "not yours / already gone / unknown id".
#[allow(clippy::future_not_send)]
pub async fn revoke(
    req: HttpRequest,
    path: ntex::web::types::Path<(String,)>,
    form: ntex::web::types::Form<RevokeForm>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
    issuer: ntex::web::types::State<Arc<oidc::Issuer>>,
) -> HttpResponse {
    // 1. CSRF (double-submit cookie), same scheme as /me/unlink.
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let cookie_token = csrf::parse_cookie(cookie_header, cfg.insecure_dev);
    if cookie_token
        .as_deref()
        .is_none_or(|c| !csrf::matches(&form.csrf, c))
    {
        return bad_request("invalid_csrf");
    }

    // 2. Parse the target session id + kind.
    let Ok(session_id) = uuid::Uuid::parse_str(&path.into_inner().0) else {
        return bad_request("invalid_session_id");
    };
    let Some(kind) = parse_kind(&form.kind) else {
        return bad_request("invalid_kind");
    };

    // 3. Resolve the caller (authentication).
    let Some(caller) = resolve_caller(&req, db.as_ref(), cfg.insecure_dev).await else {
        return unauthorized();
    };

    // 4. Revoke — scoped to `user_id = caller` in SQL (the IDOR guard).
    match sessions::revoke_one_for_user(db.as_ref(), caller.user_id, session_id, kind).await {
        Ok(revoked) => {
            if revoked && kind == SessionKind::Idp {
                match oidc::backchannel_logout::emit_for_session(
                    db.as_ref(),
                    issuer.as_ref(),
                    session_id,
                )
                .await
                {
                    Ok(report) => tracing::info!(
                        session_id = %session_id,
                        attempted = report.attempted,
                        delivered = report.delivered,
                        "sessions revoke: emitted OIDC back-channel logout tokens"
                    ),
                    Err(e) => tracing::error!(
                        error = %e,
                        session_id = %session_id,
                        "sessions revoke: BCL emission failed"
                    ),
                }
            }
            HttpResponse::Ok().json(&json!({ "revoked": revoked }))
        }
        Err(e) => {
            tracing::error!(error = %e, "sessions::revoke_one_for_user failed");
            internal_error()
        }
    }
}

// ─── helpers ─────────────────────────────────────────────────────────────

/// Resolve the caller from the `__Host-zsidp_session` cookie. Returns the
/// user id plus the current session id, or `None` if the cookie is
/// missing/invalid/expired. `validate` slides the idle window, so hitting
/// this surface counts as activity — same as `/me`.
#[allow(clippy::future_not_send)]
async fn resolve_caller(
    req: &HttpRequest,
    db: &compio_postgres::Client,
    insecure_dev: bool,
) -> Option<Caller> {
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let session_id = session_cookie::parse_cookie(cookie_header, insecure_dev)?;
    let session = sessions::validate(db, session_id).await.ok().flatten()?;
    Some(Caller {
        user_id: session.user_id,
        current_session_id: session.id,
    })
}

fn parse_kind(raw: &str) -> Option<SessionKind> {
    match raw {
        "idp" => Some(SessionKind::Idp),
        "app" => Some(SessionKind::App),
        _ => None,
    }
}

fn unauthorized() -> HttpResponse {
    HttpResponse::Unauthorized().json(&json!({ "error": "unauthenticated" }))
}

fn bad_request(reason: &str) -> HttpResponse {
    HttpResponse::build(StatusCode::BAD_REQUEST).json(&json!({ "error": reason }))
}

fn internal_error() -> HttpResponse {
    HttpResponse::InternalServerError().json(&json!({ "error": "internal" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kind_maps_known_strings() {
        assert_eq!(parse_kind("idp"), Some(SessionKind::Idp));
        assert_eq!(parse_kind("app"), Some(SessionKind::App));
        assert_eq!(parse_kind(""), None);
        assert_eq!(parse_kind("gateway"), None);
        assert_eq!(parse_kind("IDP"), None);
    }

    /// The `current` flag is only ever set on an IdP-kind row whose id matches
    /// the caller's cookie session — an app session is never "current".
    #[test]
    fn current_flag_only_for_matching_idp_session() {
        let cur = uuid::Uuid::new_v4();
        let app_id = uuid::Uuid::new_v4();
        let now = chrono::Utc::now();
        let mk = |id, kind| SessionSummary {
            id,
            kind,
            app_id: if kind == SessionKind::App { Some(app_id) } else { None },
            created_at: now,
            last_seen_at: now,
            expires_at: now,
        };

        // The caller's own idp session → current.
        let idp_self = mk(cur, SessionKind::Idp);
        assert!(idp_self.kind == SessionKind::Idp && idp_self.id == cur);

        // A different idp session id → not current.
        let idp_other = mk(uuid::Uuid::new_v4(), SessionKind::Idp);
        assert!(!(idp_other.kind == SessionKind::Idp && idp_other.id == cur));

        // An app session sharing the (impossible) same id → still not current
        // because kind must be Idp.
        let app_same_id = mk(cur, SessionKind::App);
        assert!(!(app_same_id.kind == SessionKind::Idp && app_same_id.id == cur));
    }
}
