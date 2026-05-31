//! `/logout` GET + POST handlers — RP-initiated logout flow.
//!
//! OIDC Session Management §5 / RFC 9207: the RP redirects the
//! user-agent to hydra's `end_session_endpoint`
//! (`/oauth2/sessions/logout`) with an `id_token_hint`. Hydra validates
//! the hint, issues a `logout_challenge`, and 302s here. We render a
//! CSRF-protected confirm form; on POST we call hydra admin's
//! `accept_logout` and 302 to the post-logout `redirect_to`.
//!
//! Algorithm:
//!
//! 1. **GET `/logout?logout_challenge=<…>`** — fetch the logout request
//!    from hydra (`get_logout`). On success render [`LogoutPage`] with
//!    a fresh CSRF cookie. On hydra error render an error page.
//!
//! 2. **POST `/logout`** — validate the CSRF double-submit, call
//!    `accept_logout(challenge)`. Hydra returns a `redirect_to` —
//!    that's the RP's `post_logout_redirect_uri` (or hydra's
//!    default if the RP didn't supply one). Best-effort revoke
//!    the local `zeroship.sessions` row keyed by the IdP session cookie,
//!    and also attempt the historical Hydra-`sid` revoke.
//!
//! No "Stay signed in" reject path: hydra has no
//! `reject_logout` admin endpoint. If the user wants to abandon the
//! logout dance they close the tab; hydra times the challenge out.

use askama::Template;
use ntex::http::header::{HeaderValue, COOKIE, LOCATION, SET_COOKIE};
use ntex::web::{HttpRequest, HttpResponse};
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

use crate::audit::{self, AuditEvent};
use crate::config::AuthConfig;
use crate::csrf;
use crate::hydra_client::HydraAdmin;
use crate::sessions::login as session_cookie;
use crate::store::sessions;
use crate::ui::{ErrorPage, LogoutPage, PublicErrorMessage};

#[derive(Debug, Deserialize)]
pub struct LogoutQuery {
    pub logout_challenge: String,
}

#[derive(Debug, Deserialize)]
pub struct LogoutForm {
    pub csrf: String,
    pub logout_challenge: String,
}

/// `/logout?logout_challenge=<…>` GET — render the confirmation form.
//
// ntex's per-thread service futures are intentionally `!Send`.
#[allow(clippy::future_not_send)]
pub async fn get(
    query: ntex::web::types::Query<LogoutQuery>,
    admin: ntex::web::types::State<HydraAdmin>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
) -> HttpResponse {
    let challenge = &query.logout_challenge;

    // Fetch the logout request from hydra. This validates the
    // challenge exists + isn't stale, and surfaces the optional RP
    // client_name we render on the confirmation page.
    let info = match admin.get_logout(challenge).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, challenge = %challenge, "logout challenge fetch failed");
            return render_error(PublicErrorMessage::InvalidRequest);
        }
    };

    let csrf_token = csrf::generate_token();
    let client_name = info
        .client
        .as_ref()
        .and_then(|c| c.client_name.as_deref().or(Some(c.client_id.as_str())));
    let page = LogoutPage {
        challenge,
        csrf: &csrf_token,
        client_name,
        error: None,
    };
    let body = page
        .render()
        .unwrap_or_else(|_| "<h1>error</h1>".to_string());

    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token, cfg.insecure_dev));
    resp.body(body)
}

/// `/logout` POST — validate CSRF, accept the logout at hydra, 302 to
/// hydra's post-logout `redirect_to`. Best-effort revoke the local IdP
/// session row from the browser cookie so the cookie stops resolving.
#[allow(clippy::future_not_send)]
pub async fn post(
    req: HttpRequest,
    form: ntex::web::types::Form<LogoutForm>,
    admin: ntex::web::types::State<HydraAdmin>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    // 1. CSRF.
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let cookie_token = csrf::parse_cookie(cookie_header, cfg.insecure_dev);
    let local_session_id = session_cookie::parse_cookie(cookie_header, cfg.insecure_dev);
    if cookie_token
        .as_deref()
        .is_none_or(|c| !csrf::matches(&form.csrf, c))
    {
        return render_error(PublicErrorMessage::InvalidRequest);
    }

    let challenge = form.logout_challenge.as_str();

    // 2. Re-fetch — we want the authoritative subject + sid for the
    //    audit event and the local-session revoke step. The form's
    //    challenge is attacker-controlled.
    let info = match admin.get_logout(challenge).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, challenge = %challenge, "POST /logout: get_logout failed");
            return render_error(PublicErrorMessage::InvalidRequest);
        }
    };

    // 3. Best-effort: revoke the local zeroship.sessions row from the
    //    cookie the browser is actually presenting. Failures are
    //    non-fatal — hydra's accept_logout still tears down hydra's
    //    own session.
    if let Some(session_id) = local_session_id {
        if let Err(e) = sessions::revoke(db.as_ref(), session_id).await {
            tracing::warn!(error = %e, session_id = %session_id, "logout: local cookie session revoke failed");
        }
    }

    // 4. Best-effort compatibility with Hydra sessions that happen to
    //    carry the local UUID as `sid`; Hydra still needs its row gone,
    //    and older flows may have aligned the two ids.
    if let Ok(sid) = Uuid::parse_str(&info.sid) {
        if let Err(e) = sessions::revoke(db.as_ref(), sid).await {
            tracing::warn!(error = %e, sid = %info.sid, "logout: local session revoke failed");
        }
    } else {
        tracing::debug!(sid = %info.sid, "logout: sid is not a UUID (probably hydra-internal); skipping local revoke");
    }

    // 5. Accept the logout at hydra.
    let resp = match admin.accept_logout(challenge).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "accept_logout failed");
            return render_error(PublicErrorMessage::ContactSupport);
        }
    };

    // 6. Audit. Best-effort — failure here doesn't fail the response.
    let subject = info.subject.clone();
    audit::emit(
        db.as_ref(),
        &AuditEvent {
            event_type: "logout",
            outcome: "success",
            user_id: None,
            client_id: info.client.as_ref().map(|c| c.client_id.as_str()),
            auth_method: None,
            detail: serde_json::json!({
                "subject": subject,
                "rp_initiated": info.rp_initiated,
            }),
            ..AuditEvent::from_request(&req)
        },
    )
    .await;

    // 7. 302 + clear the IdP session cookie so the browser drops it
    //    immediately (don't wait for Max-Age expiry).
    let mut http_resp = HttpResponse::Found();
    http_resp.header(
        LOCATION,
        HeaderValue::from_str(&resp.redirect_to)
            .unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    http_resp.header(
        SET_COOKIE,
        session_cookie::clear_cookie(cfg.insecure_dev),
    );
    http_resp.finish()
}

fn render_error(message: PublicErrorMessage) -> HttpResponse {
    let page = ErrorPage {
        message,
        error_code: message.error_code(),
    };
    let body = page
        .render()
        .unwrap_or_else(|_| format!("<h1>{}</h1>", message.as_str()));
    let mut resp = HttpResponse::BadRequest();
    resp.content_type("text/html; charset=utf-8");
    resp.body(body)
}
