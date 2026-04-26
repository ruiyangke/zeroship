//! AES-256-GCM wrapper for secrets at rest with key-rotation support.
//!
//! ## Wire format
//!
//! New ciphertexts (`encrypt`):
//!     0x01 || nonce(12) || ciphertext || tag(16)
//!     ^^^^   version byte; identifies KDF + cipher choice.
//!
//! Legacy ciphertexts (pre-rotation deployments):
//!     nonce(12) || ciphertext || tag(16)   // no version byte
//!
//! `decrypt_with_keys` tries the primary key first, then falls back to
//! provided previous keys. This lets ops rotate the master key with a
//! grace period: deploy the new primary, keep the old as `legacy_keys`,
//! re-encrypt secrets in the background, then drop the old key.
//!
//! Decoding rules:
//! - If `blob[0] == 0x01` AND `blob.len() >= 1+12+16` → versioned format.
//! - Otherwise → legacy unversioned format.
//!
//! Future versions: 0x02+ reserved for migrations to XChaCha20-Poly1305
//! / AES-GCM-SIV / different KDF.

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use sha2::{Digest, Sha256};

const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

/// Current ciphertext version. Increment when changing KDF / cipher /
/// nonce strategy in a way that's incompatible with the previous reader.
pub const CURRENT_VERSION: u8 = 0x01;

#[derive(Debug)]
pub enum CryptoError {
    TooShort,
    Decrypt,
    Encrypt,
    /// Encountered a version byte the current binary doesn't know how
    /// to decode. Either roll back, or upgrade the binary.
    UnknownVersion(u8),
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort => write!(f, "ciphertext too short to contain a nonce"),
            Self::Decrypt => write!(f, "decryption failed (wrong key, tampered, or corrupt)"),
            Self::Encrypt => write!(f, "encryption failed"),
            Self::UnknownVersion(v) => write!(f, "ciphertext has unknown version byte 0x{v:02x}"),
        }
    }
}

impl std::error::Error for CryptoError {}

/// Derive a 32-byte AES key from a master-key string. We SHA-256 the
/// string so callers can pass any length. For production, supply a
/// pre-generated high-entropy master key; for dev, any string works.
pub fn derive_key(master: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"zeroship-secret-key-v1");
    h.update(master.as_bytes());
    let out = h.finalize();
    let mut k = [0u8; 32];
    k.copy_from_slice(&out);
    k
}

/// Encrypt `plaintext`. Always emits the current versioned format.
pub fn encrypt(key: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ct = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|_| CryptoError::Encrypt)?;
    let mut out = Vec::with_capacity(1 + NONCE_LEN + ct.len());
    out.push(CURRENT_VERSION);
    out.extend_from_slice(nonce.as_slice());
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Decrypt a single-key blob. Accepts both versioned and legacy formats.
pub fn decrypt(key: &[u8; 32], blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let (nonce_bytes, ct) = parse(blob)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher.decrypt(nonce, ct).map_err(|_| CryptoError::Decrypt)
}

/// Try every key in order; return the first successful decrypt. Used
/// during key rotation: pass the new primary first, then old keys.
/// Returns `Decrypt` only if EVERY key fails.
pub fn decrypt_with_keys(keys: &[[u8; 32]], blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if keys.is_empty() {
        return Err(CryptoError::Decrypt);
    }
    // Parse once; the nonce/ct slices don't change between key attempts.
    let (nonce_bytes, ct) = parse(blob)?;
    let nonce = Nonce::from_slice(nonce_bytes);
    let mut last_err = CryptoError::Decrypt;
    for k in keys {
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(k));
        match cipher.decrypt(nonce, ct) {
            Ok(plain) => return Ok(plain),
            Err(_) => last_err = CryptoError::Decrypt,
        }
    }
    Err(last_err)
}

/// Parse a blob into `(nonce_slice, ciphertext_slice)`. Handles both
/// versioned and legacy formats.
fn parse(blob: &[u8]) -> Result<(&[u8], &[u8]), CryptoError> {
    if blob.is_empty() {
        return Err(CryptoError::TooShort);
    }
    // Versioned format starts with a known version byte AND has at
    // least 1 + NONCE_LEN + TAG_LEN bytes. Legacy format has no
    // version byte and starts directly with the nonce.
    let versioned = blob[0] == CURRENT_VERSION && blob.len() >= 1 + NONCE_LEN + TAG_LEN;
    if versioned {
        let body = &blob[1..];
        let (n, c) = body.split_at(NONCE_LEN);
        Ok((n, c))
    } else {
        // Legacy: blob must be at least nonce(12) + tag(16) = 28 bytes.
        if blob.len() < NONCE_LEN + TAG_LEN {
            return Err(CryptoError::TooShort);
        }
        // Reject blobs that look versioned with a future / unknown version.
        if blob[0] != CURRENT_VERSION
            && blob[0] != 0
            && blob.len() >= 1 + NONCE_LEN + TAG_LEN
            && blob[0] < 0x20
        {
            // Heuristic: small first byte that's not the current version
            // suggests a future versioned format we can't decode.
            return Err(CryptoError::UnknownVersion(blob[0]));
        }
        Ok(blob.split_at(NONCE_LEN))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_versioned() {
        let key = derive_key("platform-key");
        let ct = encrypt(&key, b"sk_live_hunter2").unwrap();
        assert_eq!(ct[0], CURRENT_VERSION, "first byte should be version");
        assert_eq!(decrypt(&key, &ct).unwrap(), b"sk_live_hunter2");
    }

    #[test]
    fn wrong_key_fails() {
        let k1 = derive_key("key1");
        let k2 = derive_key("key2");
        let ct = encrypt(&k1, b"secret").unwrap();
        assert!(matches!(decrypt(&k2, &ct), Err(CryptoError::Decrypt)));
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let key = derive_key("k");
        let mut ct = encrypt(&key, b"hello").unwrap();
        *ct.last_mut().unwrap() ^= 0x01;
        assert!(matches!(decrypt(&key, &ct), Err(CryptoError::Decrypt)));
    }

    #[test]
    fn tampered_nonce_fails() {
        let key = derive_key("k");
        let mut ct = encrypt(&key, b"hello").unwrap();
        // Skip version byte (index 0) — flip a nonce byte.
        ct[1] ^= 0x01;
        assert!(matches!(decrypt(&key, &ct), Err(CryptoError::Decrypt)));
    }

    #[test]
    fn tampered_version_byte_fails() {
        let key = derive_key("k");
        let mut ct = encrypt(&key, b"hello").unwrap();
        // Change to an unknown version that doesn't qualify as legacy.
        ct[0] = 0x05;
        let err = decrypt(&key, &ct).unwrap_err();
        assert!(matches!(err, CryptoError::UnknownVersion(0x05) | CryptoError::Decrypt));
    }

    #[test]
    fn nonce_uniqueness() {
        let key = derive_key("k");
        let a = encrypt(&key, b"x").unwrap();
        let b = encrypt(&key, b"x").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn empty_plaintext_roundtrip() {
        let key = derive_key("k");
        let ct = encrypt(&key, b"").unwrap();
        assert_eq!(decrypt(&key, &ct).unwrap(), b"");
    }

    #[test]
    fn too_short_blob_errors() {
        let key = derive_key("k");
        assert!(matches!(decrypt(&key, &[]), Err(CryptoError::TooShort)));
        assert!(matches!(decrypt(&key, &[0u8; 5]), Err(CryptoError::TooShort)));
    }

    #[test]
    fn derive_is_deterministic() {
        assert_eq!(derive_key("same"), derive_key("same"));
        assert_ne!(derive_key("a"), derive_key("b"));
    }

    #[test]
    fn legacy_format_still_decrypts() {
        // Synthesize a legacy blob (no version byte) by encrypting and
        // stripping the version. Legacy data in production was created
        // before key rotation shipped.
        let key = derive_key("k");
        let ct = encrypt(&key, b"old data").unwrap();
        let legacy: Vec<u8> = ct[1..].to_vec(); // drop version byte
        // Make sure first byte isn't accidentally the version + long enough.
        assert!(legacy[0] != CURRENT_VERSION);
        assert_eq!(decrypt(&key, &legacy).unwrap(), b"old data");
    }

    #[test]
    fn rotation_keys_fallback_in_order() {
        let primary = derive_key("new-key");
        let prev_a = derive_key("old-key-a");
        let prev_b = derive_key("ancient-key-b");

        // Encrypt with the OLDEST key.
        let ct = encrypt(&prev_b, b"rotated").unwrap();

        // Primary fails, prev_a fails, prev_b succeeds.
        let plain = decrypt_with_keys(&[primary, prev_a, prev_b], &ct).unwrap();
        assert_eq!(plain, b"rotated");
    }

    #[test]
    fn rotation_primary_succeeds_first() {
        let primary = derive_key("primary");
        let legacy = derive_key("legacy");
        let ct = encrypt(&primary, b"fresh").unwrap();
        assert_eq!(
            decrypt_with_keys(&[primary, legacy], &ct).unwrap(),
            b"fresh",
        );
    }

    #[test]
    fn rotation_all_keys_fail_returns_decrypt() {
        let k1 = derive_key("a");
        let k2 = derive_key("b");
        let bad1 = derive_key("c");
        let bad2 = derive_key("d");
        let ct = encrypt(&k1, b"x").unwrap();
        let _ = k2;
        let err = decrypt_with_keys(&[bad1, bad2], &ct).unwrap_err();
        assert!(matches!(err, CryptoError::Decrypt));
    }

    #[test]
    fn empty_keys_list_errors() {
        let blob = encrypt(&derive_key("k"), b"x").unwrap();
        let err = decrypt_with_keys(&[], &blob).unwrap_err();
        assert!(matches!(err, CryptoError::Decrypt));
    }
}
