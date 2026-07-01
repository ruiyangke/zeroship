//! `/signup` GET + POST handlers.
//!
//! Signup creates the `zeroship.users` row, then redirects back to `/login`
//! with the same native OP `return_to` continuation target.
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
use crate::oidc::auth_request::AuthRequest;
use crate::ratelimit::{self, Bucket, RateLimitDecision};
use crate::store::users;
use crate::ui::{ErrorPage, PublicErrorMessage, SignupPage};
use zeroship_mailer::templates::{build_email, VerifyEmailHtml, VerifyEmailText};
use zeroship_mailer::{Address, Mailer};

const MAX_RETURN_TO_BYTES: usize = 4096;
const MAX_SIGNUP_NAME_CHARS: usize = 200;

#[derive(Debug, Deserialize)]
pub struct SignupQuery {
    #[serde(default)]
    pub return_to: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SignupForm {
    pub csrf: String,
    pub return_to: Option<String>,
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
    let continuation = match SignupContinuation::from_query(&query) {
        Ok(continuation) => continuation,
        Err(_) => return render_signup_bad_request(None, &cfg, "invalid request"),
    };
    let csrf_token = csrf::generate_token();
    let page = SignupPage {
        return_to: continuation.return_to(),
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
    let continuation = match SignupContinuation::from_post(&query, &form) {
        Ok(continuation) => continuation,
        Err(_) => return render_signup_bad_request(None, &cfg, "invalid request"),
    };

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
        return render_signup_error(Some(&continuation), &cfg, "invalid request");
    }

    // 2. Password length (NIST 800-63B Rev 4: 15 char minimum). We count
    // characters, not bytes, so multibyte passphrases aren't penalised.
    if form.password.chars().count() < 15 {
        return render_signup_error(
            Some(&continuation),
            &cfg,
            "password must be at least 15 characters",
        );
    }

    // 3. Email sanity before we enter rate limits, hashing, or storage.
    let email = form.email.trim().to_ascii_lowercase();
    if email_validation::validate_email(&email).is_err() {
        return render_signup_bad_request(Some(&continuation), &cfg, "enter a valid email");
    }

    // 4. Name sanity before entering rate limits, hashing, or storage.
    let Some(name) = normalize_signup_name(&form.name) else {
        return render_signup_bad_request(
            Some(&continuation),
            &cfg,
            "name must be 1-200 characters",
        );
    };
    let name = name.to_string();

    // 5. Rate-limit per IP before entering the CPU-bound password hash.
    //    Use the forwarded client IP (auth runs behind the gateway, so the
    //    socket peer is the gateway — keying on it would make this a single
    //    global bucket). Matches link.rs and the audit RequestContext.
    let ip = crate::headers::client_ip(&req);
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
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
            return redirect_to_login(&continuation);
        }
        Err(e) => {
            tracing::error!(error = %e, bucket = %signup_ip_key, "signup rate-limit consume failed");
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
    }

    // 6. Hash the password — argon2 is CPU-bound, run on spawn_blocking so
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

    // 7. Insert the user row. Account-enumeration defense: a duplicate
    // email is logged but produces the same response as a successful
    // insert — the attacker cannot probe email existence via this endpoint.
    let created = match users::create(db.as_ref(), &email, &name, Some(&phc)).await {
        Ok(u) => Some(u),
        Err(e) if e.db_code() == Some("23505") => {
            tracing::info!(error = %e, "signup users::create rejected duplicate email");
            None
        }
        Err(e) => {
            tracing::error!(error = %e, "signup users::create failed");
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "signup_failed",
                    outcome: "failure",
                    auth_method: Some("password"),
                    detail: serde_json::json!({
                        "reason": "users_create_failed",
                        "db_code": e.db_code(),
                    }),
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
    };

    // 7b. On a successful create, issue a verification token and email
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
                        ..AuditEvent::from_request(&req)
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

    // 8. Redirect to /login carrying the same continuation so the user can
    // immediately sign in. The verification email is in their inbox; verifying
    // is decoupled from sign-in.
    redirect_to_login(&continuation)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SignupContinuation {
    ReturnTo(String),
}

impl SignupContinuation {
    fn from_query(query: &SignupQuery) -> Result<Self, SignupContinuationError> {
        Self::from_inputs(query.return_to.as_deref(), None)
    }

    fn from_post(query: &SignupQuery, form: &SignupForm) -> Result<Self, SignupContinuationError> {
        Self::from_inputs(query.return_to.as_deref(), form.return_to.as_deref())
    }

    fn from_inputs(
        query_return_to: Option<&str>,
        form_return_to: Option<&str>,
    ) -> Result<Self, SignupContinuationError> {
        let mut targets = Vec::new();

        for return_to in [query_return_to, form_return_to] {
            if let Some(return_to) = bounded_return_to(return_to)? {
                targets.push(Self::ReturnTo(return_to));
            }
        }

        if targets.len() == 1 {
            Ok(targets.remove(0))
        } else {
            Err(SignupContinuationError)
        }
    }

    fn return_to(&self) -> &str {
        match self {
            Self::ReturnTo(return_to) => return_to,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SignupContinuationError;

fn redirect_to_login(continuation: &SignupContinuation) -> HttpResponse {
    let to = match continuation {
        SignupContinuation::ReturnTo(return_to) => {
            let return_to_enc: String =
                form_urlencoded::byte_serialize(return_to.as_bytes()).collect();
            format!("/login?return_to={return_to_enc}")
        }
    };
    let mut resp = HttpResponse::Found();
    resp.header(
        LOCATION,
        HeaderValue::from_str(&to).unwrap_or_else(|_| HeaderValue::from_static("/login")),
    );
    resp.finish()
}

fn bounded_return_to(return_to: Option<&str>) -> Result<Option<String>, SignupContinuationError> {
    let Some(return_to) = return_to.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if return_to.len() > MAX_RETURN_TO_BYTES {
        return Err(SignupContinuationError);
    }
    let request =
        AuthRequest::parse_return_to(return_to).map_err(|_| SignupContinuationError)?;
    Ok(Some(request.return_to))
}

fn normalize_signup_name(name: &str) -> Option<&str> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_SIGNUP_NAME_CHARS {
        None
    } else {
        Some(name)
    }
}

fn render_signup_error(
    continuation: Option<&SignupContinuation>,
    cfg: &AuthConfig,
    err: &str,
) -> HttpResponse {
    render_signup_error_with_status(continuation, cfg, err, StatusCode::OK)
}

fn render_signup_bad_request(
    continuation: Option<&SignupContinuation>,
    cfg: &AuthConfig,
    err: &str,
) -> HttpResponse {
    render_signup_error_with_status(continuation, cfg, err, StatusCode::BAD_REQUEST)
}

fn render_signup_error_with_status(
    continuation: Option<&SignupContinuation>,
    cfg: &AuthConfig,
    err: &str,
    status: StatusCode,
) -> HttpResponse {
    let csrf_token = csrf::generate_token();
    let page = SignupPage {
        return_to: continuation.map(SignupContinuation::return_to).unwrap_or(""),
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
    fn redirect_to_login_url_encodes_return_to() {
        let return_to =
            "/oauth2/authorize?client_id=oac_123&redirect_uri=https%3A%2F%2Fapp.test%2Fcb";
        let continuation = SignupContinuation::ReturnTo(return_to.into());
        let resp = redirect_to_login(&continuation);
        assert_eq!(
            location(&resp),
            "/login?return_to=%2Foauth2%2Fauthorize%3Fclient_id%3Doac_123%26redirect_uri%3Dhttps%253A%252F%252Fapp.test%252Fcb"
        );
    }

    #[test]
    fn signup_continuation_accepts_exactly_one_native_target() {
        let return_to =
            "/oauth2/authorize?client_id=oac_123&redirect_uri=https%3A%2F%2Fapp.test%2Fcb";
        assert_eq!(
            SignupContinuation::from_inputs(Some(return_to), None),
            Ok(SignupContinuation::ReturnTo(return_to.into()))
        );
        assert!(SignupContinuation::from_inputs(None, None).is_err());
        assert!(SignupContinuation::from_inputs(
            Some(return_to),
            Some(return_to)
        )
        .is_err());
    }

    #[test]
    fn signup_continuation_rejects_invalid_return_to_at_intake() {
        for bad in ["//evil.com", "https://evil.com", "/me"] {
            assert!(
                SignupContinuation::from_inputs(Some(bad), None).is_err(),
                "must reject {bad:?}"
            );
        }
    }

    #[test]
    fn normalize_signup_name_trims_and_accepts_limit() {
        let name = "A".repeat(200);
        assert_eq!(normalize_signup_name(" Ada "), Some("Ada"));
        assert_eq!(normalize_signup_name(&name), Some(name.as_str()));
    }

    #[test]
    fn normalize_signup_name_rejects_empty_and_long_values() {
        let name = "A".repeat(201);
        assert_eq!(normalize_signup_name("   "), None);
        assert_eq!(normalize_signup_name(&name), None);
    }
}
