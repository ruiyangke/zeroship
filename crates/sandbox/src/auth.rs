//! Bearer-token auth for the sandbox HTTP API.
//!
//! The editor app / control plane holds the token (populated from
//! its env at deploy time) and sends `Authorization: Bearer <token>`
//! on every call. Constant-time comparison after a length check so
//! an attacker timing the response can't recover the token byte by
//! byte.
//!
//! ## Why startup refuses an empty token unless explicitly opted out
//!
//! Earlier versions of this module returned `true` for *every*
//! request when `SANDBOX_TOKEN` was unset, with only a stderr
//! warning. In any deployment where stderr isn't watched (k8s,
//! systemd, docker compose with the default driver) an operator
//! who forgot the env var got a fully-public RCE-as-a-service.
//!
//! [`crate::config::SandboxConfig::from_env`] now refuses to start
//! unless either `SANDBOX_TOKEN` is set OR `SANDBOX_ALLOW_NO_AUTH=1`
//! is also explicitly set — so the dev convenience still exists,
//! but the prod foot-gun no longer fires by accident.

use ntex::web::HttpRequest;
use subtle::ConstantTimeEq;

use crate::AppState;

/// Returns `true` if the request bears a valid token. The
/// "auth disabled" path is reached **only** when the operator
/// explicitly set `SANDBOX_ALLOW_NO_AUTH=1` (and consequently left
/// `SANDBOX_TOKEN` empty). `SandboxConfig::from_env` enforces that
/// opt-in at startup.
pub fn check(req: &HttpRequest, state: &AppState) -> bool {
    if state.config.token.is_empty() {
        // Reachable only via the explicit dev opt-in. We logged a
        // loud warning at startup; nothing useful to add per request.
        return true;
    }
    let header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let presented = header.strip_prefix("Bearer ").unwrap_or("").as_bytes();
    let expected = state.config.token.as_bytes();
    // `ct_eq`'s implementations short-circuit on length mismatch,
    // which leaks length via timing. Rejecting unequal-length
    // up-front is honest about the property and stops the empty-
    // header path from looking different from a wrong-length one.
    if presented.len() != expected.len() {
        return false;
    }
    presented.ct_eq(expected).into()
}
