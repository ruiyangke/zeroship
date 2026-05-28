//! Double-submit CSRF token. The `IdP`'s own login/signup/consent forms
//! carry a `csrf` form field that MUST match the CSRF cookie.
//!
//! Cookie is **NOT** `HttpOnly` — the inline `<script nonce>` reads it for
//! the hidden form field (that's the "double-submit" pattern).
//!
//! Cookie name is `__Host-zsidp_csrf` in production. In dev mode
//! (`insecure_dev = true`) we drop the `__Host-` prefix and emit
//! `zsidp_csrf` — RFC 6265bis §4.1.3.2 requires `__Host-` cookies to
//! carry `Secure`, and dev runs over plain HTTP without it. Compliant
//! browsers (and curl) reject `__Host-` cookies missing `Secure`, so
//! the prefix has to come off together with `Secure` or no cookie
//! reaches the client at all.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::Rng;

/// Production cookie name (`__Host-` prefix → Secure required).
pub const COOKIE_NAME_PROD: &str = "__Host-zsidp_csrf";
/// Dev cookie name (no prefix → no Secure requirement).
pub const COOKIE_NAME_DEV: &str = "zsidp_csrf";

const TOKEN_LEN_BYTES: usize = 16;
const MAX_AGE_SECS: i64 = 3600;

/// Resolve the cookie name for the current environment.
///
/// `insecure_dev = true` → bare `zsidp_csrf` (no `__Host-` prefix,
/// emitted alongside no-Secure, no-Domain, Path=/).
/// `insecure_dev = false` → `__Host-zsidp_csrf` (paired with Secure).
#[must_use]
pub fn cookie_name(insecure_dev: bool) -> &'static str {
    if insecure_dev { COOKIE_NAME_DEV } else { COOKIE_NAME_PROD }
}

/// Generate a fresh token (128-bit, base64url-encoded, no padding).
#[must_use]
pub fn generate_token() -> String {
    let mut bytes = [0u8; TOKEN_LEN_BYTES];
    rand::thread_rng().fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Build the `Set-Cookie` header value for the CSRF token.
///
/// `insecure_dev = true` drops the `Secure` flag AND the `__Host-`
/// prefix from the cookie name (RFC 6265bis §4.1.3.2 — `__Host-`
/// requires Secure). In production both are set.
#[must_use]
pub fn set_cookie(token: &str, insecure_dev: bool) -> String {
    let name = cookie_name(insecure_dev);
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!("{name}={token}; Path=/; SameSite=Strict{secure}; Max-Age={MAX_AGE_SECS}")
}

/// Parse the CSRF token from a request's `Cookie` header value.
///
/// Looks for whichever name corresponds to the current environment
/// (`__Host-zsidp_csrf` in prod, `zsidp_csrf` in dev).
#[must_use]
pub fn parse_cookie(cookie_header: &str, insecure_dev: bool) -> Option<String> {
    let name = cookie_name(insecure_dev);
    let prefix = format!("{name}=");
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(&prefix) {
            return Some(rest.to_string());
        }
    }
    None
}

/// Constant-time comparison of the form-field token vs the cookie token.
///
/// Returns `true` only on exact byte match. Uses a length-prefix check
/// followed by an XOR accumulator so the loop runs over every byte of
/// equal-length inputs without short-circuiting on the first mismatch.
#[must_use]
pub fn matches(form_token: &str, cookie_token: &str) -> bool {
    if form_token.len() != cookie_token.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in form_token.bytes().zip(cookie_token.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_exact() {
        let t = generate_token();
        assert!(matches(&t, &t));
    }

    #[test]
    fn rejects_mismatch() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b);
        assert!(!matches(&a, &b));
    }

    #[test]
    fn rejects_length_mismatch() {
        assert!(!matches("short", "much-longer-than-short"));
    }

    #[test]
    fn set_cookie_prod_uses_host_prefix_and_secure() {
        let c = set_cookie("tok", false);
        assert!(c.starts_with("__Host-zsidp_csrf=tok"), "prod cookie: {c}");
        assert!(c.contains("; Secure"));
    }

    #[test]
    fn set_cookie_dev_drops_host_prefix_and_secure() {
        // Regression for the __Host- + insecure-dev incompatibility:
        // RFC 6265bis §4.1.3.2 requires Secure for __Host-, so when we
        // drop Secure we must also drop the prefix or the cookie is
        // silently rejected by compliant clients.
        let c = set_cookie("tok", true);
        assert!(!c.starts_with("__Host-"), "dev cookie must NOT use __Host- prefix: {c}");
        assert!(c.starts_with("zsidp_csrf=tok"), "dev cookie: {c}");
        assert!(!c.contains("Secure"), "dev cookie must NOT have Secure: {c}");
    }

    #[test]
    fn parse_cookie_dev_matches_bare_name() {
        let header = "foo=bar; zsidp_csrf=abc; baz=qux";
        assert_eq!(parse_cookie(header, true), Some("abc".to_string()));
        // Prod parser must not pick up the dev name.
        assert_eq!(parse_cookie(header, false), None);
    }

    #[test]
    fn parse_cookie_prod_matches_host_prefixed_name() {
        let header = "foo=bar; __Host-zsidp_csrf=abc; baz=qux";
        assert_eq!(parse_cookie(header, false), Some("abc".to_string()));
        assert_eq!(parse_cookie(header, true), None);
    }
}
