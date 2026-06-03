//! `/verify` handlers — GET renders a POST interstitial, then POST redeems
//! an email-verification token and marks `zeroship.users.email_verified_at = NOW()`.
//!
//! Per proposal §8.3 (Phase 5). The token is issued by [`crate::ui::signup`]
//! on a successful signup and emailed to the user. Clicking the link in
//! the email lands here; we atomically stamp the user's
//! `email_verified_at` and consume the row via
//! [`crate::identity::verification::redeem_and_mark_verified`], emit an
//! audit event, and render a confirmation page.
//!
//! No session is minted here — verification is decoupled from sign-in.
//! The success page links back to `/login` so the user can continue.

use askama::Template;
use ntex::http::header::COOKIE;
use ntex::http::StatusCode;
use ntex::web::{
    types::{Form, Query, State},
    HttpRequest, HttpResponse,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::audit::{self, AuditEvent};
use crate::config::AuthConfig;
use crate::csrf;
use crate::identity::verification;
use crate::ui::{
    render_token_interstitial, ErrorPage, PublicErrorMessage, TokenRedeemInterstitial,
    VerifyOkPage,
};

#[derive(Debug, Deserialize)]
pub struct VerifyQuery {
    /// Raw verification token. base64url, no padding (32-byte CSPRNG).
    pub token: String,
}

#[derive(Debug, Deserialize)]
pub struct VerifyRedeemForm {
    pub csrf: Option<String>,
    pub token: String,
}

/// `/verify?token=<token>` — render a one-shot interstitial that immediately
/// POSTs the token to `/verify/redeem`.
#[allow(clippy::unused_async, clippy::future_not_send)]
pub async fn get(query: Query<VerifyQuery>, cfg: State<Arc<AuthConfig>>) -> HttpResponse {
    let csrf_token = csrf::generate_token();
    let page = TokenRedeemInterstitial {
        title: "Verify email",
        action: "/verify/redeem",
        token: &query.token,
        csrf: &csrf_token,
        // Overwritten with an independent per-response nonce inside
        // `render_token_interstitial`; callers must not set it.
        script_nonce: "",
        extra_fields: Vec::new(),
    };
    let csrf_set_cookie = csrf::set_cookie(&csrf_token, cfg.insecure_dev);
    render_token_interstitial(&page, &csrf_set_cookie)
}

/// `/verify/redeem` — atomically redeem the verification token, set
/// `email_verified_at = NOW()` on the user, render the success page.
///
/// Invalid/expired tokens render the generic [`ErrorPage`]; the user can
/// request a fresh verification email from the (future) `/me` action.
#[allow(clippy::future_not_send)]
pub async fn post_redeem(
    req: HttpRequest,
    form: Form<VerifyRedeemForm>,
    cfg: State<Arc<AuthConfig>>,
    db: State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    if !valid_csrf(&req, form.csrf.as_deref(), &cfg) {
        return render_error_with_status(
            PublicErrorMessage::InvalidRequest,
            StatusCode::FORBIDDEN,
        );
    }

    // 1. Redeem atomically AND mark the user verified in one statement.
    //    `redeem` alone only consumes the token; it leaves
    //    `zeroship.users.email_verified_at` untouched, so the user would see
    //    "Email verified" while the row stayed unverified. Use
    //    `redeem_and_mark_verified` so the consume and the user update are
    //    the same atomic SQL statement (see verification.rs).
    let redeemed = match verification::redeem_and_mark_verified(db.as_ref(), &form.token).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "verification_redeemed",
                    outcome: "failure",
                    detail: serde_json::json!({ "reason": "invalid_or_expired" }),
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
            return render_error(PublicErrorMessage::SessionExpired);
        }
        Err(e) => {
            tracing::error!(error = %e, "verification redeem db error");
            return render_error(PublicErrorMessage::ContactSupport);
        }
    };

    audit::emit(
        db.as_ref(),
        &AuditEvent {
            event_type: "verification_redeemed",
            outcome: "success",
            user_id: Some(&redeemed.user_id),
            ..AuditEvent::from_request(&req)
        },
    )
    .await;

    let page = VerifyOkPage {
        email: &redeemed.email,
    };
    let body = page
        .render()
        .unwrap_or_else(|_| "<h1>Email verified.</h1>".to_string());
    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.body(body)
}

fn valid_csrf(req: &HttpRequest, form_csrf: Option<&str>, cfg: &AuthConfig) -> bool {
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let cookie_token = csrf::parse_cookie(cookie_header, cfg.insecure_dev);
    cookie_token
        .as_deref()
        .zip(form_csrf)
        .is_some_and(|(cookie, form)| csrf::matches(form, cookie))
}

fn render_error(message: PublicErrorMessage) -> HttpResponse {
    render_error_with_status(message, StatusCode::OK)
}

fn render_error_with_status(message: PublicErrorMessage, status: StatusCode) -> HttpResponse {
    let page = ErrorPage {
        message,
        error_code: message.error_code(),
    };
    let body = page
        .render()
        .unwrap_or_else(|_| format!("<h1>{}</h1>", message.as_str()));
    let mut resp = HttpResponse::build(status);
    resp.content_type("text/html; charset=utf-8");
    resp.body(body)
}
