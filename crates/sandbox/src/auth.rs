//! Bearer-token auth for the sandbox HTTP API.
//!
//! The editor app / control plane holds the token (populated from
//! its env at deploy time) and sends `Authorization: Bearer <token>`
//! on every call. Comparison hashes both presented and expected
//! bearers with SHA-256 and constant-time compares the fixed-size
//! digests, so token length is not exposed through a raw `ct_eq`
//! short-circuit.
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
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::AppState;

pub(crate) fn constant_time_bearer_eq(presented: &[u8], expected: &[u8]) -> bool {
    let p_digest = Sha256::digest(presented);
    let e_digest = Sha256::digest(expected);
    p_digest.ct_eq(&e_digest).into()
}

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
    constant_time_bearer_eq(presented, expected)
}

#[cfg(test)]
mod tests {
    use super::constant_time_bearer_eq;

    #[test]
    fn constant_time_bearer_eq_correctness() {
        assert!(constant_time_bearer_eq(b"right-token", b"right-token"));
        assert!(!constant_time_bearer_eq(b"right-token", b"wrong-token"));
        assert!(!constant_time_bearer_eq(b"short", b"right-token"));
        assert!(!constant_time_bearer_eq(
            b"right-token-with-extra",
            b"right-token",
        ));
        assert!(!constant_time_bearer_eq(b"", b"right-token"));
        assert!(constant_time_bearer_eq(b"", b""));
    }
}
