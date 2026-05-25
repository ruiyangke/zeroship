//! AES-256-GCM encryption with two write-side modes.
//!
//! ## Modes
//!
//! - **Randomised**: 12-byte nonce sampled from `OsRng` per write.
//!   Two encrypts of the same plaintext (under the same key + AAD)
//!   produce different ciphertexts. Fail-safe default.
//! - **Deterministic**: 12-byte synthetic nonce = HMAC-SHA256(k_siv,
//!   plaintext)[..12]. Two encrypts of the same plaintext (under the
//!   same key + AAD) produce identical ciphertexts. Enables B-tree
//!   equality lookups on encrypted columns. Note: this is **not** RFC
//!   5297 AES-SIV — design §7.2 builds the deterministic mode out of
//!   `aes-gcm` + `hmac::Hmac<Sha256>` directly to avoid adding the
//!   `aes-siv` crate to the workspace.
//!
//! ## Decrypt
//!
//! [`decrypt`] is mode-agnostic: the wire format carries the nonce,
//! and AES-GCM verifies the tag regardless of how the nonce was
//! produced. The caller still has to reconstruct the right AAD —
//! see `docs/proposals/p5-encryption-backup-implementation-plan.md`
//! §13 for the (mode, AAD-shape) pairing.

use aes_gcm::aead::{Aead, OsRng, Payload};
use aes_gcm::{AeadCore, Aes256Gcm, KeyInit, Nonce};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use zeroize::{Zeroize, ZeroizeOnDrop};

use super::wire;
use crate::error::DbError;

type HmacSha256 = Hmac<Sha256>;

/// AES-GCM nonce length (RFC 5116 §5.3) — mirrors the constant in
/// [`super::wire`].
const NONCE_LEN: usize = 12;

/// Per-(app, key_id) AEAD key material. Two 32-byte halves:
///
/// - `k_enc` — the AES-256-GCM encryption key.
/// - `k_siv` — the HMAC-SHA256 key for deterministic-mode synthetic
///   nonce derivation. Distinct from `k_enc` so a leak of one half
///   doesn't compromise both modes simultaneously.
///
/// Both halves are produced by HKDF-SHA256 from the per-platform
/// root key (see [`super::keys::KeyStore`]).
#[derive(Debug, Clone, Zeroize, ZeroizeOnDrop)]
#[repr(C)]
pub struct AeadKey {
    pub k_enc: [u8; 32],
    pub k_siv: [u8; 32],
}

/// Encrypt with a per-write random nonce. Returns the packed wire
/// blob (`[version_flag | nonce | ct+tag]`).
///
/// `aad` is the canonical AAD bytes produced by
/// [`super::aad::canonical_aad`] — the caller is responsible for
/// folding the right context in for the mode (per Camp-A,
/// `(collection, column, row_pk_bytes)` for Randomised).
pub fn encrypt_randomised(
    key: &AeadKey,
    plaintext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, DbError> {
    let nonce_arr = Aes256Gcm::generate_nonce(&mut OsRng);
    // `Aes256Gcm::generate_nonce` returns a `GenericArray<u8, 12>`
    // — copy out so we can hand the same 12-byte buffer to
    // `wire::pack` and to `encrypt_with_nonce` without lifetime
    // gymnastics on the GenericArray.
    let mut nonce_bytes = [0u8; NONCE_LEN];
    nonce_bytes.copy_from_slice(nonce_arr.as_slice());
    encrypt_with_nonce(key, &nonce_bytes, plaintext, aad)
}

/// Encrypt with a synthetic nonce = HMAC-SHA256(k_siv, plaintext)[..12].
/// Same plaintext + key produces the same ciphertext, enabling
/// equality-only B-tree lookups on encrypted columns.
pub fn encrypt_deterministic(
    key: &AeadKey,
    plaintext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, DbError> {
    // `Hmac::new_from_slice` only fails for HMAC backends with
    // fixed key-length requirements — `Hmac<Sha256>` accepts any
    // length key, so this never errors in practice (mirrors the
    // pattern in `backend/sqlite/session_minter.rs`). Fully qualify
    // the call because `aes-gcm` brings `KeyInit::new_from_slice`
    // into scope through its `aead` re-exports and `HmacSha256`
    // satisfies both — the explicit `<… as Mac>::` syntax disambiguates.
    let mut mac = <HmacSha256 as Mac>::new_from_slice(&key.k_siv)
        .map_err(|_| DbError::internal("encryption::encrypt_deterministic: HMAC key length"))?;
    mac.update(plaintext);
    let tag = mac.finalize().into_bytes();
    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(&tag[..NONCE_LEN]);
    encrypt_with_nonce(key, &nonce, plaintext, aad)
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

/// Decrypt a packed wire blob. Mode-agnostic — the synthetic-vs-random
/// distinction lives on the write path; decrypt just reads the nonce
/// out of the wire and verifies the tag.
///
/// Returns `ValidationFailed { code: "encryption_aead_failed" }` on
/// tag-verification failure (tampered ciphertext, wrong AAD, wrong
/// key). Returns `Internal` on aes-gcm key-length panics, which only
/// fire if `AeadKey.k_enc` is somehow corrupted to a non-32-byte
/// state — not a user-visible failure mode.
pub fn decrypt(key: &AeadKey, blob: &[u8], aad: &[u8]) -> Result<Vec<u8>, DbError> {
    let (nonce, ct_and_tag) = wire::unpack(blob)?;
    let cipher = Aes256Gcm::new_from_slice(&key.k_enc).map_err(|_| {
        DbError::internal("encryption::decrypt: AES-256 key must be 32 bytes")
    })?;
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

    /// Synthetic test key with distinct `k_enc` / `k_siv` halves so
    /// any code path that accidentally swaps them trips test failures.
    fn test_key() -> AeadKey {
        AeadKey {
            k_enc: [0x42; 32],
            k_siv: [0x99; 32],
        }
    }

    fn test_key_alt() -> AeadKey {
        AeadKey {
            k_enc: [0x11; 32],
            k_siv: [0x22; 32],
        }
    }

    /// Randomised: same plaintext encrypts to different ciphertext
    /// (nonce is fresh per write); decrypt recovers the plaintext.
    #[test]
    fn randomised_round_trip_and_nonce_freshness() {
        let key = test_key();
        let aad = b"aad-bytes";
        let pt = b"the answer is 42";

        let ct1 = encrypt_randomised(&key, pt, aad).expect("encrypt 1");
        let ct2 = encrypt_randomised(&key, pt, aad).expect("encrypt 2");
        assert_ne!(
            ct1, ct2,
            "randomised mode must produce a fresh nonce per write"
        );

        let recovered1 = decrypt(&key, &ct1, aad).expect("decrypt 1");
        let recovered2 = decrypt(&key, &ct2, aad).expect("decrypt 2");
        assert_eq!(recovered1, pt);
        assert_eq!(recovered2, pt);
    }

    /// Deterministic: same plaintext encrypts to the same ciphertext.
    /// This is the property the B-tree-on-ciphertext index lookup
    /// depends on.
    #[test]
    fn deterministic_round_trip_and_repeatability() {
        let key = test_key();
        let aad = b"aad-bytes-det";
        let pt = b"ssn-123-45-6789";

        let ct1 = encrypt_deterministic(&key, pt, aad).expect("encrypt 1");
        let ct2 = encrypt_deterministic(&key, pt, aad).expect("encrypt 2");
        assert_eq!(
            ct1, ct2,
            "deterministic mode must produce identical ciphertext for identical plaintext"
        );

        let recovered = decrypt(&key, &ct1, aad).expect("decrypt");
        assert_eq!(recovered, pt);
    }

    /// Different plaintexts under deterministic mode produce
    /// different ciphertexts (otherwise the equality lookup would
    /// false-match across rows).
    #[test]
    fn deterministic_different_plaintexts_differ() {
        let key = test_key();
        let aad = b"aad";
        let ct_a = encrypt_deterministic(&key, b"alice", aad).expect("encrypt a");
        let ct_b = encrypt_deterministic(&key, b"bob", aad).expect("encrypt b");
        assert_ne!(ct_a, ct_b);
    }

    /// Wrong AAD on decrypt fails with `encryption_aead_failed`.
    /// This is the load-bearing defence — the Camp-A
    /// ciphertext-oracle attack reduces to "swap ciphertexts between
    /// rows", which in our scheme manifests as AAD mismatch.
    #[test]
    fn wrong_aad_fails() {
        let key = test_key();
        let blob = encrypt_randomised(&key, b"plaintext", b"correct-aad").expect("encrypt");
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
        let mut blob = encrypt_randomised(&key, b"plaintext", b"aad").expect("encrypt");
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

    /// Decrypt is mode-agnostic — a blob produced by randomised
    /// `encrypt_randomised` decrypts fine through the same `decrypt`
    /// function the deterministic path uses. Pins that there is no
    /// hidden mode-dispatching state on the read side.
    #[test]
    fn cross_mode_decrypt_is_mode_agnostic() {
        let key = test_key();
        let aad = b"aad";
        let pt = b"data";

        // Encrypt randomised, decrypt.
        let r_blob = encrypt_randomised(&key, pt, aad).expect("encrypt r");
        assert_eq!(decrypt(&key, &r_blob, aad).expect("decrypt r"), pt);

        // Encrypt deterministic, decrypt.
        let d_blob = encrypt_deterministic(&key, pt, aad).expect("encrypt d");
        assert_eq!(decrypt(&key, &d_blob, aad).expect("decrypt d"), pt);
    }

    /// Ciphertext from key A doesn't decrypt under key B. Pins the
    /// "per-app HKDF isolation" property (cross-tenant ciphertext
    /// replay is blocked at the key layer; AAD is the second line).
    #[test]
    fn different_keys_dont_decrypt() {
        let key_a = test_key();
        let key_b = test_key_alt();
        let aad = b"aad";
        let blob = encrypt_randomised(&key_a, b"secret", aad).expect("encrypt a");
        let err = decrypt(&key_b, &blob, aad).expect_err("wrong key must error");
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "encryption_aead_failed");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    /// `k_siv` mismatch under deterministic mode → different nonce
    /// → different ciphertext (separate from `k_enc` mismatch).
    /// Pins that the two key halves are kept distinct on the write
    /// path.
    #[test]
    fn deterministic_uses_k_siv_for_nonce() {
        let aad = b"aad";
        let pt = b"data";
        let k1 = AeadKey {
            k_enc: [0x42; 32],
            k_siv: [0xAA; 32],
        };
        let k2 = AeadKey {
            k_enc: [0x42; 32], // same k_enc
            k_siv: [0xBB; 32], // different k_siv
        };
        let ct1 = encrypt_deterministic(&k1, pt, aad).expect("encrypt 1");
        let ct2 = encrypt_deterministic(&k2, pt, aad).expect("encrypt 2");
        assert_ne!(ct1, ct2);
    }

    #[test]
    #[allow(unsafe_code)]
    fn aead_key_zeroizes_on_drop() {
        let mut slot = std::mem::MaybeUninit::new(AeadKey {
            k_enc: [0xAB; 32],
            k_siv: [0xCD; 32],
        });
        let ptr = slot.as_mut_ptr();
        unsafe {
            std::ptr::drop_in_place(ptr);
            let bytes = std::slice::from_raw_parts(
                ptr.cast::<u8>(),
                std::mem::size_of::<AeadKey>(),
            );
            assert!(
                bytes.iter().all(|b| *b == 0),
                "AeadKey drop must zeroize both key halves"
            );
        }
    }
}
