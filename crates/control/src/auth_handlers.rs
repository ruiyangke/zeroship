//! Auth API handlers — register, login, userinfo, consent, logout, OAuth.
//!
//! All session state lives in the `__zs_session` httpOnly cookie
//! (signed JWT, 24h). The dashboard never sees the raw token —
//! browser-managed cookie + same-origin via the vite proxy.

use std::sync::Arc;

use ntex::http::header;
use ntex::web;
use ntex::web::types::{Json, Query, State};
use serde::Deserialize;

use crate::auth_service::{OAuthProvider, TokenClaims};
use crate::oauth;
use crate::AppState;

// ---------------------------------------------------------------------------
// Cookie helpers
// ---------------------------------------------------------------------------

const SESSION_COOKIE: &str = "__zs_session";
const OAUTH_STATE_COOKIE: &str = "__zs_oauth_state";
const OAUTH_PKCE_COOKIE: &str = "__zs_oauth_pkce";
const OAUTH_RETURN_COOKIE: &str = "__zs_oauth_return";

fn extract_cookie(req: &web::HttpRequest, name: &str) -> Option<String> {
    let header_val = req.headers().get(header::COOKIE)?.to_str().ok()?;
    for part in header_val.split(';') {
        let part = part.trim();
        let prefix = format!("{name}=");
        if let Some(value) = part.strip_prefix(&prefix) {
            return Some(value.to_string());
        }
    }
    None
}

/// Same-Site=Lax / Secure / HttpOnly. We deliberately drop `Secure`
/// in dev mode (cleartext localhost) — browsers reject `Secure`
/// cookies on http://. In prod the dashboard is served over HTTPS
/// and we re-add it.
fn cookie_attrs(insecure_dev: bool) -> &'static str {
    if insecure_dev {
        "HttpOnly; SameSite=Lax; Path=/; Max-Age=86400"
    } else {
        "HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=86400"
    }
}

fn set_session_cookie(token: &str, insecure_dev: bool) -> String {
    format!("{SESSION_COOKIE}={token}; {}", cookie_attrs(insecure_dev))
}
fn clear_session_cookie(insecure_dev: bool) -> String {
    let attrs = cookie_attrs(insecure_dev);
    format!("{SESSION_COOKIE}=; {attrs}; Max-Age=0")
}
fn set_short_lived_cookie(name: &str, value: &str, insecure_dev: bool) -> String {
    let secure = if insecure_dev { "" } else { " Secure;" };
    format!("{name}={value}; HttpOnly;{secure} SameSite=Lax; Path=/; Max-Age=600")
}
fn clear_short_lived_cookie(name: &str, insecure_dev: bool) -> String {
    let secure = if insecure_dev { "" } else { " Secure;" };
    format!("{name}=; HttpOnly;{secure} SameSite=Lax; Path=/; Max-Age=0")
}

/// Validate the session cookie and return claims, or None.
pub fn validate_session(req: &web::HttpRequest, state: &AppState) -> Option<TokenClaims> {
    let token = extract_cookie(req, SESSION_COOKIE)?;
    state.auth.verify_token(&token).ok()
}

// ---------------------------------------------------------------------------
// Request bodies / queries
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct RegisterBody { pub email: String, pub password: String, pub name: String }

#[derive(Deserialize)]
pub struct LoginBody {
    pub email: String,
    pub password: String,
    /// Optional. Omitted = creator (dashboard) session.
    #[serde(default)]
    pub app_id: Option<String>,
}

#[derive(Deserialize)]
pub struct ConsentBody { pub app_id: String }

#[derive(Deserialize)]
pub struct AuthorizeQuery {
    pub app_id: Option<String>,
    #[serde(rename = "return")]
    pub return_url: Option<String>,
}

#[derive(Deserialize)]
pub struct OAuthStartQuery {
    /// URL to bounce back to after OAuth completes. Defaults to "/".
    /// Validated to be a same-origin path so we can't be used as an
    /// open-redirect oracle.
    #[serde(rename = "return")]
    pub return_url: Option<String>,
}

#[derive(Deserialize)]
pub struct OAuthCallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Email + password handlers
// ---------------------------------------------------------------------------

/// POST /auth/register — create an account.
pub async fn register(
    state: State<Arc<AppState>>,
    body: Json<RegisterBody>,
) -> web::HttpResponse {
    match state.auth.register(&body.email, &body.password, &body.name).await {
        Ok(user) => web::HttpResponse::Created().json(&serde_json::json!({ "user": user })),
        Err(msg) => {
            if msg.contains("already registered") {
                web::HttpResponse::Conflict().json(&serde_json::json!({ "error": msg }))
            } else if msg.contains("invalid") || msg.contains("must be") || msg.contains("required") {
                web::HttpResponse::BadRequest().json(&serde_json::json!({ "error": msg }))
            } else {
                web::HttpResponse::InternalServerError().json(&serde_json::json!({ "error": msg }))
            }
        }
    }
}

/// POST /auth/login — email + password → session cookie.
pub async fn login(
    state: State<Arc<AppState>>,
    body: Json<LoginBody>,
) -> web::HttpResponse {
    let app_id = body.app_id.as_deref();
    match state.auth.login(&body.email, &body.password, app_id).await {
        Ok(result) => {
            let cookie = set_session_cookie(&result.token, state.insecure_dev);
            web::HttpResponse::Ok()
                .header(header::SET_COOKIE, cookie)
                .json(&serde_json::json!({ "user": result.user }))
        }
        Err(msg) => {
            if msg.contains("invalid email or password") || msg.contains("registered via") {
                web::HttpResponse::Unauthorized().json(&serde_json::json!({ "error": msg }))
            } else {
                web::HttpResponse::InternalServerError().json(&serde_json::json!({ "error": msg }))
            }
        }
    }
}

/// GET /auth/userinfo — { user, app? } if authed; 401 otherwise.
pub async fn userinfo(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    let claims = match validate_session(&req, &state) {
        Some(c) => c,
        None => return web::HttpResponse::Unauthorized()
            .json(&serde_json::json!({ "error": "not authenticated" })),
    };
    match state.auth.get_user(&claims.sub).await {
        Ok(user) => web::HttpResponse::Ok().json(&serde_json::json!({
            "user": user,
            "app": claims.app,
        })),
        Err(msg) => web::HttpResponse::InternalServerError()
            .json(&serde_json::json!({ "error": msg })),
    }
}

pub async fn consent(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    body: Json<ConsentBody>,
) -> web::HttpResponse {
    let claims = match validate_session(&req, &state) {
        Some(c) => c,
        None => return web::HttpResponse::Unauthorized()
            .json(&serde_json::json!({ "error": "not authenticated" })),
    };
    match state.auth.grant_consent(&claims.sub, &body.app_id).await {
        Ok(()) => web::HttpResponse::Ok()
            .json(&serde_json::json!({ "granted": true, "app_id": body.app_id })),
        Err(msg) => web::HttpResponse::InternalServerError()
            .json(&serde_json::json!({ "error": msg })),
    }
}

pub async fn logout(state: State<Arc<AppState>>) -> web::HttpResponse {
    let cookie = clear_session_cookie(state.insecure_dev);
    web::HttpResponse::Ok()
        .header(header::SET_COOKIE, cookie)
        .json(&serde_json::json!({ "logged_out": true }))
}

pub async fn authorize(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    query: Query<AuthorizeQuery>,
) -> web::HttpResponse {
    let app_id = query.app_id.as_deref().unwrap_or("");
    let return_url = query.return_url.as_deref().unwrap_or("/");

    if let Some(claims) = validate_session(&req, &state) {
        if let Ok(true) = state.auth.has_consent(&claims.sub, app_id).await {
            return web::HttpResponse::Found()
                .header(header::LOCATION, return_url)
                .finish();
        }
    }
    web::HttpResponse::Ok().json(&serde_json::json!({
        "authorize": true,
        "app_id": app_id,
        "return": return_url,
        "message": "Login required. POST /auth/login to authenticate.",
    }))
}

// ---------------------------------------------------------------------------
// Google OAuth handlers
// ---------------------------------------------------------------------------

/// GET /auth/google/start?return=/p/abc/chat
pub async fn google_start(
    state: State<Arc<AppState>>,
    query: Query<OAuthStartQuery>,
) -> web::HttpResponse {
    let cfg = match &state.google_oauth {
        Some(c) => c,
        None => return web::HttpResponse::ServiceUnavailable()
            .json(&serde_json::json!({ "error": "google oauth not configured" })),
    };

    let auth = oauth::start_authorize_url(cfg);
    let return_url = sanitize_return_url(query.return_url.as_deref());

    web::HttpResponse::Found()
        .header(header::SET_COOKIE, set_short_lived_cookie(OAUTH_STATE_COOKIE, &auth.state, state.insecure_dev))
        .header(header::SET_COOKIE, set_short_lived_cookie(OAUTH_PKCE_COOKIE, &auth.pkce_verifier, state.insecure_dev))
        .header(header::SET_COOKIE, set_short_lived_cookie(OAUTH_RETURN_COOKIE, &return_url, state.insecure_dev))
        .header(header::LOCATION, auth.url)
        .finish()
}

/// GET /auth/google/callback?code=...&state=...
pub async fn google_callback(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    query: Query<OAuthCallbackQuery>,
) -> web::HttpResponse {
    let cfg = match &state.google_oauth {
        Some(c) => c,
        None => return web::HttpResponse::ServiceUnavailable()
            .json(&serde_json::json!({ "error": "google oauth not configured" })),
    };

    if let Some(err) = &query.error {
        return web::HttpResponse::Found()
            .header(header::LOCATION, format!("/login?error={}", url::form_urlencoded::byte_serialize(err.as_bytes()).collect::<String>()))
            .finish();
    }

    let stored_state = extract_cookie(&req, OAUTH_STATE_COOKIE)
        .unwrap_or_default();
    let stored_pkce = extract_cookie(&req, OAUTH_PKCE_COOKIE)
        .unwrap_or_default();
    let return_url = extract_cookie(&req, OAUTH_RETURN_COOKIE)
        .unwrap_or_else(|| "/".to_string());

    let code = match query.code.as_deref() {
        Some(c) => c,
        None => return bad_oauth("missing code"),
    };
    let state_param = match query.state.as_deref() {
        Some(s) => s,
        None => return bad_oauth("missing state"),
    };
    if stored_state.is_empty() || stored_state != state_param {
        return bad_oauth("state mismatch (csrf)");
    }
    if stored_pkce.is_empty() {
        return bad_oauth("missing pkce verifier");
    }

    let identity = match oauth::complete_callback(cfg, code, &stored_pkce).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, "control/auth: google callback failed");
            return bad_oauth("oauth exchange failed");
        }
    };

    let result = match state.auth.oauth_login(
        OAuthProvider::Google,
        &identity.subject,
        &identity.email,
        &identity.name,
        identity.avatar_url.as_deref(),
        None, // creator session
    ).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "control/auth: oauth_login failed");
            return web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({ "error": e }));
        }
    };

    let mut resp = web::HttpResponse::Found();
    resp.header(header::SET_COOKIE, set_session_cookie(&result.token, state.insecure_dev));
    resp.header(header::SET_COOKIE, clear_short_lived_cookie(OAUTH_STATE_COOKIE, state.insecure_dev));
    resp.header(header::SET_COOKIE, clear_short_lived_cookie(OAUTH_PKCE_COOKIE, state.insecure_dev));
    resp.header(header::SET_COOKIE, clear_short_lived_cookie(OAUTH_RETURN_COOKIE, state.insecure_dev));
    resp.header(header::LOCATION, return_url);
    resp.finish()
}

fn bad_oauth(msg: &str) -> web::HttpResponse {
    web::HttpResponse::Found()
        .header(header::LOCATION,
                format!("/login?error={}",
                        url::form_urlencoded::byte_serialize(msg.as_bytes()).collect::<String>()))
        .finish()
}

/// Reject anything that smells like an absolute URL or `//host` —
/// only accept paths that start with a single `/`.
fn sanitize_return_url(input: Option<&str>) -> String {
    let raw = input.unwrap_or("/");
    if raw.starts_with("//") || raw.contains("://") || !raw.starts_with('/') {
        return "/".into();
    }
    raw.to_string()
}
