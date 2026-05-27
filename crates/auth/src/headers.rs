//! Security headers applied to every `crates/auth` response. Hydra
//! sets its own on `/oauth2/*` responses.
//!
//! Phase 2 exposes [`apply`] as a per-handler call site; Phase 2 Unit U6
//! installs it as ntex middleware so handlers don't have to remember.

use ntex::http::header::{HeaderName, HeaderValue};
use ntex::http::HeaderMap;

/// Apply the standard security headers to an outgoing response's header map.
///
/// Values are all static ASCII strings; this is a pure writer with no
/// failure modes. The baseline CSP blocks everything except `'self'`; the
/// login/consent/signup handlers extend it per-page with a `'nonce-...'`
/// for their inline script.
pub fn apply(headers: &mut HeaderMap) {
    static_set(
        headers,
        "strict-transport-security",
        "max-age=63072000; includeSubDomains; preload",
    );
    static_set(headers, "x-frame-options", "DENY");
    static_set(headers, "x-content-type-options", "nosniff");
    static_set(headers, "referrer-policy", "no-referrer");
    static_set(
        headers,
        "permissions-policy",
        "camera=(), microphone=(), geolocation=(), payment=(), \
         publickey-credentials-get=(self), interest-cohort=()",
    );
    static_set(headers, "cross-origin-opener-policy", "same-origin");
    static_set(headers, "cross-origin-resource-policy", "same-origin");
    static_set(headers, "cache-control", "no-store");

    // CSP — same shape as proposal §14. `'nonce-...'` and per-page hardening
    // are added by handlers that render inline scripts; the baseline blocks
    // everything else.
    static_set(
        headers,
        "content-security-policy",
        "default-src 'self'; \
         script-src 'self'; \
         style-src 'self'; \
         img-src 'self' data: https://*.zeroship.ai \
                       https://lh3.googleusercontent.com \
                       https://avatars.githubusercontent.com; \
         connect-src 'self'; \
         form-action 'self'; \
         frame-ancestors 'none'; \
         base-uri 'none'; \
         object-src 'none'; \
         upgrade-insecure-requests",
    );
}

fn static_set(headers: &mut HeaderMap, name: &'static str, value: &'static str) {
    headers.insert(
        HeaderName::from_static(name),
        HeaderValue::from_static(value),
    );
}
