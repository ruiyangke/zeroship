//! Power-token wire contract — the shared types + constants for the
//! grant-gated, identity-bound, server-side capability the console (and any
//! future platform-privileged app) uses to act on the control plane.
//!
//! ## Trust model (R4 — `docs/superpowers/specs/2026-05-30-auth-bff-session-redesign.md`)
//!
//! An app's SERVER code asks for a short-lived, scope-capped, identity-bound
//! access token for `audience = CONTROL_PLANE_AUDIENCE`. Control mints it ONLY
//! after verifying:
//!
//!   1. the request carries the worker→control shared `control_key`
//!      (`internal.rs` `check_auth`) — proving the call came from the worker,
//!      not from app JS;
//!   2. the **gateway-signed `ZeroShip-User` header** (HMAC over `worker_key`)
//!      verifies — so identity is derived from a signature only the gateway can
//!      produce, NEVER from anything app JS can assert;
//!   3. the requested scopes are within the user's GRANT CEILING for this app;
//!   4. step-up freshness for elevated (deploy / secret / billing / delete)
//!      scopes;
//!   5. for `CONTROL_PLANE_AUDIENCE`, the app is **platform-privileged** — an
//!      ordinary creator app categorically cannot mint a control-audience token.
//!
//! The minted token is used SERVER-SIDE and NEVER reaches the browser. The
//! `control_key` is attached by the Rust runtime, never by app JS, so it is not
//! JS-visible.

use serde::{Deserialize, Serialize};

/// The logical audience an app's server code requests when it needs to act on
/// the control plane. This is a STABLE wire constant shared by the SDK
/// (`@zeroship/auth/server`), the runtime-mediated mint op, and the control
/// mint endpoint. Control maps this logical audience onto the concrete
/// `expected_oauth_audience` the resource-server (`AuthzGuard`) checks, so the
/// SDK never has to know the deployment's OAuth audience string.
///
/// It is deliberately NOT a hostname: a hostname would couple the SDK to a
/// particular deployment's DNS. `zeroship:control` is an opaque capability
/// label; only control knows how to satisfy it.
pub const CONTROL_PLANE_AUDIENCE: &str = "zeroship:control";

/// HTTP header carrying the dispatching app's UUID on the mint call. The Rust
/// runtime stamps it from `Runtime::app_id` (not from app JS), so control can
/// resolve the `(app, user)` grant ceiling without trusting an app-supplied
/// app id.
pub const POWER_TOKEN_APP_ID_HEADER: &str = "x-zs-app-id";

/// Default lifetime of a minted power token. Short — the token is a per-op
/// capability, not a session. The resource server re-checks scope + step-up on
/// every request, so a leaked server-side token caps blast radius tightly.
pub const POWER_TOKEN_DEFAULT_TTL_SECS: i64 = 300;

/// Maximum `auth_time` age (seconds) accepted for a step-up-gated scope. A
/// deploy / secret-rotation / billing / delete-class mint with an `auth_time`
/// older than this is rejected (`step_up_required`); the console must run a
/// fresh `max_age=0` re-auth to advance `auth_time`.
pub const STEP_UP_MAX_AGE_SECS: i64 = 300;

/// The request body app JS supplies to `getAccessToken` — ONLY `(audience,
/// scopes)`. Identity, app id, and `control_key` are attached Rust-side; app JS
/// cannot influence them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerTokenRequest {
    /// The resource server this token is FOR. Required — there is no broad,
    /// audience-less token. For v1 the only mintable audience is
    /// [`CONTROL_PLANE_AUDIENCE`].
    pub audience: String,
    /// Least-privilege scope subset. MUST be ⊆ the user's grant ceiling for
    /// this app, or the mint fails closed (`scope_required`). Empty is allowed
    /// (a zero-scope token) but useless; over-broad fails closed.
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// The successful mint response. The `access_token` is the server-side bearer;
/// it is handed to the server caller (via `fetchAs`) and never serialized
/// anywhere the browser can read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerTokenResponse {
    pub access_token: String,
    /// Unix seconds at which the token expires.
    pub expires_at: i64,
    /// The scopes actually granted on the token (the capped intersection of the
    /// request and the ceiling).
    pub scopes: Vec<String>,
}

/// Stable error codes returned (as `{ "error": <code> }`) when a mint is
/// refused. All of these are FAIL-CLOSED outcomes.
pub mod error_code {
    /// The worker→control `control_key` was missing/invalid (internal router
    /// rejection). The caller is not the worker.
    pub const UNAUTHENTICATED_CHANNEL: &str = "unauthenticated";
    /// The gateway-signed `ZeroShip-User` header was absent, malformed,
    /// expired, or its HMAC did not verify. Identity could not be trusted.
    pub const UNAUTHENTICATED_IDENTITY: &str = "unauthenticated_identity";
    /// A requested scope is not within the user's grant ceiling for this app.
    pub const SCOPE_REQUIRED: &str = "scope_required";
    /// The user's grant for this app has been revoked (or never existed).
    pub const CONSENT_REQUIRED: &str = "consent_required";
    /// An elevated (deploy/secret/billing/delete-class) scope was requested but
    /// the authenticating event is too old; a fresh re-auth is required.
    pub const STEP_UP_REQUIRED: &str = "step_up_required";
    /// The requesting app is not platform-privileged and asked for a
    /// control-audience token — the headline boundary rejection.
    pub const FORBIDDEN_AUDIENCE: &str = "forbidden_audience";
    /// The requested audience is not one this control plane can mint.
    pub const UNSUPPORTED_AUDIENCE: &str = "unsupported_audience";
}

/// The set of scope names that require step-up (fresh `auth_time`) to mint.
/// These are the irreversible / money-moving / secret-exposing capabilities.
/// Matching is by scope STRING; a scope counts as elevated if it equals one of
/// these or begins with one of the elevated prefixes below.
pub const STEP_UP_SCOPES: &[&str] = &[
    "apps:deploy",
    "deployments:rollback",
    "secrets:read",
    "secrets:write",
    "billing:write",
    "apps:delete",
];

/// Prefixes that mark a whole scope family as elevated (so e.g.
/// `secrets:rotate` is caught even if not enumerated above).
pub const STEP_UP_SCOPE_PREFIXES: &[&str] = &["secrets:", "billing:"];

/// Whether `scope` requires step-up freshness to be minted.
#[must_use]
pub fn scope_requires_step_up(scope: &str) -> bool {
    if STEP_UP_SCOPES.contains(&scope) {
        return true;
    }
    STEP_UP_SCOPE_PREFIXES
        .iter()
        .any(|prefix| scope.starts_with(prefix))
}

/// Whether ANY scope in `scopes` requires step-up.
#[must_use]
pub fn any_scope_requires_step_up(scopes: &[String]) -> bool {
    scopes.iter().any(|s| scope_requires_step_up(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_audience_is_a_stable_opaque_label() {
        // Not a hostname (would couple the SDK to deployment DNS).
        assert!(!CONTROL_PLANE_AUDIENCE.contains('.'));
        assert!(CONTROL_PLANE_AUDIENCE.starts_with("zeroship:"));
    }

    #[test]
    fn step_up_classification_matches_elevated_families() {
        assert!(scope_requires_step_up("apps:deploy"));
        assert!(scope_requires_step_up("secrets:write"));
        assert!(scope_requires_step_up("secrets:read"));
        assert!(scope_requires_step_up("billing:write"));
        assert!(scope_requires_step_up("apps:delete"));
        // Prefix-matched even if not enumerated.
        assert!(scope_requires_step_up("secrets:rotate"));
        assert!(scope_requires_step_up("billing:refund"));
        // Read-class scopes are NOT step-up.
        assert!(!scope_requires_step_up("apps:read"));
        assert!(!scope_requires_step_up("env:read"));
        assert!(!scope_requires_step_up("deployments:read"));
    }

    #[test]
    fn any_scope_step_up_is_true_iff_one_member_is() {
        assert!(!any_scope_requires_step_up(&[
            "apps:read".to_string(),
            "env:read".to_string()
        ]));
        assert!(any_scope_requires_step_up(&[
            "apps:read".to_string(),
            "apps:deploy".to_string()
        ]));
    }
}
