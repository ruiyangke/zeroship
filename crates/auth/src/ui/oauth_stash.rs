//! HMAC-signed stash cookie used during the `OAuth` federation dance.
//!
//! The browser is bounced through the upstream `IdP` (`accounts.google.com`,
//! `github.com/login/oauth/...`), so the random material we need to verify
//! the callback — `state`, PKCE `verifier`, OIDC `nonce`, and the pending
//! hydra `login_challenge` — has to ride with the browser. We stash it in
//! a short-lived signed cookie, MAC'd against `AuthConfig::stash_signing_key`.
//!
//! Cookie layout: `base64url(json).base64url(hmac-sha256)`. The HMAC
//! covers the base64url of the JSON (i.e. we sign the wire bytes, not the
//! original JSON), which makes decode order verifier-first / parser-second
//! the same as the gateway's `oidc_rp::Stash`.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use zeroship_core::auth::hmac_sha256;

/// Payload stashed between `/oauth/<provider>/start` and `/oauth/<provider>/callback`.
///
/// `state` and `nonce` are the OAuth/OIDC CSRF + replay tokens; `verifier`
/// is the PKCE verifier; `login_challenge` is hydra's pending login id
/// that we'll `accept_login` once the upstream dance succeeds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OAuthStash {
    pub state: String,
    pub verifier: String,
    pub nonce: String,
    pub login_challenge: String,
}

impl OAuthStash {
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
    /// (matching MAC + parseable JSON), `None` otherwise. Uses a
    /// constant-time MAC comparison to keep the signing key opaque
    /// against timing probes.
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
        serde_json::from_slice(&json).ok()
    }
}

// ─── Cookie helpers ──────────────────────────────────────────────────────

/// Google federation stash cookie.
///
/// Set on `/oauth/google/start`, cleared on `/oauth/google/callback`. 10
/// minute lifetime — enough for a slow upstream consent + login, short
/// enough that abandoned dances expire on their own.
pub const GOOGLE_STASH_COOKIE: &str = "__Host-zsidp_google_stash";

/// GitHub federation stash cookie.
///
/// Same wire format and lifetime as the Google stash; a separate cookie
/// name so concurrent dances (a user who triggered both providers via
/// different tabs) don't clobber each other.
pub const GITHUB_STASH_COOKIE: &str = "__Host-zsidp_github_stash";

/// 10-minute window for the OAuth dance to complete.
pub const STASH_MAX_AGE_SECS: i64 = 600;

/// Build a `Set-Cookie` header for a stash cookie with the given name.
/// `insecure_dev` drops `Secure` (localhost-only override).
///
/// The cookie is `HttpOnly; SameSite=Lax; Path=/`. `__Host-` prefix
/// (used by both [`GOOGLE_STASH_COOKIE`] and [`GITHUB_STASH_COOKIE`])
/// additionally forces `Secure` + `Path=/` + no `Domain` at the browser
/// level — defence in depth against subdomain-cookie attacks.
#[must_use]
pub fn set_stash_cookie(name: &str, value: &str, insecure_dev: bool) -> String {
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!(
        "{name}={value}; Path=/; HttpOnly; SameSite=Lax{secure}; Max-Age={STASH_MAX_AGE_SECS}"
    )
}

/// Clear the named stash cookie (set on the callback response so the
/// short-lived stash doesn't linger).
#[must_use]
pub fn clear_stash_cookie(name: &str, insecure_dev: bool) -> String {
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!("{name}=; Path=/; HttpOnly; SameSite=Lax{secure}; Max-Age=0")
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
        OAuthStash {
            state: "state-xyz".into(),
            verifier: "v".into(),
            nonce: "n".into(),
            login_challenge: "lc-123".into(),
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
    fn rejects_malformed_input() {
        let key = b"k".repeat(32);
        assert!(OAuthStash::decode("no-dot-separator", &key).is_none());
        assert!(OAuthStash::decode("@@@.@@@", &key).is_none());
        assert!(OAuthStash::decode("", &key).is_none());
    }

    #[test]
    fn google_set_cookie_has_secure_in_prod() {
        let c = set_stash_cookie(GOOGLE_STASH_COOKIE, "payload.signed", false);
        assert!(c.starts_with("__Host-zsidp_google_stash=payload.signed"));
        assert!(c.contains("Path=/"));
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("SameSite=Lax"));
        assert!(c.contains("Secure"));
        assert!(c.contains("Max-Age=600"));
    }

    #[test]
    fn google_set_cookie_drops_secure_in_dev() {
        let c = set_stash_cookie(GOOGLE_STASH_COOKIE, "v", true);
        assert!(!c.contains("Secure"));
    }

    #[test]
    fn google_clear_cookie_zero_max_age() {
        let c = clear_stash_cookie(GOOGLE_STASH_COOKIE, false);
        assert!(c.contains("Max-Age=0"));
        assert!(c.contains("Secure"));
        let dev = clear_stash_cookie(GOOGLE_STASH_COOKIE, true);
        assert!(!dev.contains("Secure"));
    }

    #[test]
    fn google_parse_cookie_roundtrips() {
        let header = "foo=bar; __Host-zsidp_google_stash=abc.def; baz=qux";
        assert_eq!(
            parse_stash_cookie(header, GOOGLE_STASH_COOKIE),
            Some("abc.def".into())
        );
        assert_eq!(parse_stash_cookie("nothing", GOOGLE_STASH_COOKIE), None);
    }

    #[test]
    fn github_cookie_name_is_distinct() {
        // Concurrent dances must not collide on cookie storage.
        assert_ne!(GOOGLE_STASH_COOKIE, GITHUB_STASH_COOKIE);
        let g = set_stash_cookie(GOOGLE_STASH_COOKIE, "v", false);
        let h = set_stash_cookie(GITHUB_STASH_COOKIE, "v", false);
        assert!(g.starts_with("__Host-zsidp_google_stash="));
        assert!(h.starts_with("__Host-zsidp_github_stash="));
    }

    #[test]
    fn github_parse_cookie_isolated_from_google() {
        let header = "__Host-zsidp_github_stash=gh.payload; __Host-zsidp_google_stash=go.payload";
        assert_eq!(
            parse_stash_cookie(header, GITHUB_STASH_COOKIE),
            Some("gh.payload".into())
        );
        assert_eq!(
            parse_stash_cookie(header, GOOGLE_STASH_COOKIE),
            Some("go.payload".into())
        );
    }
}
