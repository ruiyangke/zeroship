//! Double-submit CSRF token. The `IdP`'s own login/signup/consent forms
//! carry a `csrf` form field that MUST match the CSRF cookie.
//!
//! Both halves are written by the SERVER in one handler: `generate_token()`
//! feeds the template's `{{ csrf }}` and [`set_cookie`] in the same response
//! (see `ui::login::render_challenge`). No client script is involved, so the
//! cookie is `HttpOnly` - double-submit needs the cookie to be sent, not to be
//! readable.
//!
//! This doc used to say the opposite: that the cookie was deliberately NOT
//! `HttpOnly` because "the inline `<script nonce>` reads it for the hidden form
//! field". No such script has ever existed here - the three templates that
//! carry an inline script (`reset`, `token_redeem_interstitial`,
//! `device_supabase`) scrub history and submit a form, and none touches
//! `document.cookie`. The claim read as a justified trade-off, so it stopped
//! anyone asking. The Vite development provider uses the same server-rendered
//! double-submit shape.
//!
//! The cookie always uses the `__Host-` prefix and `Secure`. The local browser
//! topology uses `.localhost`, which browsers treat as potentially trustworthy;
//! it does not require a weaker cookie shape.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::Rng;

/// Cookie name (`__Host-` prefix requires Secure, Path=/, and no Domain).
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
#[must_use]
pub fn set_cookie(token: &str) -> String {
    format!(
        "{COOKIE_NAME}={token}; Path=/; SameSite=Strict; Secure; HttpOnly; Max-Age={MAX_AGE_SECS}"
    )
}

/// Parse the CSRF token from a request's `Cookie` header value.
///
#[must_use]
pub fn parse_cookie(cookie_header: &str) -> Option<String> {
    let prefix = format!("{COOKIE_NAME}=");
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
    fn set_cookie_uses_host_prefix_and_secure() {
        let c = set_cookie("tok");
        assert!(c.starts_with("__Host-zsidp_csrf=tok"), "cookie: {c}");
        assert!(c.contains("; Secure"));
    }

    /// The CSRF cookie must be `HttpOnly`.
    ///
    /// Double-submit only needs the cookie to be UNREADABLE-BUT-SENT: the
    /// server compares the `Cookie` header against the form field. It needs a
    /// script-readable cookie ONLY in the variant where client JS copies the
    /// value into the field. This crate is not that variant - every form field
    /// is `{{ csrf }}`, rendered from the same `generate_token()` that produced
    /// the cookie in the same handler (see `login.rs` `render_challenge`), and
    /// every read is `parse_cookie` on the server. So `HttpOnly` costs nothing
    /// and denies an injected script the token.
    ///
    /// WHAT THIS DOES NOT CATCH: a future template that adds a script reading
    /// `document.cookie` for the token would keep this test green and be broken
    /// at runtime instead. The guard against that direction is that no such
    /// script exists to regress - grep `document.cookie` under
    /// `crates/auth/src/ui/templates/` before adding one.
    #[test]
    fn set_cookie_is_http_only() {
        let c = set_cookie("tok");
        assert!(c.contains("; HttpOnly"), "cookie: {c}");
    }

    #[test]
    fn parse_cookie_rejects_bare_name() {
        let header = "foo=bar; zsidp_csrf=abc; baz=qux";
        assert_eq!(parse_cookie(header), None);
    }

    #[test]
    fn parse_cookie_matches_host_prefixed_name() {
        let header = "foo=bar; __Host-zsidp_csrf=abc; baz=qux";
        assert_eq!(parse_cookie(header), Some("abc".to_string()));
    }
}
