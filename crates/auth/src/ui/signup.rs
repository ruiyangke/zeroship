//! `/signup` GET + POST handlers.
//!
//! Signup creates the `auth.users` row, then redirects back to `/login`
//! (continuing the OIDC flow if a `login_challenge` is present). Email
//! verification is a Phase 5 addition; for Phase 2 we just create the row
//! and let the user proceed straight to `/login`.
//!
//! Account-enumeration defense: a duplicate-email INSERT is treated the
//! same as a fresh INSERT (same redirect, same status, same response). The
//! attacker can already learn this via `/login` timing if we leaked here —
//! and the OWASP guidance is to make signup look uniform.

use askama::Template;
use ntex::http::header::{HeaderValue, COOKIE, LOCATION, SET_COOKIE};
use ntex::http::StatusCode;
use ntex::web::{HttpRequest, HttpResponse};
use serde::Deserialize;
use std::sync::Arc;
use url::form_urlencoded;

use crate::audit::{self, AuditEvent};
use crate::config::AuthConfig;
use crate::csrf;
use crate::identity::{email as email_validation, password, verification};
use crate::mailer::templates::{build_email, VerifyEmailHtml, VerifyEmailText};
use crate::mailer::{Address, Mailer};
use crate::ratelimit::{self, Bucket, RateLimitDecision};
use crate::store::users;
use crate::ui::{ErrorPage, PublicErrorMessage, SignupPage};

const MAX_LOGIN_CHALLENGE_BYTES: usize = 256;

#[derive(Debug, Deserialize)]
pub struct SignupQuery {
    #[serde(default)]
    pub login_challenge: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SignupForm {
    pub csrf: String,
    pub name: String,
    pub email: String,
    pub password: String,
}

/// `/signup` GET — renders the empty form with a fresh CSRF token cookie.
///
/// Marked `async` to satisfy ntex's `Handler` trait (route registration in
/// P2-U6 expects the handler to return a future); the body itself is
/// non-blocking.
//
// ntex's per-thread service futures are intentionally `!Send`.
#[allow(clippy::unused_async, clippy::future_not_send)]
pub async fn get(
    query: ntex::web::types::Query<SignupQuery>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
) -> HttpResponse {
    let challenge = bounded_login_challenge(query.login_challenge.as_deref());
    let csrf_token = csrf::generate_token();
    let page = SignupPage {
        challenge,
        csrf: &csrf_token,
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

// ntex's per-thread service futures are intentionally `!Send`.
#[allow(clippy::future_not_send, clippy::too_many_lines)]
pub async fn post(
    req: HttpRequest,
    query: ntex::web::types::Query<SignupQuery>,
    form: ntex::web::types::Form<SignupForm>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
    mailer: ntex::web::types::State<Arc<dyn Mailer>>,
) -> HttpResponse {
    let challenge = bounded_login_challenge(query.login_challenge.as_deref()).to_string();

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
        return render_signup_error(&challenge, &cfg, "invalid request");
    }

    // 2. Password length (NIST 800-63B Rev 4: 15 char minimum). We count
    // characters, not bytes, so multibyte passphrases aren't penalised.
    if form.password.chars().count() < 15 {
        return render_signup_error(
            &challenge,
            &cfg,
            "password must be at least 15 characters",
        );
    }

    // 3. Email sanity before we enter rate limits, hashing, or storage.
    let email = form.email.trim().to_ascii_lowercase();
    if email_validation::validate_email(&email).is_err() {
        return render_signup_bad_request(&challenge, &cfg, "enter a valid email");
    }

    // 4. Rate-limit per IP before entering the CPU-bound password hash.
    let ip = req
        .peer_addr()
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|| "0.0.0.0".to_string());
    let signup_ip_key = format!("signup_ip:{ip}");
    match ratelimit::consume_or_throttle(db.as_ref(), &signup_ip_key, Bucket::SIGNUP_IP).await {
        Ok(RateLimitDecision::Allowed) => {}
        Ok(RateLimitDecision::Throttled(_)) => {
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "signup_throttled",
                    outcome: "failure",
                    detail: serde_json::json!({ "bucket": "signup_per_ip" }),
                    ..Default::default()
                },
            )
            .await;
            return redirect_to_login(&challenge);
        }
        Err(e) => {
            tracing::error!(error = %e, bucket = %signup_ip_key, "signup rate-limit consume failed");
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
    }

    // 5. Hash the password — argon2 is CPU-bound, run on spawn_blocking so
    // the ntex event loop is not parked (same constraint as /login).
    let password_clone = form.password.clone();
    let phc = match compio::runtime::spawn_blocking(move || password::hash(&password_clone)).await {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            tracing::error!(error = %e, "signup password hash failed");
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
        Err(_) => {
            tracing::error!("signup hash spawn_blocking panicked");
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
    };

    // 6. Insert the user row. Account-enumeration defense: a duplicate
    // email is logged but produces the same response as a successful
    // insert — the attacker cannot probe email existence via this endpoint.
    let name = form.name.trim();
    let created = match users::create(db.as_ref(), &email, name, Some(&phc)).await {
        Ok(u) => Some(u),
        Err(e) => {
            tracing::info!(error = %e, "signup users::create rejected (duplicate or otherwise)");
            None
        }
    };

    // 6b. On a successful create, issue a verification token and email
    //     it to the user. Both the token issue and the email send are
    //     best-effort — a failure is logged but never surfaced to the
    //     user, because:
    //     - revealing an issue failure would leak DB load / connectivity;
    //     - revealing a send failure would leak suppression status,
    //       defeating enumeration defense for known-bouncer addresses;
    //     - users can request a resend later (deferred to a future phase).
    if let Some(user) = &created {
        match verification::issue(db.as_ref(), user.id, &user.email).await {
            Ok(issued) => {
                let link = format!(
                    "{}/verify?token={}",
                    cfg.public_url(),
                    issued.raw,
                );
                let name_hint = user
                    .name
                    .split_whitespace()
                    .next()
                    .unwrap_or("there");

                let html = VerifyEmailHtml {
                    name: name_hint,
                    link: &link,
                    expires_in: "24 hours",
                }
                .render()
                .unwrap_or_default();
                let text = VerifyEmailText {
                    name: name_hint,
                    link: &link,
                    expires_in: "24 hours",
                }
                .render()
                .unwrap_or_default();

                let msg = build_email(
                    Address {
                        email: user.email.clone(),
                        name: Some(user.name.clone()),
                    },
                    Address {
                        email: cfg.mail_from_email.clone(),
                        name: Some(cfg.mail_from_name.clone()),
                    },
                    "Verify your zeroship email".into(),
                    text,
                    html,
                    vec!["verification".into()],
                );
                if let Err(e) = mailer.send(db.as_ref(), msg).await {
                    tracing::warn!(error = %e, user_id = %user.id, "verification email send failed");
                }

                audit::emit(
                    db.as_ref(),
                    &AuditEvent {
                        event_type: "verification_issued",
                        outcome: "success",
                        user_id: Some(&user.id),
                        ..Default::default()
                    },
                )
                .await;
            }
            Err(e) => {
                tracing::error!(error = %e, user_id = %user.id, "verification token issue failed");
                // Don't fail the signup — the user can request resend later.
            }
        }
    }

    // 7. Redirect to /login carrying the same challenge so the user can
    // immediately sign in. The verification email is in their inbox;
    // verifying is decoupled from sign-in.
    redirect_to_login(&challenge)
}

fn redirect_to_login(challenge: &str) -> HttpResponse {
    let to = if challenge.is_empty() || challenge.len() > MAX_LOGIN_CHALLENGE_BYTES {
        "/login".to_string()
    } else {
        let challenge_enc: String =
            form_urlencoded::byte_serialize(challenge.as_bytes()).collect();
        format!("/login?login_challenge={challenge_enc}")
    };
    let mut resp = HttpResponse::Found();
    resp.header(
        LOCATION,
        HeaderValue::from_str(&to).unwrap_or_else(|_| HeaderValue::from_static("/login")),
    );
    resp.finish()
}

fn bounded_login_challenge(challenge: Option<&str>) -> &str {
    challenge
        .filter(|value| value.len() <= MAX_LOGIN_CHALLENGE_BYTES)
        .unwrap_or("")
}

fn render_signup_error(challenge: &str, cfg: &AuthConfig, err: &str) -> HttpResponse {
    render_signup_error_with_status(challenge, cfg, err, StatusCode::OK)
}

fn render_signup_bad_request(challenge: &str, cfg: &AuthConfig, err: &str) -> HttpResponse {
    render_signup_error_with_status(challenge, cfg, err, StatusCode::BAD_REQUEST)
}

fn render_signup_error_with_status(
    challenge: &str,
    cfg: &AuthConfig,
    err: &str,
    status: StatusCode,
) -> HttpResponse {
    let csrf_token = csrf::generate_token();
    let page = SignupPage {
        challenge,
        csrf: &csrf_token,
        error: Some(err),
    };
    let body = page
        .render()
        .unwrap_or_else(|_| format!("<h1>{err}</h1>"));
    let mut resp = HttpResponse::build(status);
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token, cfg.insecure_dev));
    resp.body(body)
}

fn render_error_page(message: PublicErrorMessage) -> HttpResponse {
    let page = ErrorPage {
        message,
        error_code: message.error_code(),
    };
    let body = page
        .render()
        .unwrap_or_else(|_| format!("<h1>{}</h1>", message.as_str()));
    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.body(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location(resp: &HttpResponse) -> &str {
        resp.headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .expect("location header")
    }

    #[test]
    fn redirect_to_login_url_encodes_challenge() {
        let resp = redirect_to_login("foo&malicious=value");
        assert_eq!(
            location(&resp),
            "/login?login_challenge=foo%26malicious%3Dvalue"
        );
    }

    #[test]
    fn redirect_to_login_drops_oversized_challenge() {
        let challenge = "a".repeat(257);
        let resp = redirect_to_login(&challenge);
        assert_eq!(location(&resp), "/login");
    }
}
