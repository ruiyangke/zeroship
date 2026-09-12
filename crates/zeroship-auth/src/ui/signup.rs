//! `/signup` GET + POST handlers.
//!
//! Signup creates the `zeroship.users` row, then redirects back to `/login`
//! carrying a `return_to` continuation, so the new account lands wherever it
//! was headed once it signs in.
//!
//! The continuation is sanitized by the SAME rule `/login` uses -- one call
//! to `return_to::sanitize(raw, return_to::SAFE_DEFAULT)`. Any target that is
//! absent, over-long, or not a same-origin path becomes `SAFE_DEFAULT`; none
//! of them is an error. This is not decoration: `/login` renders
//! `href="/signup?return_to={{ return_to }}"` with its own already-sanitized
//! value, which on a plain visit is `SAFE_DEFAULT` (`/me`). Signup used to
//! demand that the continuation parse as an `/oauth2/authorize` request and
//! that exactly one copy of it be present, so BOTH the link the login page
//! renders and a bare `/signup` answered 400 "invalid request" on a page that
//! still drew the form. Only an OIDC RP continuation could reach signup at
//! all.
//!
//! Account-enumeration defense: a duplicate-email INSERT is treated the
//! same as a fresh INSERT (same redirect, same status, same response). The
//! attacker can already learn this via `/login` timing if we leaked here --
//! and the OWASP guidance is to make signup look uniform.

use askama::Template;
use ntex::http::header::{HeaderValue, COOKIE, LOCATION, SET_COOKIE};
use ntex::http::StatusCode;
use ntex::web::{HttpRequest, HttpResponse};
use serde::Deserialize;
use std::sync::Arc;

use crate::audit::{self, AuditEvent};
use crate::config::AuthConfig;
use crate::csrf;
use crate::identity::{email as email_validation, password, verification};
use crate::return_to;
use crate::store::users;
use crate::ui::{ErrorPage, PublicErrorMessage, SignupPage};
use zeroship_authn::rate_limit::{self, Quota, RateLimitDecision};
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
pub async fn get(query: ntex::web::types::Query<SignupQuery>) -> HttpResponse {
    let return_to = continuation(query.return_to.as_deref());
    let csrf_token = csrf::generate_token();
    let page = SignupPage {
        return_to: &return_to,
        csrf: &csrf_token,
        error: None,
    };
    let body = page
        .render()
        .unwrap_or_else(|_| "<h1>error</h1>".to_string());
    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token));
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
    // Form field first, query second -- the same precedence `/login`'s POST
    // uses, so a form that echoes its hidden field wins over a stale query.
    let return_to = continuation(form.return_to.as_deref().or(query.return_to.as_deref()));

    // 1. CSRF.
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let cookie_token = csrf::parse_cookie(cookie_header);
    if cookie_token
        .as_deref()
        .is_none_or(|c| !csrf::matches(&form.csrf, c))
    {
        return render_signup_error(&return_to, "invalid request");
    }

    // 2. Password length (NIST 800-63B Rev 4: 15 char minimum). We count
    // characters, not bytes, so multibyte passphrases aren't penalised.
    if form.password.chars().count() < crate::identity::password::MIN_PASSWORD_CHARS {
        return render_signup_error(&return_to, "password must be at least 15 characters");
    }

    // 3. Email sanity before we enter rate limits, hashing, or storage.
    let email = form.email.trim().to_ascii_lowercase();
    if email_validation::validate_email(&email).is_err() {
        return render_signup_bad_request(&return_to, "enter a valid email");
    }

    // 4. Name sanity before entering rate limits, hashing, or storage.
    let Some(name) = normalize_signup_name(&form.name) else {
        return render_signup_bad_request(&return_to, "name must be 1-200 characters");
    };
    let name = name.to_string();

    // 5. Rate-limit per IP before entering the CPU-bound password hash.
    //    Use the forwarded client IP (auth runs behind the gateway, so the
    //    socket peer is the gateway — keying on it would make this a single
    //    global bucket). Matches link.rs and the audit RequestContext.
    let ip = crate::headers::client_ip(&req);
    let signup_ip_key = format!("signup_ip:{ip}");
    match rate_limit::consume(db.as_ref(), &signup_ip_key, Quota::SIGNUP_IP).await {
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
            return redirect_to_login(&return_to);
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
        match verification::issue(db.as_ref(), &user.id, &user.email).await {
            Ok(issued) => {
                let link = format!("{}/verify?token={}", cfg.public_url(), issued.raw,);
                let name_hint = user.name.split_whitespace().next().unwrap_or("there");

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
                        email: cfg.settings.mail_from_email.get().clone(),
                        name: Some(cfg.settings.mail_from_name.get().clone()),
                    },
                    "Verify your zeroship email".into(),
                    text,
                    html,
                    vec!["verification".into()],
                );
                if let Err(e) = mailer.send(db.as_ref(), msg).await {
                    tracing::warn!(error = %e, user_id = user.id.as_str(), "verification email send failed");
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
                tracing::error!(error = %e, user_id = user.id.as_str(), "verification token issue failed");
                // Don't fail the signup — the user can request resend later.
            }
        }
    }

    // 8. Redirect to /login carrying the same continuation so the user can
    // immediately sign in. The verification email is in their inbox; verifying
    // is decoupled from sign-in.
    redirect_to_login(&return_to)
}

/// The post-signup continuation.
///
/// One rule, shared with `/login`: keep a same-origin path, otherwise fall
/// back to `return_to::SAFE_DEFAULT`. Absent, over-long, cross-origin and
/// control-character targets all take the fallback -- none of them is an
/// error, because a signup form the user can see but never submit is worse
/// than a signup that lands on the default page.
///
/// The length bound is applied before `sanitize` so an absurd query string
/// cannot be echoed back into the rendered form or a `Location` header.
fn continuation(raw: Option<&str>) -> String {
    let bounded = raw.filter(|value| value.len() <= MAX_RETURN_TO_BYTES);
    return_to::sanitize(bounded, return_to::SAFE_DEFAULT)
}

fn redirect_to_login(return_to: &str) -> HttpResponse {
    let to = return_to::login_location(return_to);
    let mut resp = HttpResponse::Found();
    resp.header(
        LOCATION,
        HeaderValue::from_str(&to).unwrap_or_else(|_| HeaderValue::from_static("/login")),
    );
    resp.finish()
}

fn normalize_signup_name(name: &str) -> Option<&str> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_SIGNUP_NAME_CHARS {
        None
    } else {
        Some(name)
    }
}

fn render_signup_error(return_to: &str, err: &str) -> HttpResponse {
    render_signup_error_with_status(return_to, err, StatusCode::OK)
}

fn render_signup_bad_request(return_to: &str, err: &str) -> HttpResponse {
    render_signup_error_with_status(return_to, err, StatusCode::BAD_REQUEST)
}

fn render_signup_error_with_status(return_to: &str, err: &str, status: StatusCode) -> HttpResponse {
    let csrf_token = csrf::generate_token();
    let page = SignupPage {
        return_to,
        csrf: &csrf_token,
        error: Some(err),
    };
    let body = page.render().unwrap_or_else(|_| format!("<h1>{err}</h1>"));
    let mut resp = HttpResponse::build(status);
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token));
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
        let resp = redirect_to_login(return_to);
        assert_eq!(
            location(&resp),
            "/login?return_to=%2Foauth2%2Fauthorize%3Fclient_id%3Doac_123%26redirect_uri%3Dhttps%253A%252F%252Fapp.test%252Fcb"
        );
    }

    /// THE regression: `/login` renders `href="/signup?return_to=/me"` from
    /// its own sanitized value, and `/me` is not an `/oauth2/authorize`
    /// request. The old intake called that an error, so the login page's own
    /// link answered 400.
    #[test]
    fn continuation_keeps_the_target_the_login_page_links_to() {
        assert_eq!(continuation(Some(return_to::SAFE_DEFAULT)), "/me");
        assert_eq!(continuation(Some("/me")), "/me");
        assert_eq!(continuation(Some("/apps/new")), "/apps/new");
    }

    #[test]
    fn continuation_keeps_a_native_authorize_target() {
        let return_to =
            "/oauth2/authorize?client_id=oac_123&redirect_uri=https%3A%2F%2Fapp.test%2Fcb";
        assert_eq!(continuation(Some(return_to)), return_to);
    }

    /// An absent continuation is the bare `/signup` visit. It used to be an
    /// error too: `from_inputs` required exactly one target, and zero is not
    /// one, so `return_to: Option<String>` was a lie.
    #[test]
    fn continuation_falls_back_when_absent_or_empty() {
        assert_eq!(continuation(None), return_to::SAFE_DEFAULT);
        assert_eq!(continuation(Some("")), return_to::SAFE_DEFAULT);
        assert_eq!(continuation(Some("   ")), return_to::SAFE_DEFAULT);
    }

    /// Falling back is not the same as accepting: an off-origin or
    /// control-character target must never survive into the form or the
    /// `Location` header, it must be REPLACED by the safe default.
    #[test]
    fn continuation_replaces_unsafe_targets_with_the_safe_default() {
        for bad in [
            "//evil.com",
            "///evil.com",
            "https://evil.com",
            "/\\evil.com",
            "evil.com",
            "/me\r\nSet-Cookie: x=1",
            "/me\u{0000}foo",
        ] {
            assert_eq!(
                continuation(Some(bad)),
                return_to::SAFE_DEFAULT,
                "must not carry {bad:?} forward"
            );
        }
    }

    #[test]
    fn continuation_falls_back_on_an_over_long_target() {
        let long = format!("/{}", "a".repeat(MAX_RETURN_TO_BYTES));
        assert!(long.len() > MAX_RETURN_TO_BYTES);
        assert_eq!(continuation(Some(&long)), return_to::SAFE_DEFAULT);

        let at_limit = format!("/{}", "a".repeat(MAX_RETURN_TO_BYTES - 1));
        assert_eq!(at_limit.len(), MAX_RETURN_TO_BYTES);
        assert_eq!(continuation(Some(&at_limit)), at_limit);
    }

    /// The redirect a signup actually issues, for the default continuation.
    /// Before the fix this code path was unreachable from the login page.
    #[test]
    fn redirect_to_login_carries_the_safe_default() {
        let resp = redirect_to_login(&continuation(None));
        assert_eq!(location(&resp), "/login?return_to=%2Fme");
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
