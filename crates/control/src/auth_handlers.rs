//! Auth API handlers — register, login, userinfo, consent, logout, authorize.

use std::sync::Arc;

use ntex::http::header;
use ntex::web;
use ntex::web::types::{Json, Query, State};
use serde::Deserialize;

use crate::auth_service::TokenClaims;
use crate::AppState;

// ---------------------------------------------------------------------------
// Cookie helpers
// ---------------------------------------------------------------------------

const COOKIE_NAME: &str = "__zs_session";

/// Extract the `__zs_session` value from the raw Cookie header.
fn extract_session_cookie(req: &web::HttpRequest) -> Option<String> {
    let header_val = req.headers().get(header::COOKIE)?.to_str().ok()?;
    for part in header_val.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix(COOKIE_NAME) {
            let value = value.strip_prefix('=')?;
            return Some(value.to_string());
        }
    }
    None
}

/// Build a Set-Cookie header value for setting the session cookie.
fn set_cookie_header(token: &str) -> String {
    format!(
        "{COOKIE_NAME}={token}; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=86400"
    )
}

/// Build a Set-Cookie header value that clears the session cookie.
fn clear_cookie_header() -> String {
    format!("{COOKIE_NAME}=; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=0")
}

/// Validate the session cookie and return claims, or None.
fn validate_session(req: &web::HttpRequest, state: &AppState) -> Option<TokenClaims> {
    let token = extract_session_cookie(req)?;
    state.auth.verify_token(&token).ok()
}

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct RegisterBody {
    pub email: String,
    pub password: String,
    pub name: String,
}

#[derive(Deserialize)]
pub struct LoginBody {
    pub email: String,
    pub password: String,
    pub app_id: String,
}

#[derive(Deserialize)]
pub struct ConsentBody {
    pub app_id: String,
}

#[derive(Deserialize)]
pub struct AuthorizeQuery {
    pub app_id: Option<String>,
    #[serde(rename = "return")]
    pub return_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// POST /auth/register — create a new user account.
pub async fn register(
    state: State<Arc<AppState>>,
    body: Json<RegisterBody>,
) -> web::HttpResponse {
    match state
        .auth
        .register(&body.email, &body.password, &body.name)
        .await
    {
        Ok(user) => web::HttpResponse::Created().json(&serde_json::json!({ "user": user })),
        Err(msg) => {
            if msg.contains("already registered") {
                web::HttpResponse::Conflict()
                    .json(&serde_json::json!({ "error": msg }))
            } else if msg.contains("invalid") || msg.contains("must be") || msg.contains("required")
            {
                web::HttpResponse::BadRequest()
                    .json(&serde_json::json!({ "error": msg }))
            } else {
                web::HttpResponse::InternalServerError()
                    .json(&serde_json::json!({ "error": msg }))
            }
        }
    }
}

/// POST /auth/login — authenticate and receive a session cookie.
pub async fn login(
    state: State<Arc<AppState>>,
    body: Json<LoginBody>,
) -> web::HttpResponse {
    match state
        .auth
        .login(&body.email, &body.password, &body.app_id)
        .await
    {
        Ok(result) => {
            let cookie = set_cookie_header(&result.token);
            web::HttpResponse::Ok()
                .header(header::SET_COOKIE, cookie)
                .json(&serde_json::json!({
                    "token": result.token,
                    "user": result.user,
                }))
        }
        Err(msg) => {
            if msg.contains("invalid email or password") {
                web::HttpResponse::Unauthorized()
                    .json(&serde_json::json!({ "error": msg }))
            } else {
                web::HttpResponse::InternalServerError()
                    .json(&serde_json::json!({ "error": msg }))
            }
        }
    }
}

/// GET /auth/userinfo — return the authenticated user from the session cookie.
pub async fn userinfo(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    let claims = match validate_session(&req, &state) {
        Some(c) => c,
        None => {
            return web::HttpResponse::Unauthorized()
                .json(&serde_json::json!({ "error": "not authenticated" }));
        }
    };

    match state.auth.get_user(claims.sub).await {
        Ok(user) => web::HttpResponse::Ok().json(&serde_json::json!({
            "user": user,
            "app": claims.app,
        })),
        Err(msg) => {
            web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({ "error": msg }))
        }
    }
}

/// POST /auth/consent — grant consent for the authenticated user to an app.
pub async fn consent(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    body: Json<ConsentBody>,
) -> web::HttpResponse {
    let claims = match validate_session(&req, &state) {
        Some(c) => c,
        None => {
            return web::HttpResponse::Unauthorized()
                .json(&serde_json::json!({ "error": "not authenticated" }));
        }
    };

    match state.auth.grant_consent(claims.sub, &body.app_id).await {
        Ok(()) => web::HttpResponse::Ok()
            .json(&serde_json::json!({ "granted": true, "app_id": body.app_id })),
        Err(msg) => {
            web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({ "error": msg }))
        }
    }
}

/// POST /auth/logout — clear the session cookie.
pub async fn logout() -> web::HttpResponse {
    let cookie = clear_cookie_header();
    web::HttpResponse::Ok()
        .header(header::SET_COOKIE, cookie)
        .json(&serde_json::json!({ "logged_out": true }))
}

/// GET /auth/authorize — consent flow entry point.
///
/// Query params: `app_id`, `return` (the URL to redirect back to after auth).
/// For now this returns a JSON response indicating the authorize endpoint;
/// a full HTML page will be added when the platform UI is built.
pub async fn authorize(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    query: Query<AuthorizeQuery>,
) -> web::HttpResponse {
    let app_id = query.app_id.as_deref().unwrap_or("");
    let return_url = query.return_url.as_deref().unwrap_or("/");

    // If the user already has a valid session with consent, redirect back.
    if let Some(claims) = validate_session(&req, &state) {
        if let Ok(true) = state.auth.has_consent(claims.sub, app_id).await {
            return web::HttpResponse::Found()
                .header(header::LOCATION, return_url)
                .finish();
        }
    }

    // Otherwise, return the authorize info. In production this would render
    // the login/consent HTML page.
    web::HttpResponse::Ok().json(&serde_json::json!({
        "authorize": true,
        "app_id": app_id,
        "return": return_url,
        "message": "Login required. POST /auth/login to authenticate.",
    }))
}
