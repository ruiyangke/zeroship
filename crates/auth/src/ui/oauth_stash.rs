//! HMAC-signed stash cookie used during the `OAuth` federation dance.
//!
//! The browser is bounced through the upstream `IdP` (`accounts.google.com`,
//! `github.com/login/oauth/...`), so the random material we need to verify
//! the callback — `state`, PKCE `verifier`, OIDC `nonce`, and the pending
//! native continuation target — has to ride with the browser. We stash it in a
//! short-lived signed cookie, MAC'd against `AuthConfig::stash_signing_key`.
//!
//! Cookie layout: `base64url(json).base64url(hmac-sha256)`. The HMAC
//! covers the base64url of the JSON (i.e. we sign the wire bytes, not the
//! original JSON), which makes decode order verifier-first / parser-second
//! the same as the gateway's `oidc_rp::Stash`.
//!
//! Cookie names always use the `__Host-` prefix and `Secure`. The local browser
//! topology uses `.localhost`, which browsers treat as potentially trustworthy;
//! it does not require a weaker cookie shape.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use zeroship_core::auth::hmac_sha256;

/// Payload stashed between `/oauth/<provider>/start` and `/oauth/<provider>/callback`.
///
/// `state` and `nonce` are the OAuth/OIDC CSRF + replay tokens; `verifier`
/// is the PKCE verifier. Exactly one continuation target is present:
/// the native `return_to`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OAuthStash {
    pub state: String,
    pub verifier: String,
    pub nonce: String,
    pub return_to: Option<String>,
    pub iat: i64,
    pub exp: i64,
}

impl OAuthStash {
    /// Build a fresh native stash with the standard 10-minute validity window.
    #[must_use]
    pub fn with_return_to(
        state: String,
        verifier: String,
        nonce: String,
        return_to: String,
    ) -> Self {
        Self::new(state, verifier, nonce, Some(return_to))
    }

    fn new(
        state: String,
        verifier: String,
        nonce: String,
        return_to: Option<String>,
    ) -> Self {
        debug_assert!(has_return_to(return_to.as_deref()));
        let iat = unix_now();
        Self {
            state,
            verifier,
            nonce,
            return_to,
            iat,
            exp: iat.saturating_add(STASH_MAX_AGE_SECS),
        }
    }

    /// Encode + HMAC-sign with `key`. Returns `base64url(json).base64url(hmac)`.
    ///
    /// The JSON is the canonical wire payload; we sign its base64url form
    /// so the verifier can recompute the MAC without re-serialising
    /// (avoids any field-ordering ambiguity).
    ///
    /// # Panics
    ///
    /// Panics if `serde_json` fails to serialise this struct — only
    /// possible if a non-string field is ever added that contains a
    /// non-UTF-8 byte sequence. Today every field is `String`.
    #[must_use]
    pub fn encode(&self, key: &[u8]) -> String {
        let json = serde_json::to_vec(self).expect("oauth stash serialize");
        let b64 = URL_SAFE_NO_PAD.encode(&json);
        let mac = hmac_sha256(key, b64.as_bytes());
        let mac_b64 = URL_SAFE_NO_PAD.encode(mac);
        format!("{b64}.{mac_b64}")
    }

    /// Decode + verify the HMAC, returning `Some(stash)` on a valid cookie
    /// (matching MAC + parseable JSON + unexpired timestamp claims), `None`
    /// otherwise. Uses a constant-time MAC comparison to keep the signing key
    /// opaque against timing probes.
    #[must_use]
    pub fn decode(value: &str, key: &[u8]) -> Option<Self> {
        let (b64, mac_b64) = value.split_once('.')?;
        let expected_mac = hmac_sha256(key, b64.as_bytes());
        let provided_mac = URL_SAFE_NO_PAD.decode(mac_b64).ok()?;
        if expected_mac.len() != provided_mac.len() {
            return None;
        }
        // Constant-time compare.
        let mut diff = 0u8;
        for (a, b) in expected_mac.iter().zip(provided_mac.iter()) {
            diff |= a ^ b;
        }
        if diff != 0 {
            return None;
        }
        let json = URL_SAFE_NO_PAD.decode(b64).ok()?;
        let stash: Self = serde_json::from_slice(&json).ok()?;
        if !has_return_to(stash.return_to.as_deref()) {
            return None;
        }
        let now = unix_now();
        if stash.exp < now {
            return None;
        }
        if stash.iat > now.saturating_add(STASH_FUTURE_SKEW_SECS) {
            return None;
        }
        Some(stash)
    }
}

// ─── Cookie helpers ──────────────────────────────────────────────────────

/// Google federation stash cookie (production name with `__Host-` prefix).
///
/// Set on `/oauth/google/start`, cleared on `/oauth/google/callback`. 10
/// minute lifetime — enough for a slow upstream consent + login, short
/// enough that abandoned dances expire on their own.
pub const GOOGLE_STASH_COOKIE: &str = "__Host-zsidp_google_stash";

/// GitHub federation stash cookie (production name with `__Host-` prefix).
///
/// Same wire format and lifetime as the Google stash; a separate cookie
/// name so concurrent dances (a user who triggered both providers via
/// different tabs) don't clobber each other.
pub const GITHUB_STASH_COOKIE: &str = "__Host-zsidp_github_stash";

/// 10-minute window for the OAuth dance to complete.
pub const STASH_MAX_AGE_SECS: i64 = 600;
/// Tolerated issuer clock skew for a freshly signed stash.
pub const STASH_FUTURE_SKEW_SECS: i64 = 30;

fn unix_now() -> i64 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    i64::try_from(secs).unwrap_or(i64::MAX)
}

fn has_return_to(return_to: Option<&str>) -> bool {
    return_to.is_some_and(|value| !value.is_empty())
}

/// Return the Google stash cookie name.
#[must_use]
pub const fn google_stash_cookie_name() -> &'static str {
    GOOGLE_STASH_COOKIE
}

/// Return the GitHub stash cookie name.
#[must_use]
pub const fn github_stash_cookie_name() -> &'static str {
    GITHUB_STASH_COOKIE
}

/// Build a `Set-Cookie` header for a stash cookie with the given (already
/// resolved) name.
///
/// The cookie is `HttpOnly; SameSite=Lax; Path=/`. With the `__Host-`
/// prefix the browser additionally enforces `Secure` + no `Domain=` —
/// defence in depth against subdomain-cookie attacks.
#[must_use]
pub fn set_stash_cookie(name: &str, value: &str) -> String {
    format!(
        "{name}={value}; Path=/; HttpOnly; SameSite=Lax; Secure; Max-Age={STASH_MAX_AGE_SECS}"
    )
}

/// Clear the named stash cookie (set on the callback response so the
/// short-lived stash doesn't linger).
#[must_use]
pub fn clear_stash_cookie(name: &str) -> String {
    format!("{name}=; Path=/; HttpOnly; SameSite=Lax; Secure; Max-Age=0")
}

/// Extract the named stash blob from a `Cookie` header. Returns the
/// signed string verbatim — pass to [`OAuthStash::decode`] to verify.
#[must_use]
pub fn parse_stash_cookie(cookie_header: &str, name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(&prefix) {
            return Some(rest.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_stash() -> OAuthStash {
        let iat = unix_now();
        sample_stash_at(iat, iat + 60)
    }

    fn sample_stash_at(iat: i64, exp: i64) -> OAuthStash {
        OAuthStash {
            state: "state-xyz".into(),
            verifier: "v".into(),
            nonce: "n".into(),
            return_to: Some("/oauth2/authorize?client_id=oac_123&redirect_uri=https%3A%2F%2Fapp.test%2Fcb".into()),
            iat,
            exp,
        }
    }

    #[test]
    fn roundtrip_sign_verify() {
        let key = b"k".repeat(32);
        let stash = sample_stash();
        let encoded = stash.encode(&key);
        assert!(encoded.contains('.'));
        let decoded = OAuthStash::decode(&encoded, &key).expect("decode");
        assert_eq!(decoded, stash);
    }

    #[test]
    fn rejects_wrong_key() {
        let stash = sample_stash();
        let encoded = stash.encode(b"key-one-32-bytes-padded-padded-X");
        assert!(OAuthStash::decode(&encoded, b"key-two-32-bytes-padded-padded-X").is_none());
    }

    #[test]
    fn rejects_tampering() {
        let key = b"k".repeat(32);
        let stash = sample_stash();
        // Flip the last byte to corrupt either the MAC or the payload —
        // both must be rejected.
        let mut tampered = stash.encode(&key);
        let last = tampered.pop().unwrap();
        tampered.push(if last == 'A' { 'B' } else { 'A' });
        assert!(OAuthStash::decode(&tampered, &key).is_none());
    }

    #[test]
    fn accepts_current_stash_claims() {
        let key = b"k".repeat(32);
        let now = unix_now();
        let stash = sample_stash_at(now, now + 60);
        let encoded = stash.encode(&key);
        let decoded = OAuthStash::decode(&encoded, &key).expect("decode current stash");
        assert_eq!(decoded, stash);
    }

    #[test]
    fn roundtrips_native_return_to() {
        let key = b"k".repeat(32);
        let now = unix_now();
        let stash = sample_stash_at(now, now + 60);
        let encoded = stash.encode(&key);
        let decoded = OAuthStash::decode(&encoded, &key).expect("decode native stash");
        assert_eq!(decoded, stash);
        assert_eq!(decoded.return_to, stash.return_to);
    }

    #[test]
    fn rejects_stash_without_return_to() {
        let key = b"k".repeat(32);
        let now = unix_now();
        let mut neither = sample_stash_at(now, now + 60);
        neither.return_to = None;
        assert!(OAuthStash::decode(&neither.encode(&key), &key).is_none());
    }

    #[test]
    fn rejects_expired_stash_claims() {
        let key = b"k".repeat(32);
        let now = unix_now();
        let stash = sample_stash_at(now - 1000, now - 600);
        let encoded = stash.encode(&key);
        assert!(OAuthStash::decode(&encoded, &key).is_none());
    }

    #[test]
    fn rejects_future_issued_stash_claims() {
        let key = b"k".repeat(32);
        let now = unix_now();
        let stash = sample_stash_at(now + 60, now + 600);
        let encoded = stash.encode(&key);
        assert!(OAuthStash::decode(&encoded, &key).is_none());
    }

    #[test]
    fn rejects_malformed_input() {
        let key = b"k".repeat(32);
        assert!(OAuthStash::decode("no-dot-separator", &key).is_none());
        assert!(OAuthStash::decode("@@@.@@@", &key).is_none());
        assert!(OAuthStash::decode("", &key).is_none());
    }

    #[test]
    fn google_set_cookie_has_secure() {
        let c = set_stash_cookie(google_stash_cookie_name(), "payload.signed");
        assert!(c.starts_with("__Host-zsidp_google_stash=payload.signed"));
        assert!(c.contains("Path=/"));
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("SameSite=Lax"));
        assert!(c.contains("Secure"));
        assert!(c.contains("Max-Age=600"));
    }

    #[test]
    fn google_clear_cookie_zero_max_age() {
        let c = clear_stash_cookie(google_stash_cookie_name());
        assert!(c.contains("Max-Age=0"));
        assert!(c.contains("Secure"));
    }

    #[test]
    fn google_parse_cookie_roundtrips() {
        let header = "foo=bar; __Host-zsidp_google_stash=abc.def; baz=qux";
        assert_eq!(
            parse_stash_cookie(header, google_stash_cookie_name()),
            Some("abc.def".into())
        );
        assert_eq!(parse_stash_cookie("nothing", google_stash_cookie_name()), None);
    }

    #[test]
    fn github_cookie_name_is_distinct() {
        // Concurrent dances must not collide on cookie storage.
        assert_ne!(google_stash_cookie_name(), github_stash_cookie_name());
        let g = set_stash_cookie(google_stash_cookie_name(), "v");
        let h = set_stash_cookie(github_stash_cookie_name(), "v");
        assert!(g.starts_with("__Host-zsidp_google_stash="));
        assert!(h.starts_with("__Host-zsidp_github_stash="));
    }

    #[test]
    fn github_parse_cookie_isolated_from_google() {
        let header = "__Host-zsidp_github_stash=gh.payload; __Host-zsidp_google_stash=go.payload";
        assert_eq!(
            parse_stash_cookie(header, github_stash_cookie_name()),
            Some("gh.payload".into())
        );
        assert_eq!(
            parse_stash_cookie(header, google_stash_cookie_name()),
            Some("go.payload".into())
        );
    }
}
