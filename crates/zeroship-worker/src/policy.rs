//! Worker-side enforcement of the DECLARED route policy.
//!
//! # Why a second fence exists
//!
//! The gateway already resolves each request's `EffectivePolicy` and refuses
//! it there. This module refuses it AGAIN, in Rust, before the creator's
//! handler is entered. The next reader will call that redundant. It is not,
//! for two independent reasons:
//!
//! 1. **The gateway is not the only way in.** `WorkerUser`'s own doc states
//!    the threat: signing the `ZeroShip-User` header "prevents a caller with
//!    direct network access to the worker from forging a user identity, even
//!    if the worker's endpoint bearer-auth were ever bypassed." That sentence
//!    only means something if the worker itself rules on identity. Until this
//!    module existed the format was designed for an enforcer that was never
//!    built, and `crates/zeroship-gateway/src/router/dispatch.rs` recorded the
//!    gap as prose: "Resource-tree `user` are already short-circuited by
//!    `resolve_auth` upstream, so they never reach the worker" — a statement
//!    about the gateway's current behaviour, not an invariant the worker held.
//!
//! 2. **The worker's only other gate is creator-called.**
//!    `env.auth.requireUser()` is a convenience for READING identity; a
//!    creator who forgets to call it had no fence at all. Enforcement that
//!    depends on remembering is not enforcement.
//!
//! This does NOT weaken the gateway. Both fences run, and neither is allowed
//! to become the other's excuse: the gateway must not turn into a default-
//! allow cache on the strength of this module, and this module must not be
//! dropped on the strength of the gateway.
//!
//! # What it does not cover
//!
//! Resolution is shared with the gateway (`zeroship_bundle::compiled`), so the
//! two tiers cannot disagree about which resource a path names or what its
//! effective policy is. Everything else the gateway does — rate limits, spend
//! and account gates, CORS, CSRF, idempotency, method-vs-kind — stays the
//! gateway's alone. This fence answers exactly one question: does the DECLARED
//! policy for this request admit the principal the request actually carries?

use ntex::web::HttpResponse;
use uuid::Uuid;
use zeroship_bundle::compiled::{admit, Admission, CompiledManifest};

/// The platform's refusal of a dispatch, before any creator code ran.
///
/// Distinguishable from a creator-thrown error by construction: every arm
/// answers a `{"error": …}` envelope whose `error` value begins with
/// `platform_` and carries `"refused_by":"worker"`, and every arm logs at
/// `warn` with the target `zeroship_worker::policy`. A creator error reaches
/// the client through `make_error` in `handler.rs`, which serialises the JS
/// exception and never sets either field. An operator reading a log line or a
/// captured response body can therefore tell "the platform refused this" from
/// "the app threw" without knowing which app it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// A resource declared `auth: user` was reached with no verified identity.
    NoPrincipal,
    /// The verified identity's granted scopes do not cover the resource's.
    MissingScopes { required: Vec<String> },
    /// The request URL could not be parsed, so no resource could be resolved
    /// against a manifest that DOES declare resources. Fail closed: an
    /// enforcer that cannot determine the policy must not serve the request.
    UnreadableUrl,
}

impl Refusal {
    fn code(&self) -> &'static str {
        match self {
            Refusal::NoPrincipal => "platform_unauthenticated",
            Refusal::MissingScopes { .. } => "platform_scope_required",
            Refusal::UnreadableUrl => "platform_unresolvable_request",
        }
    }

    fn status(&self) -> u16 {
        match self {
            Refusal::NoPrincipal => 401,
            Refusal::MissingScopes { .. } => 403,
            Refusal::UnreadableUrl => 400,
        }
    }

    /// The response the gateway (or a direct caller) receives.
    pub fn response(&self) -> HttpResponse {
        let body = match self {
            Refusal::MissingScopes { required } => serde_json::json!({
                "error": self.code(),
                "refused_by": "worker",
                "required_scopes": required,
            }),
            _ => serde_json::json!({
                "error": self.code(),
                "refused_by": "worker",
            }),
        };
        match self.status() {
            401 => HttpResponse::Unauthorized().json(&body),
            403 => HttpResponse::Forbidden().json(&body),
            _ => HttpResponse::BadRequest().json(&body),
        }
    }

    /// Emit the operator-facing record. Separate from `response` so the log
    /// happens exactly once per refusal, at the call site that owns the
    /// request identifiers.
    pub fn log(&self, app_id: &Uuid, method: &str, path: &str) {
        tracing::warn!(
            target: "zeroship_worker::policy",
            app_id = %app_id,
            method = %method,
            path = %path,
            refusal = self.code(),
            "worker: platform refused a dispatch on the declared route policy \
             before creator code ran"
        );
    }
}

/// Rule the deploy's declared policy on one dispatch.
///
/// `user_json` is the payload `handler::dispatch` already HMAC-verified, and
/// `None` means no identity was presented. `url` is the worker-visible URL the
/// gateway forwarded (absolute, query included).
///
/// A path that matches NO declared resource is admitted, and that is a
/// deliberate boundary rather than an oversight: the worker is not a router
/// and must not invent a second routing verdict. Routing is the gateway's —
/// it answers `404 no resource matched` for exactly this case. What the worker
/// refuses is a request whose policy IS declared and IS not satisfied. The
/// dangerous half of "undeclared" is already closed one layer down: SEC-5 in
/// `resolve_effective_policy` compiles an `rpc:` resource whose whole
/// inheritance chain declares no `auth` to `User`, so a forgotten policy on
/// the procedure rail arrives here as a protected route, not a silent public
/// one.
pub fn enforce(
    compiled: &CompiledManifest,
    url: &str,
    user_json: Option<&str>,
) -> Result<(), Refusal> {
    let Some(path) = request_path(url) else {
        // A deploy that declares nothing has no policy to enforce and no
        // dispatchable surface either; refusing it would turn an unparseable
        // URL into a hard failure for apps this fence has no opinion about.
        if compiled.declares_no_resource() {
            return Ok(());
        }
        return Err(Refusal::UnreadableUrl);
    };
    // `lookup_resource` canonicalises before matching (SEC-2), so a dot-segment
    // or percent-encoded-dot evasion resolves to the protected resource here
    // for the same reason it does at the gateway.
    let Some(policy) = compiled.lookup_resource(&path) else {
        return Ok(());
    };
    match admit(policy, user_json) {
        Admission::Admit => Ok(()),
        Admission::NoPrincipal => Err(Refusal::NoPrincipal),
        Admission::MissingScopes { required } => Err(Refusal::MissingScopes { required }),
    }
}

/// The path component of a worker-visible URL.
///
/// The gateway builds this URL with `worker_visible_url`, so it is always
/// absolute; parsing rather than string-slicing keeps the worker reading the
/// same component the gateway matched on when a query string, a port or
/// userinfo is present.
fn request_path(url: &str) -> Option<String> {
    url::Url::parse(url).ok().map(|u| u.path().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroship_bundle::Manifest;

    fn compile(json: serde_json::Value) -> CompiledManifest {
        let manifest: Manifest = serde_json::from_value(json).expect("manifest parses");
        CompiledManifest::compile(&manifest)
    }

    fn protected_tree() -> CompiledManifest {
        compile(serde_json::json!({
            "version": 1,
            "resources": {
                "/api/private": { "auth": "user" },
                "/api/public": { "auth": "anonymous", "publicly_accessible": true },
                "rpc:billing.read": { "auth": "user", "required_scopes": ["read:billing"] },
            }
        }))
    }

    #[test]
    fn a_user_route_without_identity_is_refused() {
        let c = protected_tree();
        assert_eq!(
            enforce(&c, "https://app.test/api/private", None),
            Err(Refusal::NoPrincipal)
        );
    }

    /// The control for the arm above: same tree, same missing identity, one
    /// variable changed (the resource's declared principal).
    #[test]
    fn an_anonymous_route_without_identity_is_admitted() {
        let c = protected_tree();
        assert_eq!(enforce(&c, "https://app.test/api/public", None), Ok(()));
    }

    #[test]
    fn a_user_route_with_identity_is_admitted() {
        let c = protected_tree();
        assert_eq!(
            enforce(
                &c,
                "https://app.test/api/private",
                Some(r#"{"id":"pws_x","scopes":[]}"#)
            ),
            Ok(())
        );
    }

    /// The dev tier resolves identity from its own session cookie and hands
    /// the SAME payload shape to `call_fetch_handler_with_user`
    /// (`crates/zeroship-runtime/src/core/dev_auth.rs`). This fence reads that
    /// payload, not a signature, so a dev identity satisfies it for the same
    /// reason a gateway-signed one does — there is no dev arm to bypass.
    #[test]
    fn a_dev_tier_payload_satisfies_the_same_check() {
        let c = protected_tree();
        let dev_user = r#"{"id":"usr_dev","email":"dev@localhost","scopes":["read:billing"]}"#;
        assert_eq!(
            enforce(&c, "https://app.test/api/private", Some(dev_user)),
            Ok(())
        );
        assert_eq!(
            enforce(
                &c,
                "https://app.test/__zeroship/v1/billing.read",
                Some(dev_user)
            ),
            Ok(())
        );
    }

    #[test]
    fn a_scoped_route_refuses_a_principal_missing_the_scope() {
        let c = protected_tree();
        assert_eq!(
            enforce(
                &c,
                "https://app.test/__zeroship/v1/billing.read",
                Some(r#"{"id":"pws_x","scopes":["read:other"]}"#)
            ),
            Err(Refusal::MissingScopes {
                required: vec!["read:billing".to_string()]
            })
        );
    }

    /// SEC-2: a traversal spelling of a protected path must resolve to the
    /// protected resource, not fall through to "no resource matched".
    #[test]
    fn a_dot_segment_spelling_of_a_protected_path_is_still_refused() {
        let c = protected_tree();
        assert_eq!(
            enforce(&c, "https://app.test/api/public/../private", None),
            Err(Refusal::NoPrincipal),
            "canonicalisation happens before the match, as at the gateway"
        );
    }

    /// SEC-5: an `rpc:` resource whose chain declares no `auth` compiles to
    /// `User`, so a forgotten policy arrives here protected.
    #[test]
    fn an_rpc_resource_with_no_declared_auth_is_protected() {
        let c = compile(serde_json::json!({
            "version": 1,
            "resources": { "rpc:todos.list": { "kind": "query" } }
        }));
        assert_eq!(
            enforce(&c, "https://app.test/__zeroship/v1/todos.list", None),
            Err(Refusal::NoPrincipal)
        );
    }

    /// The gateway forwards `worker_visible_url(scheme, host, tail, query)`,
    /// where `tail` is the CANONICAL path it gated minus its leading slash.
    /// This fence therefore has to recover exactly that path back out of the
    /// URL, or the two tiers rule on different resources. A query string, a
    /// port, and a trailing slash are the three shapes that would silently
    /// shift it.
    ///
    /// Note which direction a mismatch fails in: an unrecovered path matches
    /// no resource and is ADMITTED, so a bug here costs the second fence
    /// rather than opening a hole. That is why it is pinned rather than left
    /// to be noticed.
    #[test]
    fn the_forwarded_url_shape_recovers_the_path_the_gateway_gated() {
        let c = protected_tree();
        for url in [
            "https://app.test/api/private",
            "https://app.test/api/private?next=%2Fhome&page=2",
            "http://app.test:8080/api/private",
            "https://app.test/api/private/",
        ] {
            assert_eq!(
                enforce(&c, url, None),
                Err(Refusal::NoPrincipal),
                "{url} must resolve to the same protected resource"
            );
        }
    }

    #[test]
    fn a_path_outside_the_declared_tree_is_left_to_the_gateways_404() {
        let c = protected_tree();
        assert_eq!(enforce(&c, "https://app.test/anything/else", None), Ok(()));
    }

    #[test]
    fn a_deploy_declaring_no_resource_is_never_refused() {
        let c = compile(serde_json::json!({ "version": 1 }));
        assert!(c.declares_no_resource());
        assert_eq!(enforce(&c, "https://app.test/api/private", None), Ok(()));
        assert_eq!(enforce(&c, "not a url", None), Ok(()));
    }

    #[test]
    fn an_unparseable_url_against_a_declared_tree_fails_closed() {
        let c = protected_tree();
        assert_eq!(enforce(&c, "not a url", None), Err(Refusal::UnreadableUrl));
    }

    /// The refusal envelope must be readable as the PLATFORM's, not the app's.
    #[test]
    fn every_refusal_names_the_worker_as_the_refuser() {
        for refusal in [
            Refusal::NoPrincipal,
            Refusal::MissingScopes {
                required: vec!["read:billing".to_string()],
            },
            Refusal::UnreadableUrl,
        ] {
            assert!(
                refusal.code().starts_with("platform_"),
                "{refusal:?} must be namespaced away from creator error codes"
            );
            let response = refusal.response();
            assert_eq!(response.status().as_u16(), refusal.status());
        }
    }
}
