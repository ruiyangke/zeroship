//! PKCE (RFC 7636) verifier + S256 challenge generation.
//!
//! Shared by the gateway OIDC RP module (Phase 3 U3) and the auth crate's
//! integration tests so the helpers live in one place.
//!
//! Per RFC 7636 §4.1 the `code_verifier` is 43-128 characters drawn from
//! `[A-Z][a-z][0-9]-._~`. 32 random bytes base64url-encoded yields 43
//! characters which satisfies that range and gives 256 bits of entropy.
//!
//! Per RFC 7636 §4.2 the S256 `code_challenge` is
//! `BASE64URL-ENCODE(SHA256(ASCII(code_verifier)))`.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::auth::validate_control_key;

/// Generate a 32-byte CSPRNG verifier, base64url-encoded (no padding).
#[must_use]
pub fn generate_verifier() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Derive the S256 challenge from a verifier per RFC 7636 §4.2.
#[must_use]
pub fn s256_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// Verify a PKCE S256 verifier against a stored challenge per RFC 7636 §4.6.
#[must_use]
pub fn verify_s256(verifier: &str, expected_challenge: &str) -> bool {
    is_valid_verifier(verifier)
        && is_valid_s256_challenge(expected_challenge)
        && validate_control_key(&s256_challenge(verifier), expected_challenge)
}

/// RFC 7636 §4.1 verifier syntax: 43-128 unreserved ASCII characters.
#[must_use]
pub fn is_valid_verifier(verifier: &str) -> bool {
    (43..=128).contains(&verifier.len()) && verifier.bytes().all(is_pkce_unreserved)
}

/// S256 is a SHA-256 digest encoded base64url-no-pad: exactly 43 chars.
#[must_use]
pub fn is_valid_s256_challenge(challenge: &str) -> bool {
    challenge.len() == 43 && challenge.bytes().all(is_base64url_no_pad)
}

fn is_pkce_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

fn is_base64url_no_pad(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifier_is_43_chars_base64url() {
        let v = generate_verifier();
        // 32 bytes -> ceil(32 * 4 / 3) = 43 chars (no padding).
        assert_eq!(v.len(), 43);
        // base64url alphabet only.
        assert!(v
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn verifier_is_random() {
        let a = generate_verifier();
        let b = generate_verifier();
        assert_ne!(a, b);
    }

    #[test]
    fn s256_challenge_roundtrip_matches_rfc7636_example() {
        // RFC 7636 Appendix B test vector:
        //   code_verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
        //   code_challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = s256_challenge(verifier);
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn s256_challenge_deterministic_for_generated_verifier() {
        let v = generate_verifier();
        let c1 = s256_challenge(&v);
        let c2 = s256_challenge(&v);
        assert_eq!(c1, c2);
        // 32-byte SHA256 -> 43 base64url chars (no padding).
        assert_eq!(c1.len(), 43);
    }

    #[test]
    fn verify_s256_accepts_valid_pair() {
        let verifier = generate_verifier();
        let challenge = s256_challenge(&verifier);
        assert!(verify_s256(&verifier, &challenge));
    }

    #[test]
    fn verify_s256_rejects_wrong_or_plain_values() {
        let verifier = generate_verifier();
        assert!(!verify_s256(&verifier, &s256_challenge("wrong-verifier-value-that-is-long-enough")));
        assert!(!verify_s256(&verifier, &verifier));
        assert!(!verify_s256("short", &s256_challenge("short")));
    }
}
