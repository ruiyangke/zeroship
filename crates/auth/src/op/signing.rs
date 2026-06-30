//! Ed25519 key-file loading and JWK helpers for the platform OP.

use std::path::Path;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::SigningKey;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::error::{AuthError, Result};

/// Load an Ed25519 signing key from a PKCS#8 PEM or DER file.
///
/// This mirrors the gateway `GATEWAY_SIGNING_KEY_FILE` loader: PEM is detected
/// by its `-----BEGIN PRIVATE KEY-----` armor; everything else is parsed as DER.
/// On Unix, any group/world permission bit is rejected.
pub fn load_ed25519_from_path(path: &Path) -> Result<SigningKey> {
    let bytes = std::fs::read(path).map_err(|e| {
        AuthError::Config(format!("read AUTH_SIGNING_KEY_FILE {}: {e}", path.display()))
    })?;
    reject_insecure_permissions(path, "AUTH_SIGNING_KEY_FILE")?;

    if let Ok(s) = std::str::from_utf8(&bytes) {
        if s.contains("-----BEGIN PRIVATE KEY-----") {
            return parse_pkcs8_pem(s);
        }
    }
    parse_pkcs8_der(&bytes)
}

/// Load the raw pairwise-salt secret bytes from a file.
///
/// The returned bytes are fed to `zeroship_core::auth::derive_pairwise_salt`.
pub fn load_pairwise_salt_secret(path: &Path) -> Result<Vec<u8>> {
    let bytes = std::fs::read(path).map_err(|e| {
        AuthError::Config(format!(
            "read AUTH_PAIRWISE_SALT_FILE {}: {e}",
            path.display()
        ))
    })?;
    reject_insecure_permissions(path, "AUTH_PAIRWISE_SALT_FILE")?;
    if bytes.is_empty() {
        return Err(AuthError::Config(format!(
            "AUTH_PAIRWISE_SALT_FILE {} is empty",
            path.display()
        )));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn reject_insecure_permissions(path: &Path, label: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let mode = path
        .metadata()
        .map_err(|e| AuthError::Config(format!("stat {label} {}: {e}", path.display())))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(AuthError::Config(format!(
            "{label} {} has insecure permissions {mode:o}; require owner-only permissions",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn reject_insecure_permissions(_path: &Path, _label: &str) -> Result<()> {
    Ok(())
}

fn parse_pkcs8_pem(pem: &str) -> Result<SigningKey> {
    use ed25519_dalek::pkcs8::DecodePrivateKey;

    SigningKey::from_pkcs8_pem(pem)
        .map_err(|e| AuthError::Config(format!("Ed25519 PKCS#8 PEM: {e}")))
}

fn parse_pkcs8_der(der: &[u8]) -> Result<SigningKey> {
    use ed25519_dalek::pkcs8::DecodePrivateKey;

    SigningKey::from_pkcs8_der(der)
        .map_err(|e| AuthError::Config(format!("Ed25519 PKCS#8 DER: {e}")))
}

/// Compute the RFC 7638 JWK thumbprint of an Ed25519 public key.
#[must_use]
pub fn jwk_thumbprint_public(public: &ed25519_dalek::VerifyingKey) -> String {
    let x = URL_SAFE_NO_PAD.encode(public.to_bytes());
    let canonical = format!(r#"{{"crv":"Ed25519","kty":"OKP","x":"{x}"}}"#);
    let digest = Sha256::digest(canonical.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// Compute the RFC 7638 JWK thumbprint of the signing key's public half.
#[must_use]
pub fn jwk_thumbprint(key: &SigningKey) -> String {
    jwk_thumbprint_public(&key.verifying_key())
}

/// Build the public OKP/Ed25519 JWK advertised in `zeroship.signing_keys`.
#[must_use]
pub fn public_jwk(public: &ed25519_dalek::VerifyingKey, kid: &str) -> serde_json::Value {
    json!({
        "kty": "OKP",
        "crv": "Ed25519",
        "use": "sig",
        "kid": kid,
        "alg": "EdDSA",
        "x": URL_SAFE_NO_PAD.encode(public.to_bytes()),
    })
}
