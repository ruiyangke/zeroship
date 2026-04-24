//! AES-256-GCM wrapper for secrets at rest.
//!
//! The stored format is a single blob: `nonce(12) || ciphertext || tag(16)`.
//! The tag is appended automatically by `aes-gcm`; we prefix the nonce
//! inline so the caller only persists one `Vec<u8>` per secret.

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use sha2::{Digest, Sha256};

const NONCE_LEN: usize = 12;

#[derive(Debug)]
pub enum CryptoError {
    TooShort,
    Decrypt,
    Encrypt,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort => write!(f, "ciphertext too short to contain a nonce"),
            Self::Decrypt => write!(f, "decryption failed (wrong key, tampered, or corrupt)"),
            Self::Encrypt => write!(f, "encryption failed"),
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

pub fn encrypt(key: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ct = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|_| CryptoError::Encrypt)?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(nonce.as_slice());
    out.extend_from_slice(&ct);
    Ok(out)
}

pub fn decrypt(key: &[u8; 32], blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if blob.len() < NONCE_LEN {
        return Err(CryptoError::TooShort);
    }
    let (nonce_bytes, ct) = blob.split_at(NONCE_LEN);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher.decrypt(nonce, ct).map_err(|_| CryptoError::Decrypt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let key = derive_key("platform-key");
        let ct = encrypt(&key, b"sk_live_hunter2").unwrap();
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
        ct[0] ^= 0x01;
        assert!(matches!(decrypt(&key, &ct), Err(CryptoError::Decrypt)));
    }

    #[test]
    fn nonce_uniqueness() {
        // Two encryptions of the same plaintext must produce different
        // ciphertexts because nonces are random per call.
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
}
