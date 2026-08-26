//! AES-256-GCM wrapper for secrets at rest with key-rotation support.
//!
//! ## Wire format (single, unambiguous)
//!
//! ```text
//! aad_version(1) || nonce(12) || ciphertext || tag(16)
//! ```
//!
//! The version byte names the AAD contract used by the caller. Future
//! schema changes that alter associated data can therefore refuse old
//! blobs instead of accidentally authenticating them under a different
//! row context.
//!
//! ## Key rotation
//!
//! `decrypt_with_keys(&[primary, ...legacy], aad, blob)` tries the primary
//! first, then each legacy key. Lets ops rotate the master key with a
//! grace period: deploy the new primary, keep the old as `legacy`,
//! re-encrypt secrets in the background via `EnvStore::rotate_app`,
//! then drop the old key.

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

const VERSION_LEN: usize = 1;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const AAD_V1: u8 = 0x01;

#[derive(Debug)]
pub enum CryptoError {
    TooShort,
    UnsupportedVersion(u8),
    Decrypt,
    Encrypt,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort => write!(f, "ciphertext too short to contain a nonce"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported ciphertext AAD version {v}"),
            Self::Decrypt => write!(f, "decryption failed (wrong key, tampered, or corrupt)"),
            Self::Encrypt => write!(f, "encryption failed"),
        }
    }
}

impl std::error::Error for CryptoError {}

/// Derive a 32-byte AES key from a master-key string. SHA-256 over a
/// domain-separating prefix + the master string. Caller-friendly: any
/// length input. For production, supply a high-entropy master key.
///
/// **Per-app HKDF is a planned hardening (G-track follow-up).** The
/// current single-key derivation means a master compromise leaks every
/// app's secrets at once; an HKDF expansion mixing in `app_id` would
/// bound blast radius to one app.
pub fn derive_key(master: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"zeroship-secret-key-v1");
    h.update(master.as_bytes());
    let out = h.finalize();
    let mut k = [0u8; 32];
    k.copy_from_slice(&out);
    k
}

/// Encrypt `plaintext` bound to caller-provided associated data.
/// Always emits `aad_version(1) || nonce(12) || ct || tag(16)`.
pub fn encrypt(key: &[u8; 32], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ct = cipher
        .encrypt(&nonce, Payload { msg: plaintext, aad })
        .map_err(|_| CryptoError::Encrypt)?;
    let mut out = Vec::with_capacity(VERSION_LEN + NONCE_LEN + plaintext.len() + TAG_LEN);
    out.push(AAD_V1);
    out.extend_from_slice(nonce.as_slice());
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Decrypt with a single key.
pub fn decrypt(key: &[u8; 32], aad: &[u8], blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let (nonce_bytes, ct) = parse(blob)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(nonce, Payload { msg: ct, aad })
        .map_err(|_| CryptoError::Decrypt)
}

/// Try every key in order; return the first successful decrypt. Used
/// during key rotation: pass the new primary first, then old keys.
/// Always tries every key regardless of intermediate failure (constant-
/// time across key count to limit timing-side-channel info leak about
/// rotation state).
pub fn decrypt_with_keys(keys: &[[u8; 32]], aad: &[u8], blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if keys.is_empty() {
        return Err(CryptoError::Decrypt);
    }
    let (nonce_bytes, ct) = parse(blob)?;
    let nonce = Nonce::from_slice(nonce_bytes);
    let mut result: Option<Vec<u8>> = None;
    for k in keys {
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(k));
        match cipher.decrypt(nonce, Payload { msg: ct, aad }) {
            // Keep the FIRST success — but don't early-return. Continuing
            // through every key means the work is constant in `keys.len()`,
            // so an attacker measuring response time can't tell which key
            // (primary vs legacy) actually decrypted the ciphertext.
            Ok(plain) if result.is_none() => result = Some(plain),
            Ok(plain) => {
                // Got success on a non-first key — discard, scrubbing the
                // duplicate plaintext copy out of memory.
                let mut p = plain;
                p.zeroize();
            }
            Err(_) => {}
        }
    }
    result.ok_or(CryptoError::Decrypt)
}

/// Parse a blob into `(nonce_slice, ciphertext_slice)`.
/// Single unambiguous format — minimum length is
/// `VERSION_LEN + NONCE_LEN + TAG_LEN`.
fn parse(blob: &[u8]) -> Result<(&[u8], &[u8]), CryptoError> {
    if blob.len() < VERSION_LEN + NONCE_LEN + TAG_LEN {
        return Err(CryptoError::TooShort);
    }
    let version = blob[0];
    if version != AAD_V1 {
        return Err(CryptoError::UnsupportedVersion(version));
    }
    Ok(blob[VERSION_LEN..].split_at(NONCE_LEN))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let key = derive_key("platform-key");
        let aad = b"app:a:key:STRIPE_KEY";
        let ct = encrypt(&key, aad, b"sk_live_hunter2").unwrap();
        assert_eq!(decrypt(&key, aad, &ct).unwrap(), b"sk_live_hunter2");
    }

    #[test]
    fn wrong_key_fails() {
        let k1 = derive_key("key1");
        let k2 = derive_key("key2");
        let aad = b"aad";
        let ct = encrypt(&k1, aad, b"secret").unwrap();
        assert!(matches!(decrypt(&k2, aad, &ct), Err(CryptoError::Decrypt)));
    }

    #[test]
    fn wrong_aad_fails() {
        let key = derive_key("key1");
        let ct = encrypt(&key, b"app-a:TOKEN", b"secret").unwrap();
        assert!(matches!(
            decrypt(&key, b"app-b:TOKEN", &ct),
            Err(CryptoError::Decrypt)
        ));
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let key = derive_key("k");
        let aad = b"aad";
        let mut ct = encrypt(&key, aad, b"hello").unwrap();
        *ct.last_mut().unwrap() ^= 0x01;
        assert!(matches!(decrypt(&key, aad, &ct), Err(CryptoError::Decrypt)));
    }

    #[test]
    fn tampered_nonce_fails() {
        let key = derive_key("k");
        let aad = b"aad";
        let mut ct = encrypt(&key, aad, b"hello").unwrap();
        ct[VERSION_LEN] ^= 0x01;
        assert!(matches!(decrypt(&key, aad, &ct), Err(CryptoError::Decrypt)));
    }

    #[test]
    fn unsupported_version_fails() {
        let key = derive_key("k");
        let aad = b"aad";
        let mut ct = encrypt(&key, aad, b"hello").unwrap();
        ct[0] = 0x02;
        assert!(matches!(
            decrypt(&key, aad, &ct),
            Err(CryptoError::UnsupportedVersion(0x02))
        ));
    }

    #[test]
    fn nonce_uniqueness() {
        // Two encryptions of the same plaintext must produce different
        // ciphertexts because nonces are random per call.
        let key = derive_key("k");
        let aad = b"aad";
        let a = encrypt(&key, aad, b"x").unwrap();
        let b = encrypt(&key, aad, b"x").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn empty_plaintext_roundtrip() {
        let key = derive_key("k");
        let aad = b"aad";
        let ct = encrypt(&key, aad, b"").unwrap();
        assert_eq!(decrypt(&key, aad, &ct).unwrap(), b"");
    }

    #[test]
    fn too_short_blob_errors() {
        let key = derive_key("k");
        let aad = b"aad";
        assert!(matches!(decrypt(&key, aad, &[]), Err(CryptoError::TooShort)));
        assert!(matches!(decrypt(&key, aad, &[0u8; 5]), Err(CryptoError::TooShort)));
        // Exactly VERSION_LEN + NONCE_LEN bytes is below minimum (no tag).
        assert!(matches!(
            decrypt(&key, aad, &[0u8; VERSION_LEN + NONCE_LEN]),
            Err(CryptoError::TooShort)
        ));
    }

    #[test]
    fn derive_is_deterministic() {
        assert_eq!(derive_key("same"), derive_key("same"));
        assert_ne!(derive_key("a"), derive_key("b"));
    }

    #[test]
    fn rotation_keys_fallback_in_order() {
        let primary = derive_key("new-key");
        let prev_a = derive_key("old-key-a");
        let prev_b = derive_key("ancient-key-b");

        // Encrypt with the OLDEST key.
        let aad = b"aad";
        let ct = encrypt(&prev_b, aad, b"rotated").unwrap();

        // Primary fails, prev_a fails, prev_b succeeds.
        let plain = decrypt_with_keys(&[primary, prev_a, prev_b], aad, &ct).unwrap();
        assert_eq!(plain, b"rotated");
    }

    #[test]
    fn rotation_primary_succeeds_first() {
        let primary = derive_key("primary");
        let legacy = derive_key("legacy");
        let aad = b"aad";
        let ct = encrypt(&primary, aad, b"fresh").unwrap();
        assert_eq!(
            decrypt_with_keys(&[primary, legacy], aad, &ct).unwrap(),
            b"fresh",
        );
    }

    #[test]
    fn rotation_all_keys_fail_returns_decrypt() {
        let bad1 = derive_key("c");
        let bad2 = derive_key("d");
        let aad = b"aad";
        let ct = encrypt(&derive_key("a"), aad, b"x").unwrap();
        let err = decrypt_with_keys(&[bad1, bad2], aad, &ct).unwrap_err();
        assert!(matches!(err, CryptoError::Decrypt));
    }

    #[test]
    fn empty_keys_list_errors() {
        let aad = b"aad";
        let blob = encrypt(&derive_key("k"), aad, b"x").unwrap();
        let err = decrypt_with_keys(&[], aad, &blob).unwrap_err();
        assert!(matches!(err, CryptoError::Decrypt));
    }

    #[test]
    fn rotation_constant_iterations() {
        // Smoke test: with a successful primary, decrypt_with_keys
        // doesn't panic / misbehave when given many extra (failing) keys.
        let primary = derive_key("p");
        let extras: Vec<[u8; 32]> = (0..10).map(|i| derive_key(&format!("extra-{i}"))).collect();
        let aad = b"aad";
        let ct = encrypt(&primary, aad, b"value").unwrap();
        let mut keys = vec![primary];
        keys.extend(extras);
        assert_eq!(decrypt_with_keys(&keys, aad, &ct).unwrap(), b"value");
    }
}
