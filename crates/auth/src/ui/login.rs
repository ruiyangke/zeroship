//! `/login` GET handler.
//!
//! Algorithm (P2 §8.1 GET path):
//!
//! 1. Read `login_challenge` from the query.
//! 2. Fetch challenge metadata from hydra via `admin.get_login(challenge)`.
//! 3. If `info.skip == true`, hydra already has a session for this subject —
//!    immediately call `accept_login` and redirect back to hydra.
//! 4. Otherwise render the form with a fresh CSRF token cookie.
//!
//! Route wiring happens in P2-U6 (`server::configure`); the handler here is
//! a plain `pub async fn` with no `#[ntex::web::*]` attribute.

use askama::Template;
use ntex::http::header::{HeaderValue, COOKIE, LOCATION, SET_COOKIE};
use ntex::web::{HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use crate::audit::{self, AuditEvent};
use crate::config::AuthConfig;
use crate::csrf;
use crate::hydra_client::types::AcceptLoginRequest;
use crate::hydra_client::HydraAdmin;
use crate::identity::password;
use crate::ratelimit::{self, Bucket};
use crate::sessions::login as session_cookie;
use crate::store::{sessions, users};
use crate::ui::LoginPage;

#[derive(Debug, Deserialize)]
pub struct LoginQuery {
    pub login_challenge: String,
}

// ntex's per-thread service futures are intentionally `!Send` (Rc-based
// internal state). Marking each handler `#[allow(clippy::future_not_send)]`
// is the canonical workaround; the lint is structural, not actionable.
#[allow(clippy::future_not_send)]
pub async fn get(
    req: HttpRequest,
    query: ntex::web::types::Query<LoginQuery>,
    admin: ntex::web::types::State<HydraAdmin>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
) -> HttpResponse {
    let _ = req; // header extraction (UA, request-id) lands in later phases.
    let challenge = &query.login_challenge;

    // Fetch challenge details from hydra.
    let info = match admin.get_login(challenge).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, challenge = %challenge, "login challenge fetch failed");
            return render_error("invalid login request", Some(&e.to_string()));
        }
    };

    // Skip path: hydra already knows the subject.
    if info.skip {
        let accept = AcceptLoginRequest {
            subject: info.subject.clone(),
            remember: Some(true),
            remember_for: Some(3600),
            ..Default::default()
        };
        match admin.accept_login(challenge, &accept).await {
            Ok(resp) => return redirect(&resp.redirect_to),
            Err(e) => {
                tracing::error!(error = %e, "accept_login (skip path) failed");
                return render_error("internal error", Some(&e.to_string()));
            }
        }
    }

    // Render the form with a fresh CSRF token cookie.
    let csrf_token = csrf::generate_token();
    let page = LoginPage {
        challenge,
        csrf: &csrf_token,
        error: None,
        client_name: info.client.client_name.as_deref().unwrap_or(&info.client.client_id),
        google_enabled: cfg.google_client_id.is_some(),
        github_enabled: cfg.github_client_id.is_some(),
    };
    let body = match page.render() {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "render login.html failed");
            return render_error("internal error", Some("template render"));
        }
    };

    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token, cfg.insecure_dev));
    resp.body(body)
}

fn redirect(to: &str) -> HttpResponse {
    let mut resp = HttpResponse::Found();
    resp.header(
        LOCATION,
        HeaderValue::from_str(to).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    resp.finish()
}

fn render_error(error: &str, error_description: Option<&str>) -> HttpResponse {
    use crate::ui::ErrorPage;
    let page = ErrorPage { error, error_description };
    let body = page.render().unwrap_or_else(|_| format!("<h1>{error}</h1>"));
    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.body(body)
}

// ─── POST /login ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct LoginForm {
    pub csrf: String,
    pub email: String,
    pub password: String,
}

/// `/login` POST — credential flow per proposal §8.1.
///
/// 1. CSRF (form vs `__Host-zsidp_csrf` cookie).
/// 2. Rate-limit (3 buckets in order: email+ip, email, ip).
/// 3. User lookup by email; dummy-hash fallback when user absent / locked /
///    has no `password_hash` (account-enumeration defense).
/// 4. Argon2 verify wrapped in `compio::runtime::spawn_blocking` (~100 ms,
///    must not block the ntex event loop).
/// 5. On success: insert `auth.sessions`, set `__Host-zsidp_session` cookie,
///    `accept_login` to hydra, 302 to hydra's `redirect_to`.
/// 6. On failure: re-render the form with a status code matching the
///    failure mode (400 / 401 / 429 / 500). Cookies refreshed so the form
///    stays usable for a retry.
#[allow(clippy::too_many_lines)]
// ntex's per-thread service futures are intentionally `!Send`. See note on
// `get`.
#[allow(clippy::future_not_send)]
pub async fn post(
    req: HttpRequest,
    query: ntex::web::types::Query<LoginQuery>,
    form: ntex::web::types::Form<LoginForm>,
    admin: ntex::web::types::State<HydraAdmin>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    let challenge = query.login_challenge.clone();

    // Best-effort fetch of client_name so the re-rendered LoginPage on
    // failure still shows the relying-party label. If hydra is unreachable
    // we render an opaque error page; the login flow cannot proceed without
    // a challenge anyway.
    let info = match admin.get_login(&challenge).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, challenge = %challenge, "POST /login: get_login failed");
            return render_error("invalid login request", Some(&e.to_string()));
        }
    };
    let client_id = info.client.client_id.clone();
    let client_name = info
        .client
        .client_name
        .clone()
        .unwrap_or_else(|| client_id.clone());

    // 1. CSRF — cheapest check first.
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
        return render_login_error(
            &challenge,
            &client_name,
            &cfg,
            "invalid request",
            400,
        );
    }

    // 2. Rate limit (3 buckets, deepest scope first).
    let ip = req
        .connection_info()
        .remote()
        .unwrap_or("0.0.0.0")
        .to_string();
    let email_norm = form.email.trim().to_ascii_lowercase();
    let buckets = [
        (format!("login:eip:{email_norm}:{ip}"), Bucket::LOGIN_EIP),
        (format!("login:email:{email_norm}"), Bucket::LOGIN_EMAIL),
        (format!("login:ip:{ip}"), Bucket::LOGIN_IP),
    ];
    for (key, bucket) in &buckets {
        match ratelimit::consume(db.as_ref(), key, *bucket).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                audit::emit(
                    db.as_ref(),
                    &AuditEvent {
                        event_type: "login_failure",
                        outcome: "failure",
                        client_id: Some(&client_id),
                        auth_method: Some("pwd"),
                        detail: json!({ "reason": "rate_limited", "bucket": key }),
                        ..Default::default()
                    },
                )
                .await;
                return render_login_error(
                    &challenge,
                    &client_name,
                    &cfg,
                    "too many attempts, try again later",
                    429,
                );
            }
            Err(e) => {
                tracing::error!(error = %e, bucket = %key, "rate-limit consume failed");
                return render_error("internal error", Some("rate limit"));
            }
        }
    }

    // 3. Look up user.
    let user = match users::find_by_email(db.as_ref(), &email_norm).await {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(error = %e, "users::find_by_email failed");
            return render_error("internal error", Some("db"));
        }
    };

    // Constant-time enumeration defense: if user is None, locked, or has no
    // password hash (OAuth-only), verify against the dummy hash so the wall
    // time matches a real verify.
    let now = chrono::Utc::now();
    let phc = user
        .as_ref()
        .and_then(|u| {
            let locked = u.locked_until.is_some_and(|t| t > now);
            if locked || u.password_hash.is_none() {
                None
            } else {
                u.password_hash.clone()
            }
        })
        .unwrap_or_else(|| password::dummy_hash().to_string());

    // 4. Argon2 verify — CPU-bound, run on spawn_blocking so the event loop
    // is not parked.
    let password_clone = form.password.clone();
    let valid = compio::runtime::spawn_blocking(move || {
        password::verify(&password_clone, &phc).unwrap_or(false)
    })
    .await
    .unwrap_or(false);

    // Re-evaluate the "real user" predicate (mirror the dummy-hash arm).
    let real_user = user.as_ref().filter(|u| {
        u.locked_until.is_none_or(|t| t <= now) && u.password_hash.is_some()
    });

    let Some(u) = real_user else {
        audit::emit(
            db.as_ref(),
            &AuditEvent {
                event_type: "login_failure",
                outcome: "failure",
                client_id: Some(&client_id),
                auth_method: Some("pwd"),
                detail: json!({ "reason": "invalid_credentials" }),
                ..Default::default()
            },
        )
        .await;
        return render_login_error(
            &challenge,
            &client_name,
            &cfg,
            "invalid email or password",
            401,
        );
    };

    if !valid {
        audit::emit(
            db.as_ref(),
            &AuditEvent {
                event_type: "login_failure",
                outcome: "failure",
                user_id: Some(&u.id),
                client_id: Some(&client_id),
                auth_method: Some("pwd"),
                detail: json!({ "reason": "invalid_credentials" }),
                ..Default::default()
            },
        )
        .await;
        return render_login_error(
            &challenge,
            &client_name,
            &cfg,
            "invalid email or password",
            401,
        );
    }

    // 5. Success path.
    //
    // 5a. Create the IdP session row.
    let session = match sessions::create(
        db.as_ref(),
        &sessions::CreateSession {
            user_id: u.id,
            auth_method: "pwd",
            amr: vec!["pwd".into()],
            acr: Some("urn:zeroship:pwd"),
            idle_minutes: session_cookie::IDLE_MINUTES,
            absolute_hours: session_cookie::ABSOLUTE_HOURS,
        },
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "sessions::create failed");
            return render_error("internal error", Some("session"));
        }
    };

    // 5b. Bump last_login_at (non-fatal on failure — we already audited the
    // success; the user should still flow through to hydra).
    if let Err(e) = users::touch_last_login(db.as_ref(), u.id).await {
        tracing::warn!(error = %e, user_id = %u.id, "touch_last_login failed");
    }

    // 5c. Accept the hydra login challenge.
    let accept = AcceptLoginRequest {
        subject: u.id.to_string(),
        remember: Some(true),
        remember_for: Some(3600),
        acr: Some("urn:zeroship:pwd".into()),
        amr: Some(vec!["pwd".into()]),
        ..Default::default()
    };
    let redirect_to = match admin.accept_login(&challenge, &accept).await {
        Ok(resp) => resp.redirect_to,
        Err(e) => {
            tracing::error!(error = %e, "accept_login failed");
            return render_error("internal error", Some("hydra"));
        }
    };

    // 5d. Audit the success.
    audit::emit(
        db.as_ref(),
        &AuditEvent {
            event_type: "login_success",
            outcome: "success",
            user_id: Some(&u.id),
            client_id: Some(&client_id),
            auth_method: Some("pwd"),
            detail: json!({}),
            ..Default::default()
        },
    )
    .await;

    // 5e. 302 with the session cookie + hydra's redirect_to as Location.
    let mut resp = HttpResponse::Found();
    resp.header(
        LOCATION,
        HeaderValue::from_str(&redirect_to)
            .unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    resp.header(
        SET_COOKIE,
        session_cookie::set_cookie(&session.id, cfg.insecure_dev),
    );
    resp.finish()
}

/// Re-render the login page with an error banner + fresh CSRF cookie, at the
/// given HTTP status. Identical body shape for every failure mode (the only
/// thing that varies is the visible message + status) so timing and content
/// don't leak which arm rejected the request.
fn render_login_error(
    challenge: &str,
    client_name: &str,
    cfg: &AuthConfig,
    err: &str,
    status: u16,
) -> HttpResponse {
    let csrf_token = csrf::generate_token();
    let page = LoginPage {
        challenge,
        csrf: &csrf_token,
        error: Some(err),
        client_name,
        google_enabled: cfg.google_client_id.is_some(),
        github_enabled: cfg.github_client_id.is_some(),
    };
    let body = page
        .render()
        .unwrap_or_else(|_| format!("<h1>{err}</h1>"));
    let code = ntex::http::StatusCode::from_u16(status)
        .unwrap_or(ntex::http::StatusCode::BAD_REQUEST);
    let mut resp = HttpResponse::build(code);
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token, cfg.insecure_dev));
    resp.body(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use askama::Template;

    #[test]
    fn login_page_oauth_buttons_gated_by_config() {
        let page = LoginPage {
            challenge: "abc",
            csrf: "xyz",
            error: None,
            client_name: "Test",
            google_enabled: true,
            github_enabled: false,
        };
        let html = page.render().expect("render");
        assert!(
            html.contains("/oauth/google/start"),
            "google button should be present"
        );
        assert!(
            !html.contains("/oauth/github/start"),
            "github button should be absent"
        );
        assert!(html.contains("Sign in with Google"));
    }

    #[test]
    fn login_page_hides_section_if_no_oauth() {
        let page = LoginPage {
            challenge: "abc",
            csrf: "xyz",
            error: None,
            client_name: "Test",
            google_enabled: false,
            github_enabled: false,
        };
        let html = page.render().expect("render");
        assert!(
            !html.contains("oauth-buttons"),
            "OAuth section should be hidden"
        );
        assert!(!html.contains("or sign in with"));
    }
}
