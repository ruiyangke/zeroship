//! Double-submit CSRF token. The IdP's own login/signup/consent forms
//! carry a `csrf` form field that MUST match a `__Host-zsidp_csrf` cookie.
//!
//! Cookie is **NOT** `HttpOnly` — the inline `<script nonce>` reads it for
//! the hidden form field (that's the "double-submit" pattern).

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::Rng;

pub const COOKIE_NAME: &str = "__Host-zsidp_csrf";
const TOKEN_LEN_BYTES: usize = 16;
const MAX_AGE_SECS: i64 = 3600;

/// Generate a fresh token (128-bit, base64url-encoded, no padding).
#[must_use]
pub fn generate_token() -> String {
    let mut bytes = [0u8; TOKEN_LEN_BYTES];
    rand::thread_rng().fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Build the `Set-Cookie` header value for the CSRF token.
///
/// `insecure_dev = true` drops the `Secure` flag (so localhost HTTP works).
/// In production this MUST be false — `__Host-` cookies require `Secure`.
#[must_use]
pub fn set_cookie(token: &str, insecure_dev: bool) -> String {
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!("{COOKIE_NAME}={token}; Path=/; SameSite=Strict{secure}; Max-Age={MAX_AGE_SECS}")
}

/// Parse the CSRF token from a request's `Cookie` header value.
#[must_use]
pub fn parse_cookie(cookie_header: &str) -> Option<String> {
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(&format!("{COOKIE_NAME}=")) {
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
}
