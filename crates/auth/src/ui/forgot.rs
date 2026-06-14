//! `/forgot` GET + POST handlers.
//!
//! GET renders the email-entry form. POST always returns 200 with the
//! "if an account exists, we sent a link" page — regardless of whether
//! the address has an account, was rate-limited, or the mail send
//! failed. Enumeration defense.
//!
//! When the address does have an account, we issue a 1-hour password
//! reset token (via [`identity::password_reset::issue`]) and send the
//! reset email. Failures are logged and silently swallowed so the
//! response remains uniform.

use askama::Template;
use ntex::http::header::{COOKIE, SET_COOKIE};
use ntex::http::StatusCode;
use ntex::web::{
    types::{Form, State},
    HttpRequest, HttpResponse,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::audit::{self, AuditEvent};
use crate::config::AuthConfig;
use crate::csrf;
use crate::identity::{email as email_validation, password_reset};
use crate::ratelimit::{self, Bucket, RateLimitDecision};
use crate::store::users;
use zeroship_mailer::templates::{build_email, PasswordResetHtml, PasswordResetText};
use zeroship_mailer::{Address, Mailer};
use crate::ui::ForgotPage;
use sha2::{Digest, Sha256};

#[derive(Debug, Deserialize)]
pub struct ForgotForm {
    pub csrf: String,
    pub email: String,
}

/// `/forgot` GET — renders the email-entry form with a fresh CSRF
/// cookie.
//
// ntex's per-thread service futures are intentionally `!Send`.
#[allow(clippy::unused_async, clippy::future_not_send)]
pub async fn get(cfg: State<Arc<AuthConfig>>) -> HttpResponse {
    render_form(&cfg, None, false)
}

/// `/forgot` POST — validate CSRF, look up the user, (best-effort) issue
/// a reset token and email it, render the confirmation page. Always 200.
#[allow(clippy::future_not_send)]
pub async fn post(
    req: HttpRequest,
    form: Form<ForgotForm>,
    cfg: State<Arc<AuthConfig>>,
    db: State<Arc<compio_postgres::Client>>,
    mailer: State<Arc<dyn Mailer>>,
) -> HttpResponse {
    // 1. CSRF.
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
        return render_form(&cfg, Some("invalid request"), false);
    }

    // 2. Look up user. Absent → still render confirmation (no
    //    enumeration leak). The email is normalized to lowercase
    //    consistently with /signup so case-only differences match.
    let email_norm = form.email.trim().to_ascii_lowercase();
    if email_validation::validate_email(&email_norm).is_err() {
        return render_form_with_status(
            &cfg,
            Some("enter a valid email"),
            false,
            StatusCode::BAD_REQUEST,
        );
    }
    let email_hash = hex::encode(Sha256::digest(email_norm.as_bytes()));
    // Forwarded client IP (auth is behind the gateway); see signup.rs.
    let ip = crate::headers::client_ip(&req);
    let buckets = [
        (
            format!("forgot_email:{email_hash}"),
            Bucket::FORGOT_EMAIL,
            "forgot_per_email",
        ),
        (format!("forgot_ip:{ip}"), Bucket::FORGOT_IP, "forgot_per_ip"),
    ];
    for (key, bucket, bucket_name) in &buckets {
        match ratelimit::consume_or_throttle(db.as_ref(), key, *bucket).await {
            Ok(RateLimitDecision::Allowed) => {}
            Ok(RateLimitDecision::Throttled(_)) => {
                audit::emit(
                    db.as_ref(),
                    &AuditEvent {
                        event_type: "password_reset_requested_throttled",
                        outcome: "failure",
                        detail: serde_json::json!({ "bucket": bucket_name }),
                        ..AuditEvent::from_request(&req)
                    },
                )
                .await;
                return render_form(&cfg, None, true);
            }
            Err(e) => {
                tracing::error!(error = %e, bucket = %key, "forgot rate-limit consume failed");
                return render_form(&cfg, None, true);
            }
        }
    }

    let user = users::find_by_email(db.as_ref(), &email_norm)
        .await
        .ok()
        .flatten();

    if let Some(u) = user {
        // 3. Issue token + send email. Both are best-effort — a failure
        //    must NOT change the response, only emit logs.
        match password_reset::issue(db.as_ref(), &u.email).await {
            Ok(issued) => {
                let link = format!("{}/reset?token={}", cfg.public_url(), issued.raw);
                let name_hint = u.name.split_whitespace().next().unwrap_or("there");
                let html = PasswordResetHtml {
                    name: name_hint,
                    link: &link,
                    expires_in: "1 hour",
                }
                .render()
                .unwrap_or_default();
                let text = PasswordResetText {
                    name: name_hint,
                    link: &link,
                    expires_in: "1 hour",
                }
                .render()
                .unwrap_or_default();

                let msg = build_email(
                    Address {
                        email: u.email.clone(),
                        name: Some(u.name.clone()),
                    },
                    Address {
                        email: cfg.mail_from_email.clone(),
                        name: Some(cfg.mail_from_name.clone()),
                    },
                    "Reset your zeroship password".into(),
                    text,
                    html,
                    vec!["password-reset".into()],
                );
                if let Err(e) = mailer.send(db.as_ref(), msg).await {
                    tracing::warn!(error = %e, user_id = %u.id, "password_reset email send failed");
                }

                audit::emit(
                    db.as_ref(),
                    &AuditEvent {
                        event_type: "password_reset_requested",
                        outcome: "success",
                        user_id: Some(&u.id),
                        ..AuditEvent::from_request(&req)
                    },
                )
                .await;
            }
            Err(e) => {
                tracing::error!(error = %e, user_id = %u.id, "password_reset token issue failed");
            }
        }
    } else {
        // Even the no-user branch should emit a failure-outcome audit
        // event so attempted resets against unknown addresses are
        // visible in the audit stream. We don't include the email
        // (PII / would let log readers enumerate); just the outcome.
        audit::emit(
            db.as_ref(),
            &AuditEvent {
                event_type: "password_reset_requested",
                outcome: "failure",
                detail: serde_json::json!({ "reason": "no_such_user" }),
                ..AuditEvent::from_request(&req)
            },
        )
        .await;
    }

    render_form(&cfg, None, true)
}

fn render_form(cfg: &AuthConfig, error: Option<&str>, sent: bool) -> HttpResponse {
    render_form_with_status(cfg, error, sent, StatusCode::OK)
}

fn render_form_with_status(
    cfg: &AuthConfig,
    error: Option<&str>,
    sent: bool,
    status: StatusCode,
) -> HttpResponse {
    let csrf_token = csrf::generate_token();
    let page = ForgotPage {
        csrf: &csrf_token,
        error,
        sent,
    };
    let body = page
        .render()
        .unwrap_or_else(|_| "<h1>error</h1>".to_string());
    let mut resp = HttpResponse::build(status);
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token, cfg.insecure_dev));
    resp.body(body)
}
