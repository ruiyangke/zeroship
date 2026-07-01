//! Signed, short-lived "pending 2FA" stash for the login challenge (ISS-11).
//!
//! When a password login succeeds for a user with a CONFIRMED TOTP credential,
//! we must NOT complete the login yet — a second factor is required. But HTTP
//! is stateless across the two POSTs (password form, then code form), so the
//! "password was already verified for THIS user against THIS native return
//! target" fact has to ride with the browser. We carry it in a short-lived
//! HMAC-signed cookie, exactly like the OAuth federation stash
//! (`ui::oauth_stash`): the cookie is non-forgeable (MAC'd against the auth
//! `stash_signing_key`) and self-expiring, so it is NOT a bearer session — it
//! only attests "factor 1 passed", and the code form supplies factor 2.
//!
//! Binding the stash to `credential_version` means a password change / forced
//! logout (which bumps the version) invalidates an in-flight challenge.
//!
//! Wire format mirrors `OAuthStash`: `base64url(json).base64url(hmac-sha256)`.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use zeroship_core::auth::hmac_sha256;

/// 5-minute window to enter the second factor.
pub const CHALLENGE_MAX_AGE_SECS: i64 = 300;
/// Tolerated issuer clock skew for a freshly signed stash.
pub const CHALLENGE_FUTURE_SKEW_SECS: i64 = 30;

/// Production cookie name (`__Host-` prefix → Secure required).
pub const COOKIE_NAME_PROD: &str = "__Host-zsidp_2fa";
/// Dev cookie name (no prefix → no Secure requirement).
pub const COOKIE_NAME_DEV: &str = "zsidp_2fa";

/// The factor-1-passed attestation for an in-flight login challenge.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TotpChallenge {
    /// The password-verified user awaiting a second factor.
    pub user_id: uuid::Uuid,
    /// Credential version captured at password-verify time; a later change
    /// (password reset / forced logout) invalidates this challenge.
    pub credential_version: i64,
    /// Validated same-origin path to redirect to once factor 2 passes.
    pub return_to: String,
    pub iat: i64,
    pub exp: i64,
}

impl TotpChallenge {
    #[must_use]
    pub fn new(user_id: uuid::Uuid, credential_version: i64, return_to: String) -> Self {
        let iat = unix_now();
        Self {
            user_id,
            credential_version,
            return_to,
            iat,
            exp: iat.saturating_add(CHALLENGE_MAX_AGE_SECS),
        }
    }

    /// Encode + HMAC-sign: `base64url(json).base64url(hmac)`.
    ///
    /// # Panics
    ///
    /// Panics only if `serde_json` fails to serialise — impossible for this
    /// all-plain-field struct.
    #[must_use]
    pub fn encode(&self, key: &[u8]) -> String {
        let json = serde_json::to_vec(self).expect("totp challenge serialize");
        let b64 = URL_SAFE_NO_PAD.encode(&json);
        let mac = hmac_sha256(key, b64.as_bytes());
        let mac_b64 = URL_SAFE_NO_PAD.encode(mac);
        format!("{b64}.{mac_b64}")
    }

    /// Decode + verify the HMAC (constant-time) + unexpired claims.
    #[must_use]
    pub fn decode(value: &str, key: &[u8]) -> Option<Self> {
        let (b64, mac_b64) = value.split_once('.')?;
        let expected_mac = hmac_sha256(key, b64.as_bytes());
        let provided_mac = URL_SAFE_NO_PAD.decode(mac_b64).ok()?;
        if expected_mac.len() != provided_mac.len() {
            return None;
        }
        let mut diff = 0u8;
        for (a, b) in expected_mac.iter().zip(provided_mac.iter()) {
            diff |= a ^ b;
        }
        if diff != 0 {
            return None;
        }
        let json = URL_SAFE_NO_PAD.decode(b64).ok()?;
        let stash: Self = serde_json::from_slice(&json).ok()?;
        let now = unix_now();
        if stash.exp < now {
            return None;
        }
        if stash.iat > now.saturating_add(CHALLENGE_FUTURE_SKEW_SECS) {
            return None;
        }
        Some(stash)
    }
}

/// Resolve the cookie name for the current environment.
#[must_use]
pub const fn cookie_name(insecure_dev: bool) -> &'static str {
    if insecure_dev { COOKIE_NAME_DEV } else { COOKIE_NAME_PROD }
}

/// Build the `Set-Cookie` header. `HttpOnly; SameSite=Strict; Path=/`; Secure +
/// `__Host-` in prod, dropped together in dev (RFC 6265bis §4.1.3.2).
#[must_use]
pub fn set_cookie(value: &str, insecure_dev: bool) -> String {
    let name = cookie_name(insecure_dev);
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!("{name}={value}; Path=/; HttpOnly; SameSite=Strict{secure}; Max-Age={CHALLENGE_MAX_AGE_SECS}")
}

/// Clear the challenge cookie (set on the success/abort response).
#[must_use]
pub fn clear_cookie(insecure_dev: bool) -> String {
    let name = cookie_name(insecure_dev);
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!("{name}=; Path=/; HttpOnly; SameSite=Strict{secure}; Max-Age=0")
}

/// Extract the signed challenge blob from a `Cookie` header.
#[must_use]
pub fn parse_cookie(cookie_header: &str, insecure_dev: bool) -> Option<String> {
    let prefix = format!("{}=", cookie_name(insecure_dev));
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(&prefix) {
            return Some(rest.to_string());
        }
    }
    None
}

fn unix_now() -> i64 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    i64::try_from(secs).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(key: &[u8]) -> (TotpChallenge, String) {
        let c = TotpChallenge::new(uuid::Uuid::new_v4(), 7, "lc-abc".into());
        let enc = c.encode(key);
        (c, enc)
    }

    #[test]
    fn roundtrip_sign_verify() {
        let key = b"k".repeat(32);
        let (c, enc) = sample(&key);
        assert!(enc.contains('.'));
        assert_eq!(TotpChallenge::decode(&enc, &key).expect("decode"), c);
    }

    #[test]
    fn rejects_wrong_key() {
        let (_, enc) = sample(b"key-one-32-bytes-padded-padded-X");
        assert!(TotpChallenge::decode(&enc, b"key-two-32-bytes-padded-padded-X").is_none());
    }

    #[test]
    fn rejects_tampering() {
        let key = b"k".repeat(32);
        let (_, enc) = sample(&key);
        let mut tampered = enc;
        let last = tampered.pop().unwrap();
        tampered.push(if last == 'A' { 'B' } else { 'A' });
        assert!(TotpChallenge::decode(&tampered, &key).is_none());
    }

    #[test]
    fn rejects_expired() {
        let key = b"k".repeat(32);
        let now = unix_now();
        let c = TotpChallenge {
            user_id: uuid::Uuid::new_v4(),
            credential_version: 1,
            return_to: "/oauth2/authorize".into(),
            iat: now - 1000,
            exp: now - 600,
        };
        assert!(TotpChallenge::decode(&c.encode(&key), &key).is_none());
    }

    #[test]
    fn rejects_future_issued() {
        let key = b"k".repeat(32);
        let now = unix_now();
        let c = TotpChallenge {
            user_id: uuid::Uuid::new_v4(),
            credential_version: 1,
            return_to: "/oauth2/authorize".into(),
            iat: now + 120,
            exp: now + 600,
        };
        assert!(TotpChallenge::decode(&c.encode(&key), &key).is_none());
    }

    #[test]
    fn cookie_prod_has_host_prefix_secure_strict() {
        let c = set_cookie("v.sig", false);
        assert!(c.starts_with("__Host-zsidp_2fa=v.sig"));
        assert!(c.contains("; Secure"));
        assert!(c.contains("SameSite=Strict"));
        assert!(c.contains("HttpOnly"));
    }

    #[test]
    fn cookie_dev_drops_prefix_and_secure() {
        let c = set_cookie("v", true);
        assert!(!c.starts_with("__Host-"));
        assert!(c.starts_with("zsidp_2fa=v"));
        assert!(!c.contains("Secure"));
    }

    #[test]
    fn parse_cookie_env_scoped() {
        let header = "foo=bar; __Host-zsidp_2fa=a.b; baz=qux";
        assert_eq!(parse_cookie(header, false), Some("a.b".into()));
        assert_eq!(parse_cookie(header, true), None);
    }
}
