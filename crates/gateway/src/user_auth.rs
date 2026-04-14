//! JWT session middleware — extracts and validates the `__zs_session` cookie.
//!
//! The gateway reads the `__zs_session` cookie from every request, validates the
//! JWT signature + expiry, checks it is scoped to the current app, and (if valid)
//! encodes the user as a base64-JSON header (`X-ZS-User`) for the worker.
//!
//! The worker never sees the JWT — it only receives the decoded user object.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Types (mirrors control plane TokenClaims)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthUser {
    pub sub: i32,
    pub app: String,
    pub email: String,
    pub name: String,
    pub avatar: Option<String>,
    pub exp: usize,
    pub iat: usize,
}

// ---------------------------------------------------------------------------
// Cookie extraction + JWT validation
// ---------------------------------------------------------------------------

/// Extract and validate the `__zs_session` cookie from a request.
///
/// Returns the authenticated user if the JWT is valid and scoped to `app_id`,
/// `None` otherwise (missing cookie, bad signature, expired, wrong app).
pub fn extract_user(cookie_header: Option<&str>, jwt_secret: &str, app_id: &str) -> Option<AuthUser> {
    let cookie_str = cookie_header?;

    // Parse __zs_session from the Cookie header
    let token = cookie_str
        .split(';')
        .map(|s| s.trim())
        .find(|s| s.starts_with("__zs_session="))?
        .strip_prefix("__zs_session=")?;

    if token.is_empty() {
        return None;
    }

    // Validate JWT (HS256, checks exp automatically)
    let key = DecodingKey::from_secret(jwt_secret.as_bytes());
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = true;

    let data = decode::<AuthUser>(token, &key, &validation).ok()?;
    let claims = data.claims;

    // Ensure JWT is scoped to this app
    if claims.app != app_id {
        return None;
    }

    Some(claims)
}

// ---------------------------------------------------------------------------
// Header encoding
// ---------------------------------------------------------------------------

/// User object as seen by V8 — maps JWT claims to the public API shape.
/// `sub` becomes `id`; `app`, `exp`, `iat` are stripped.
#[derive(Serialize)]
struct UserPayload<'a> {
    id: i32,
    email: &'a str,
    name: &'a str,
    avatar: Option<&'a str>,
}

/// Serialize the authenticated user as base64-encoded JSON for the `X-ZS-User`
/// header.  The worker decodes this to populate `zeroship.auth.getUser()`.
///
/// The payload uses the public shape `{ id, email, name, avatar }` — JWT
/// internals (`sub`, `app`, `exp`, `iat`) are not forwarded to the worker.
pub fn encode_user_header(user: &AuthUser) -> String {
    let payload = UserPayload {
        id: user.sub,
        email: &user.email,
        name: &user.name,
        avatar: user.avatar.as_deref(),
    };
    let json = serde_json::to_string(&payload).unwrap_or_default();
    B64.encode(json.as_bytes())
}
