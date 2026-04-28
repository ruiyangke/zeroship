//! Bearer-token auth for the sandbox HTTP API.
//!
//! The editor app holds the token (populated from its env at deploy
//! time) and sends `Authorization: Bearer <token>` on every call.
//! Constant-time comparison so an attacker timing the response can't
//! recover the token char-by-char.

use ntex::web::HttpRequest;
use subtle::ConstantTimeEq;

use crate::AppState;

/// Returns `true` if the request bears a valid token (or auth is
/// disabled via empty `SANDBOX_TOKEN`).
pub fn check(req: &HttpRequest, state: &AppState) -> bool {
    if state.config.token.is_empty() {
        return true;
    }
    let header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let presented = header.strip_prefix("Bearer ").unwrap_or("");
    presented
        .as_bytes()
        .ct_eq(state.config.token.as_bytes())
        .into()
}
