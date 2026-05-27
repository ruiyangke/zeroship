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
use ntex::web::{HttpRequest, HttpResponse};
use serde::Deserialize;
use std::sync::Arc;

use crate::config::AuthConfig;
use crate::csrf;
use crate::identity::password;
use crate::store::users;
use crate::ui::{ErrorPage, SignupPage};

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
    let challenge = query.login_challenge.as_deref().unwrap_or("");
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
#[allow(clippy::future_not_send)]
pub async fn post(
    req: HttpRequest,
    query: ntex::web::types::Query<SignupQuery>,
    form: ntex::web::types::Form<SignupForm>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    let challenge = query.login_challenge.clone().unwrap_or_default();

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

    // 3. Email basic sanity. Full validation belongs at the SMTP-verify
    // layer (Phase 5); this is just a typo-catch.
    let email = form.email.trim().to_ascii_lowercase();
    if !email.contains('@') || email.len() < 3 {
        return render_signup_error(&challenge, &cfg, "enter a valid email");
    }

    // 4. Hash the password — argon2 is CPU-bound, run on spawn_blocking so
    // the ntex event loop is not parked (same constraint as /login).
    let password_clone = form.password.clone();
    let phc = match compio::runtime::spawn_blocking(move || password::hash(&password_clone)).await {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            tracing::error!(error = %e, "signup password hash failed");
            return render_error_page("internal error", Some("hash"));
        }
        Err(_) => {
            tracing::error!("signup hash spawn_blocking panicked");
            return render_error_page("internal error", Some("hash"));
        }
    };

    // 5. Insert the user row. Account-enumeration defense: a duplicate
    // email is logged but produces the same response as a successful
    // insert — the attacker cannot probe email existence via this endpoint.
    let name = form.name.trim();
    if let Err(e) = users::create(db.as_ref(), &email, name, Some(&phc)).await {
        tracing::info!(error = %e, "signup users::create rejected (duplicate or otherwise)");
    }

    // 6. Redirect to /login carrying the same challenge so the user can
    // immediately sign in (Phase 5 will insert an email-verification step).
    let to = if challenge.is_empty() {
        "/login".to_string()
    } else {
        format!("/login?login_challenge={challenge}")
    };
    let mut resp = HttpResponse::Found();
    resp.header(
        LOCATION,
        HeaderValue::from_str(&to).unwrap_or_else(|_| HeaderValue::from_static("/login")),
    );
    resp.finish()
}

fn render_signup_error(challenge: &str, cfg: &AuthConfig, err: &str) -> HttpResponse {
    let csrf_token = csrf::generate_token();
    let page = SignupPage {
        challenge,
        csrf: &csrf_token,
        error: Some(err),
    };
    let body = page
        .render()
        .unwrap_or_else(|_| format!("<h1>{err}</h1>"));
    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token, cfg.insecure_dev));
    resp.body(body)
}

fn render_error_page(error: &str, error_description: Option<&str>) -> HttpResponse {
    let page = ErrorPage {
        error,
        error_description,
    };
    let body = page
        .render()
        .unwrap_or_else(|_| format!("<h1>{error}</h1>"));
    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.body(body)
}
