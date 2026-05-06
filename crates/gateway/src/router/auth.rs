//! Authentication helpers used by the resource-tree dispatch path.
//!
//! `auth_satisfied` is the gate; the other two helpers (`jwt_subject_unverified`,
//! `extract_session_cookie`) are pure parsing utilities that other
//! modules also reach for (subscription affinity, per-rule rate-limit
//! bucket derivation).

use ntex::web::HttpRequest;
use uuid::Uuid;

use crate::user_auth;

/// Decide whether `req` satisfies `policy.auth`. `Anon` always passes
/// (validate() enforces `publicly_accessible: true`). `User` and `Admin`
/// require a verifiable `__zs_session` cookie. The gateway does not yet
/// distinguish admin from user roles, so for now both simply require a
/// session.
pub(super) fn auth_satisfied(
    req: &HttpRequest,
    policy: &crate::compiled::EffectivePolicy,
    auth_secret: &str,
    app_id: &Uuid,
) -> bool {
    use zeroship_bundle::AuthLevel;
    if matches!(policy.auth, AuthLevel::Anon) {
        return true;
    }
    if auth_secret.is_empty() {
        // Dev / test: when there's no auth secret configured the gateway
        // can't verify a cookie. Allow the request through; the worker
        // can still apply finer-grained checks. Matches the behavior of
        // the legacy path where `auth_secret.is_empty()` skips user
        // header injection.
        return true;
    }
    let cookie = req
        .headers()
        .get("cookie")
        .and_then(|v| v.to_str().ok());
    let app_id_str = app_id.to_string();
    user_auth::extract_user(cookie, auth_secret, &app_id_str).is_some()
}

/// Lift the `sub` claim out of a JWT *without* verification. This is
/// strictly for affinity hashing — a malicious client can pin a
/// different bucket for themselves but can't gain access to anyone
/// else's subscription state, because access control runs against
/// the verified token elsewhere (`auth_satisfied` + the worker's own
/// auth context). Returns `None` for any structural parse failure.
pub(super) fn jwt_subject_unverified(jwt: &str) -> Option<String> {
    let mut parts = jwt.split('.');
    let _header = parts.next()?;
    let payload_b64 = parts.next()?;
    use base64::Engine;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("sub").and_then(|s| s.as_str()).map(|s| s.to_string())
}

/// Pull the `__zs_session` value out of a Cookie header. Returns
/// `None` when the cookie is missing or empty so callers can fall
/// back to a different discriminator.
pub(super) fn extract_session_cookie(cookie_header: Option<&str>) -> Option<String> {
    let s = cookie_header?;
    let token = s
        .split(';')
        .map(|p| p.trim())
        .find(|p| p.starts_with("__zs_session="))?
        .strip_prefix("__zs_session=")?;
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiled::EffectivePolicy;
    use zeroship_bundle::{AuthLevel, ProcedureKind};

    #[test]
    fn auth_satisfied_passes_anon() {
        let policy = EffectivePolicy {
            auth: AuthLevel::Anon,
            rate_limit: None,
            cors: None,
            cache: None,
            csrf_origins: None,
            idempotent: false,
            idempotency_ttl_hours: None,
            max_input_bytes: None,
            middleware: vec![],
            publicly_accessible: true,
            kind: Some(ProcedureKind::Query),
            action: crate::compiled::ResolvedAction::WorkerRpc,
            timeout_ms: None,
            input_schema: None,
            output_schema: None,
        };
        let req = ntex::web::test::TestRequest::default().to_http_request();
        assert!(auth_satisfied(&req, &policy, "secret", &uuid::Uuid::nil()));
    }

    #[test]
    fn auth_satisfied_user_blocks_unauthenticated() {
        let policy = EffectivePolicy {
            auth: AuthLevel::User,
            rate_limit: None,
            cors: None,
            cache: None,
            csrf_origins: None,
            idempotent: false,
            idempotency_ttl_hours: None,
            max_input_bytes: None,
            middleware: vec![],
            publicly_accessible: false,
            kind: Some(ProcedureKind::Mutation),
            action: crate::compiled::ResolvedAction::WorkerRpc,
            timeout_ms: None,
            input_schema: None,
            output_schema: None,
        };
        // No __zs_session cookie → auth fails.
        let req = ntex::web::test::TestRequest::default().to_http_request();
        assert!(!auth_satisfied(&req, &policy, "secret", &uuid::Uuid::nil()));
    }

    #[test]
    fn auth_satisfied_falls_open_when_secret_unset() {
        // Dev / test mode: no auth_secret means the gateway can't verify
        // cookies — pass through and let the worker enforce.
        let policy = EffectivePolicy {
            auth: AuthLevel::User,
            rate_limit: None,
            cors: None,
            cache: None,
            csrf_origins: None,
            idempotent: false,
            idempotency_ttl_hours: None,
            max_input_bytes: None,
            middleware: vec![],
            publicly_accessible: false,
            kind: Some(ProcedureKind::Mutation),
            action: crate::compiled::ResolvedAction::WorkerRpc,
            timeout_ms: None,
            input_schema: None,
            output_schema: None,
        };
        let req = ntex::web::test::TestRequest::default().to_http_request();
        assert!(auth_satisfied(&req, &policy, "", &uuid::Uuid::nil()));
    }

    #[test]
    fn extract_session_cookie_handles_empty_value() {
        // `__zs_session=` (empty value) → None, so the caller falls
        // back to IP. Treating empty as a real bucket key would
        // collapse every cookie-empty client into one shared bucket.
        assert_eq!(extract_session_cookie(Some("__zs_session=")), None);
        assert_eq!(extract_session_cookie(None), None);
        assert_eq!(extract_session_cookie(Some("other=foo")), None);
    }
}
