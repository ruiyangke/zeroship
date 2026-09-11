//! Randomised authenticated encryption for stored column values.

use aes_gcm::aead::{Aead, OsRng, Payload};
use aes_gcm::{AeadCore, Aes256Gcm, KeyInit, Nonce};
use zeroize::{Zeroize, ZeroizeOnDrop};

use super::wire;
use crate::error::DbError;

/// AES-GCM nonce length (RFC 5116 §5.3) — mirrors the constant in
/// [`super::wire`].
const NONCE_LEN: usize = 12;

/// AEAD key material, erased when dropped.
#[derive(Debug, Clone, Zeroize, ZeroizeOnDrop)]
#[repr(C)]
pub struct AeadKey {
    pub k_enc: [u8; 32],
}

/// Encrypt with a fresh random nonce and authenticate the supplied context.
/// Returns the wire envelope consumed by [`decrypt`].
pub fn encrypt(key: &AeadKey, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, DbError> {
    let nonce_arr = Aes256Gcm::generate_nonce(&mut OsRng);
    // `Aes256Gcm::generate_nonce` returns a `GenericArray<u8, 12>`
    // — copy out so we can hand the same 12-byte buffer to
    // `wire::pack` and to `encrypt_with_nonce` without lifetime
    // gymnastics on the GenericArray.
    let mut nonce_bytes = [0u8; NONCE_LEN];
    nonce_bytes.copy_from_slice(nonce_arr.as_slice());
    encrypt_with_nonce(key, &nonce_bytes, plaintext, aad)
}

fn encrypt_with_nonce(
    key: &AeadKey,
    nonce: &[u8; NONCE_LEN],
    plaintext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, DbError> {
    let cipher = Aes256Gcm::new_from_slice(&key.k_enc).map_err(|_| {
        DbError::internal("encryption::encrypt_with_nonce: AES-256 key must be 32 bytes")
    })?;
    let ct_and_tag = cipher
        .encrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| DbError::internal("encryption::encrypt_with_nonce: AEAD encrypt failed"))?;
    wire::pack(nonce, &ct_and_tag)
}

/// Decrypt a packed wire blob, authenticating its ciphertext and context.
pub fn decrypt(key: &AeadKey, blob: &[u8], aad: &[u8]) -> Result<Vec<u8>, DbError> {
    let (nonce, ct_and_tag) = wire::unpack(blob)?;
    let cipher = Aes256Gcm::new_from_slice(&key.k_enc)
        .map_err(|_| DbError::internal("encryption::decrypt: AES-256 key must be 32 bytes"))?;
    cipher
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: ct_and_tag,
                aad,
            },
        )
        .map_err(|_| {
            DbError::validation(
                "encryption_aead_failed",
                "AEAD tag verification failed (tampered ciphertext, wrong AAD, or wrong key)",
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> AeadKey {
        AeadKey { k_enc: [0x42; 32] }
    }

    fn test_key_alt() -> AeadKey {
        AeadKey { k_enc: [0x11; 32] }
    }

    /// Randomised: same plaintext encrypts to different ciphertext
    /// (nonce is fresh per write); decrypt recovers the plaintext.
    #[test]
    fn randomised_round_trip_and_nonce_freshness() {
        let key = test_key();
        let aad = b"aad-bytes";
        let pt = b"the answer is 42";

        let ct1 = encrypt(&key, pt, aad).expect("encrypt 1");
        let ct2 = encrypt(&key, pt, aad).expect("encrypt 2");
        assert_ne!(
            ct1, ct2,
            "each write must produce a fresh nonce"
        );

        let recovered1 = decrypt(&key, &ct1, aad).expect("decrypt 1");
        let recovered2 = decrypt(&key, &ct2, aad).expect("decrypt 2");
        assert_eq!(recovered1, pt);
        assert_eq!(recovered2, pt);
    }

    /// Wrong AAD on decrypt fails with `encryption_aead_failed`.
    /// This is the load-bearing defence — the Camp-A
    /// ciphertext-oracle attack reduces to "swap ciphertexts between
    /// rows", which in our scheme manifests as AAD mismatch.
    #[test]
    fn wrong_aad_fails() {
        let key = test_key();
        let blob = encrypt(&key, b"plaintext", b"correct-aad").expect("encrypt");
        let err = decrypt(&key, &blob, b"wrong-aad").expect_err("wrong AAD must error");
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "encryption_aead_failed");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    /// Tampered ciphertext fails with `encryption_aead_failed` —
    /// flip one byte inside the ciphertext+tag region and expect a
    /// tag mismatch.
    #[test]
    fn tampered_ciphertext_fails() {
        let key = test_key();
        let mut blob = encrypt(&key, b"plaintext", b"aad").expect("encrypt");
        // Flip a byte in the ciphertext portion (after the
        // 1-byte version flag and 12-byte nonce).
        let tamper_at = 1 + NONCE_LEN + 2; // mid-ciphertext
        blob[tamper_at] ^= 0xFF;
        let err = decrypt(&key, &blob, b"aad").expect_err("tampered ct must error");
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "encryption_aead_failed");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    /// Ciphertext from key A doesn't decrypt under key B. Pins the
    /// "per-app HKDF isolation" property (cross-tenant ciphertext
    /// replay is blocked at the key layer; AAD is the second line).
    #[test]
    fn different_keys_dont_decrypt() {
        let key_a = test_key();
        let key_b = test_key_alt();
        let aad = b"aad";
        let blob = encrypt(&key_a, b"secret", aad).expect("encrypt a");
        let err = decrypt(&key_b, &blob, aad).expect_err("wrong key must error");
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "encryption_aead_failed");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    #[test]
    #[allow(unsafe_code)]
    fn aead_key_zeroizes_on_drop() {
        let mut slot = std::mem::MaybeUninit::new(AeadKey { k_enc: [0xAB; 32] });
        let ptr = slot.as_mut_ptr();
        unsafe {
            std::ptr::drop_in_place(ptr);
            let bytes =
                std::slice::from_raw_parts(ptr.cast::<u8>(), std::mem::size_of::<AeadKey>());
            assert!(
                bytes.iter().all(|b| *b == 0),
                "AeadKey drop must zeroize key material"
            );
        }
    }
}
