//! Auth API handlers — register, login, userinfo, consent, logout, authorize, OAuth.

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use ntex::http::header;
use ntex::web;
use ntex::web::types::{Json, Query, State};
use openidconnect::{Nonce, PkceCodeVerifier};
use serde::Deserialize;

use crate::service::TokenClaims;
use crate::AppState;

// ---------------------------------------------------------------------------
// Cookie helpers
// ---------------------------------------------------------------------------

const COOKIE_NAME: &str = "__zs_session";
const PKCE_COOKIE: &str = "__zs_pkce";
const NONCE_COOKIE: &str = "__zs_nonce";

/// Extract a named cookie value from the raw Cookie header.
fn extract_cookie(req: &web::HttpRequest, name: &str) -> Option<String> {
    let header_val = req.headers().get(header::COOKIE)?.to_str().ok()?;
    for part in header_val.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix(name) {
            let value = value.strip_prefix('=')?;
            return Some(value.to_string());
        }
    }
    None
}

/// Extract the `__zs_session` value from the raw Cookie header.
fn extract_session_cookie(req: &web::HttpRequest) -> Option<String> {
    extract_cookie(req, COOKIE_NAME)
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

/// Build a short-lived cookie for storing the PKCE verifier during OAuth flow.
fn pkce_cookie_header(verifier: &str) -> String {
    format!(
        "{PKCE_COOKIE}={verifier}; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=300"
    )
}

/// Build a short-lived cookie for storing the OIDC nonce during OAuth flow.
fn nonce_cookie_header(nonce: &str) -> String {
    format!(
        "{NONCE_COOKIE}={nonce}; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=300"
    )
}

/// Build a Set-Cookie header that clears the PKCE cookie.
fn clear_pkce_cookie() -> String {
    format!("{PKCE_COOKIE}=; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=0")
}

/// Build a Set-Cookie header that clears the nonce cookie.
fn clear_nonce_cookie() -> String {
    format!("{NONCE_COOKIE}=; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=0")
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

    match state.auth.get_user(&claims.sub).await {
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

    match state.auth.grant_consent(&claims.sub, &body.app_id).await {
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
        if let Ok(true) = state.auth.has_consent(&claims.sub, app_id).await {
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

// ---------------------------------------------------------------------------
// OAuth handlers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct OAuthStartQuery {
    pub app_id: Option<String>,
    #[serde(rename = "return")]
    pub return_url: Option<String>,
}

#[derive(Deserialize)]
pub struct OAuthCallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
}

/// Encode `app_id|return_url` as URL-safe base64 for the OAuth state parameter.
fn encode_state(app_id: &str, return_url: &str) -> String {
    let payload = format!("{app_id}|{return_url}");
    B64URL.encode(payload.as_bytes())
}

/// Decode the OAuth state parameter back to `(app_id, return_url)`.
fn decode_state(state: &str) -> Result<(String, String), String> {
    let bytes = B64URL.decode(state).map_err(|e| format!("invalid state: {e}"))?;
    let payload = String::from_utf8(bytes).map_err(|e| format!("invalid state utf8: {e}"))?;
    let (app_id, return_url) = payload
        .split_once('|')
        .ok_or_else(|| "invalid state format".to_string())?;
    Ok((app_id.to_string(), return_url.to_string()))
}

/// GET /auth/{provider} -- redirect user to provider's authorization page.
///
/// Query params: `app_id` (which app the user is logging into),
/// `return` (URL to redirect back to after auth completes).
///
/// Sets short-lived HttpOnly cookies for the PKCE verifier and OIDC nonce
/// so they can be recovered during the callback.
pub async fn oauth_start(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    query: Query<OAuthStartQuery>,
) -> web::HttpResponse {
    let provider_name = req.match_info().get("provider").unwrap_or("");

    let provider = match state.oauth.get(provider_name) {
        Some(p) => p,
        None => {
            return web::HttpResponse::NotFound()
                .json(&serde_json::json!({ "error": format!("unknown provider: {provider_name}") }));
        }
    };

    let app_id = query.app_id.as_deref().unwrap_or("");
    let return_url = query.return_url.as_deref().unwrap_or("/");
    let oauth_state = encode_state(app_id, return_url);

    let (url, _csrf_token, nonce, pkce_verifier) = provider.authorize_url(&oauth_state);

    // Store PKCE verifier and nonce in short-lived HttpOnly cookies (5 min TTL).
    let pkce_cookie = pkce_cookie_header(pkce_verifier.secret());
    let nonce_cookie = nonce_cookie_header(nonce.secret());

    web::HttpResponse::Found()
        .header(header::LOCATION, url)
        .header(header::SET_COOKIE, pkce_cookie)
        .header(header::SET_COOKIE, nonce_cookie)
        .finish()
}

/// GET /auth/callback/{provider} -- handle the OAuth provider's callback.
///
/// Query params: `code` (authorization code), `state` (encoded app_id + return_url).
/// Reads the PKCE verifier and nonce from cookies set during `oauth_start`.
pub async fn oauth_callback(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    query: Query<OAuthCallbackQuery>,
) -> web::HttpResponse {
    let provider_name = req.match_info().get("provider").unwrap_or("");

    let provider = match state.oauth.get(provider_name) {
        Some(p) => p,
        None => {
            return web::HttpResponse::NotFound()
                .json(&serde_json::json!({ "error": format!("unknown provider: {provider_name}") }));
        }
    };

    let code = match query.code.as_deref() {
        Some(c) if !c.is_empty() => c,
        _ => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({ "error": "missing code parameter" }));
        }
    };

    let oauth_state = match query.state.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({ "error": "missing state parameter" }));
        }
    };

    let (app_id, return_url) = match decode_state(oauth_state) {
        Ok(pair) => pair,
        Err(msg) => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({ "error": msg }));
        }
    };

    // Recover the PKCE verifier from the cookie set during oauth_start.
    let pkce_secret = match extract_cookie(&req, PKCE_COOKIE) {
        Some(s) if !s.is_empty() => s,
        _ => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({ "error": "missing PKCE verifier cookie" }));
        }
    };
    let pkce_verifier = PkceCodeVerifier::new(pkce_secret);

    // Recover the nonce from the cookie set during oauth_start.
    let nonce_secret = match extract_cookie(&req, NONCE_COOKIE) {
        Some(s) if !s.is_empty() => s,
        _ => {
            return web::HttpResponse::BadRequest()
                .json(&serde_json::json!({ "error": "missing nonce cookie" }));
        }
    };
    let nonce = Nonce::new(nonce_secret);

    // Exchange the authorization code for a user profile.
    let profile = match provider.exchange(code, pkce_verifier, &nonce).await {
        Ok(p) => p,
        Err(msg) => {
            return web::HttpResponse::BadGateway()
                .json(&serde_json::json!({ "error": msg }));
        }
    };

    // Find or create the user in our database.
    let user = match state
        .auth
        .find_or_create_oauth_user(&profile.email, &profile.name, profile.avatar_url.as_deref())
        .await
    {
        Ok(u) => u,
        Err(msg) => {
            return web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({ "error": msg }));
        }
    };

    // Issue a JWT scoped to the app.
    let token = match state.auth.issue_token(&user, &app_id) {
        Ok(t) => t,
        Err(msg) => {
            return web::HttpResponse::InternalServerError()
                .json(&serde_json::json!({ "error": msg }));
        }
    };

    // Set session cookie and clear the PKCE/nonce cookies.
    let session_cookie = set_cookie_header(&token);
    let clear_pkce = clear_pkce_cookie();
    let clear_nonce = clear_nonce_cookie();

    // Append token as a query parameter to the return URL so the app can
    // read it on the client side (useful for SPAs that don't read cookies).
    let redirect = if return_url.contains('?') {
        format!("{return_url}&token={token}")
    } else {
        format!("{return_url}?token={token}")
    };

    web::HttpResponse::Found()
        .header(header::SET_COOKIE, session_cookie)
        .header(header::SET_COOKIE, clear_pkce)
        .header(header::SET_COOKIE, clear_nonce)
        .header(header::LOCATION, redirect)
        .finish()
}
