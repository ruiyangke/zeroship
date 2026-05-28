//! Gateway-issued JWT signing key.
//!
//! Phase 8 U1 (this commit) ships the v1 loader: read a PKCS#8 PEM or
//! DER file from disk at boot and return an Ed25519 [`SigningKey`].
//! Future phases swap the on-disk file for a KMS/HSM-backed signer; the
//! consumer side (wrapper-token issuance, JWK thumbprint as `kid`) stays
//! the same.
//!
//! Why Ed25519: smallest signatures (64 bytes), constant-time
//! verification, no curve-choice footgun, and matches the `DPoP` proof
//! verifier in `zeroship-core` so both sides of the binding speak the
//! same JOSE algorithm (`EdDSA` / `crv: Ed25519`).

use std::path::Path;

use ed25519_dalek::SigningKey;

use crate::error::{GatewayError, Result};

/// Load an Ed25519 signing key from a PKCS#8 PEM or DER file.
///
/// The file format is whatever `openssl genpkey -algorithm ed25519 -out
/// gw.key` (PEM) or `openssl genpkey -algorithm ed25519 -outform DER
/// -out gw.der` produce. The format is auto-detected: if the file
/// begins with the PEM `-----BEGIN PRIVATE KEY-----` armor we parse it
/// as PEM, otherwise we fall through to DER.
///
/// # Errors
///
/// [`GatewayError::Config`] on:
/// - I/O failure reading the file
/// - PEM parse failure
/// - DER parse failure
/// - Wrong key type (file holds an RSA/ECDSA key)
pub fn load_from_path(path: &Path) -> Result<SigningKey> {
    let bytes = std::fs::read(path).map_err(|e| {
        GatewayError::Config(format!("read signing key {}: {e}", path.display()))
    })?;

    // PEM is ASCII; DER is binary. The `-----BEGIN PRIVATE KEY-----`
    // armor is unambiguous, so a successful UTF-8 decode + that header
    // are sufficient to route to the PEM parser. Anything else (bytes
    // that don't parse as UTF-8, or text that lacks the armor) we hand
    // to the DER parser — which will return a clean error if the body
    // is garbage.
    if let Ok(s) = std::str::from_utf8(&bytes) {
        if s.contains("-----BEGIN PRIVATE KEY-----") {
            return parse_pkcs8_pem(s);
        }
    }
    parse_pkcs8_der(&bytes)
}

fn parse_pkcs8_pem(pem: &str) -> Result<SigningKey> {
    use ed25519_dalek::pkcs8::DecodePrivateKey;
    SigningKey::from_pkcs8_pem(pem)
        .map_err(|e| GatewayError::Config(format!("Ed25519 PKCS#8 PEM: {e}")))
}

fn parse_pkcs8_der(der: &[u8]) -> Result<SigningKey> {
    use ed25519_dalek::pkcs8::DecodePrivateKey;
    SigningKey::from_pkcs8_der(der)
        .map_err(|e| GatewayError::Config(format!("Ed25519 PKCS#8 DER: {e}")))
}

/// Compute the RFC 7638 JWK thumbprint of the signing key's public half.
///
/// This is the value the gateway emits as `kid` in wrapper-token
/// headers so `DPoP`-aware relying parties can fetch the matching JWK
/// from the gateway's JWKS endpoint.
///
/// The canonical JSON for an Ed25519/OKP key per RFC 8037 §2 is
/// `{"crv":"Ed25519","kty":"OKP","x":"<base64url-no-pad>"}` with members
/// in lexicographic order and no whitespace. We hand-build the string
/// because (a) the required-members-only constraint of RFC 7638 means a
/// generic `serde_json::to_string` would either include extra members
/// or require us to re-build a stripped-down value, and (b) the spec
/// requires lexicographic ordering, which `serde_json` does not
/// guarantee.
#[must_use]
pub fn jwk_thumbprint(key: &SigningKey) -> String {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    let public = key.verifying_key();
    let x = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(public.to_bytes());
    let canonical = format!(r#"{{"crv":"Ed25519","kty":"OKP","x":"{x}"}}"#);
    let digest = Sha256::digest(canonical.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwk_thumbprint_is_stable_across_calls() {
        // Computing the thumbprint twice from the same key must yield
        // the exact same string — otherwise the `kid` we publish would
        // drift across gateway restarts and break JWKS lookups.
        let key = SigningKey::from_bytes(&[42u8; 32]);
        let t1 = jwk_thumbprint(&key);
        let t2 = jwk_thumbprint(&key);
        assert_eq!(t1, t2);
        assert!(!t1.is_empty());
    }

    #[test]
    fn jwk_thumbprint_differs_across_keys() {
        // Two distinct private keys must produce distinct thumbprints,
        // otherwise the `kid` collision would let a relying party serve
        // the wrong public key during verification.
        let k1 = SigningKey::from_bytes(&[1u8; 32]);
        let k2 = SigningKey::from_bytes(&[2u8; 32]);
        assert_ne!(jwk_thumbprint(&k1), jwk_thumbprint(&k2));
    }

    #[test]
    fn load_round_trips_pkcs8_pem() {
        // Generate a key, serialise to PKCS#8 PEM via openssl-style
        // armor, write to a tmpfile, load it back through the public
        // API, and confirm the secret bytes match. Regression coverage
        // for the PEM auto-detect branch in `load_from_path`.
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let pem = key
            .to_pkcs8_pem(ed25519_dalek::pkcs8::spki::der::pem::LineEnding::LF)
            .expect("to_pkcs8_pem");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.key");
        std::fs::write(&path, pem.as_bytes()).expect("write");
        let loaded = load_from_path(&path).expect("load");
        assert_eq!(loaded.to_bytes(), key.to_bytes());
    }

    #[test]
    fn load_round_trips_pkcs8_der() {
        // Same regression coverage as the PEM case, but for the raw
        // binary DER branch — the fallback path when the file is not
        // valid UTF-8 or lacks the PEM armor.
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        let key = SigningKey::from_bytes(&[11u8; 32]);
        let der = key.to_pkcs8_der().expect("to_pkcs8_der");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.der");
        std::fs::write(&path, der.as_bytes()).expect("write");
        let loaded = load_from_path(&path).expect("load");
        assert_eq!(loaded.to_bytes(), key.to_bytes());
    }

    #[test]
    fn load_rejects_garbage() {
        // A non-PEM, non-DER file must surface as `GatewayError::Config`
        // — not a panic, not a generic I/O error. The gateway boot path
        // matches on `Config` to print an operator-friendly message.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("garbage.key");
        std::fs::write(&path, b"not a real key").expect("write");
        let err = load_from_path(&path).expect_err("should reject garbage");
        assert!(matches!(err, GatewayError::Config(_)));
    }
}
