//! `/logout` GET + POST handlers for the native OP session logout flow.
//!
//! GET renders a CSRF-protected confirmation form. POST validates the
//! double-submit token, revokes the local `zeroship.idp_sessions` row keyed by
//! the IdP session cookie, emits OIDC back-channel logout tokens for that
//! native session, clears the browser cookie, and sends the browser back to
//! `/login`.

use askama::Template;
use ntex::http::header::{HeaderValue, COOKIE, LOCATION, SET_COOKIE};
use ntex::web::{HttpRequest, HttpResponse};
use serde::Deserialize;
use std::sync::Arc;

use crate::audit::{self, AuditEvent};
use crate::config::AuthConfig;
use crate::csrf;
use crate::oidc;
use crate::sessions::login as session_cookie;
use crate::store::sessions;
use crate::ui::{ErrorPage, LogoutPage, PublicErrorMessage};

#[derive(Debug, Deserialize)]
pub struct LogoutQuery {}

#[derive(Debug, Deserialize)]
pub struct LogoutForm {
    pub csrf: String,
}

/// `/logout` GET — render the confirmation form.
//
// ntex's per-thread service futures are intentionally `!Send`.
#[allow(clippy::future_not_send)]
pub async fn get(
    _query: ntex::web::types::Query<LogoutQuery>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
) -> HttpResponse {
    let csrf_token = csrf::generate_token();
    let page = LogoutPage {
        csrf: &csrf_token,
        client_name: None,
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

/// `/logout` POST — validate CSRF and revoke the local IdP session row from
/// the browser cookie so the cookie stops resolving.
#[allow(clippy::future_not_send)]
pub async fn post(
    req: HttpRequest,
    form: ntex::web::types::Form<LogoutForm>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
    issuer: ntex::web::types::State<Arc<oidc::Issuer>>,
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

    if let Some(session_id) = local_session_id {
        if let Err(e) = sessions::revoke(db.as_ref(), session_id).await {
            tracing::warn!(error = %e, session_id = %session_id, "logout: local cookie session revoke failed");
        }
        match oidc::backchannel_logout::emit_for_session(db.as_ref(), issuer.as_ref(), session_id)
            .await
        {
            Ok(report) => tracing::info!(
                session_id = %session_id,
                attempted = report.attempted,
                delivered = report.delivered,
                "logout: emitted OIDC back-channel logout tokens"
            ),
            Err(e) => tracing::error!(
                error = %e,
                session_id = %session_id,
                "logout: BCL emission failed"
            ),
        }
    }

    // Audit. Best-effort — failure here doesn't fail the response.
    audit::emit(
        db.as_ref(),
        &AuditEvent {
            event_type: "logout",
            outcome: "success",
            user_id: None,
            client_id: None,
            auth_method: None,
            detail: serde_json::json!({
                "session_id": local_session_id.map(|id| id.to_string()),
            }),
            ..AuditEvent::from_request(&req)
        },
    )
    .await;

    // 302 + clear the IdP session cookie so the browser drops it immediately
    // (don't wait for Max-Age expiry).
    let mut http_resp = HttpResponse::Found();
    http_resp.header(
        LOCATION,
        HeaderValue::from_static("/login"),
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
