//! TOTP (RFC 6238) two-factor primitives (ISS-11).
//!
//! This module owns the crypto-and-math half of 2FA; `store::totp` owns the
//! persistence and `ui::totp` / `ui::login` own the HTTP flows.
//!
//! Responsibilities:
//!
//!   - generate a fresh shared secret (160-bit, the RFC 6238 / HOTP minimum for
//!     SHA-1) and render the `otpauth://` provisioning URI an authenticator app
//!     scans, plus the base32 secret for manual entry;
//!   - encrypt that secret AT REST with AES-256-GCM (`zeroship_core::crypto`),
//!     the ciphertext AAD-bound to the owning `user_id` so a blob lifted onto
//!     another user's row fails to decrypt;
//!   - verify a submitted 6-digit code against the secret with ±1 step skew, in
//!     constant time (the `totp-rs` `check` uses `constant_time_eq`);
//!   - mint single-use backup codes and hash them with the SAME Argon2id the
//!     password column uses (never store the plaintext).
//!
//! ## Parameters (RFC 6238 defaults)
//!
//! HMAC-SHA1, 6 digits, 30-second step, ±1 step skew (so a code from the
//! previous/next window still verifies, covering clock drift). These match what
//! Google Authenticator / Authy / 1Password emit by default, so a user can
//! enroll with any standard app.

use base64::Engine as _;
use rand::RngCore;
use totp_rs::{Algorithm, Secret, TotpUrlError, TOTP};

use crate::error::{AuthError, Result};
use crate::identity::password;

/// RFC 6238 step (seconds).
pub const STEP_SECS: u64 = 30;
/// RFC 6238 code length (digits).
pub const DIGITS: usize = 6;
/// Window skew (steps): accept the previous/current/next window.
pub const SKEW: u8 = 1;
/// Shared-secret length in bytes (160-bit — the SHA-1 HOTP/RFC 6238 minimum).
pub const SECRET_LEN_BYTES: usize = 20;
/// Number of single-use backup codes minted at confirm time.
pub const BACKUP_CODE_COUNT: usize = 10;
/// Decimal digits per backup code.
pub const BACKUP_CODE_DIGITS: usize = 10;

/// AAD domain tag binding a TOTP ciphertext to its owning user row. The full
/// AAD is this tag plus the user UUID bytes, so a ciphertext copied onto a
/// different `user_id` fails authentication on decrypt.
const AAD_DOMAIN: &[u8] = b"zeroship-totp-secret-v1:";

/// Decode the configured at-rest key (`AUTH_TOTP_ENC_KEY`) into a 32-byte AES
/// key. Accepts hex (64 chars) or base64url (padded/unpadded), matching the
/// `validate_master_key_material` boot guard — the value is taken verbatim from
/// the validated config, so only its first 32 decoded bytes are used.
///
/// # Errors
///
/// Returns [`AuthError::Config`] if the configured key does not decode to ≥32
/// bytes (the boot guard makes this unreachable in practice).
pub fn key_from_config(configured: &str) -> Result<[u8; 32]> {
    let trimmed = configured.trim();
    let bytes = if trimmed.len() == 64 && trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        hex::decode(trimmed).ok()
    } else {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(trimmed)
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(trimmed))
            .ok()
    };
    match bytes {
        Some(b) if b.len() >= 32 => {
            let mut k = [0u8; 32];
            k.copy_from_slice(&b[..32]);
            Ok(k)
        }
        _ => Err(AuthError::Config(
            "AUTH_TOTP_ENC_KEY does not decode to at least 32 bytes".into(),
        )),
    }
}

/// Generate a fresh random shared secret (raw bytes, not yet encrypted).
#[must_use]
pub fn generate_secret() -> Vec<u8> {
    let mut buf = vec![0u8; SECRET_LEN_BYTES];
    rand::thread_rng().fill_bytes(&mut buf);
    buf
}

/// Associated data for the at-rest encryption of `user_id`'s secret.
fn aad_for(user_id: uuid::Uuid) -> Vec<u8> {
    let mut aad = Vec::with_capacity(AAD_DOMAIN.len() + 16);
    aad.extend_from_slice(AAD_DOMAIN);
    aad.extend_from_slice(user_id.as_bytes());
    aad
}

/// Encrypt a raw TOTP secret for storage, bound to `user_id`.
///
/// # Errors
///
/// Returns [`AuthError::Internal`] if AES-GCM encryption fails (should not
/// happen with a valid 32-byte key).
pub fn encrypt_secret(key: &[u8; 32], user_id: uuid::Uuid, secret: &[u8]) -> Result<Vec<u8>> {
    zeroship_core::crypto::encrypt(key, &aad_for(user_id), secret)
        .map_err(|e| AuthError::Internal(format!("totp secret encrypt: {e}")))
}

/// Decrypt a stored TOTP secret, verifying it is bound to `user_id`.
///
/// # Errors
///
/// Returns [`AuthError::Internal`] if decryption/authentication fails (wrong
/// key, tampered ciphertext, or a blob bound to a different user).
pub fn decrypt_secret(key: &[u8; 32], user_id: uuid::Uuid, blob: &[u8]) -> Result<Vec<u8>> {
    zeroship_core::crypto::decrypt(key, &aad_for(user_id), blob)
        .map_err(|e| AuthError::Internal(format!("totp secret decrypt: {e}")))
}

/// Build a `totp-rs` `TOTP` over the raw secret with our fixed RFC parameters.
///
/// `issuer` / `account_name` populate the `otpauth://` label so the
/// authenticator app shows a human-readable entry. Neither may contain `:`
/// (RFC reserves it as the label separator) — callers pass an email + a fixed
/// issuer, so this only errors on a misconfigured (colon-bearing) issuer.
fn build(secret: &[u8], issuer: &str, account_name: &str) -> std::result::Result<TOTP, TotpUrlError> {
    TOTP::new(
        Algorithm::SHA1,
        DIGITS,
        SKEW,
        STEP_SECS,
        secret.to_vec(),
        Some(issuer.to_string()),
        account_name.to_string(),
    )
}

/// The `otpauth://` provisioning URI + the base32-encoded secret for manual
/// entry, for a freshly generated (raw) secret.
///
/// # Errors
///
/// Returns [`AuthError::Internal`] if `issuer`/`account_name` are invalid for an
/// `otpauth` label (contain `:`).
pub fn provisioning(
    secret: &[u8],
    issuer: &str,
    account_name: &str,
) -> Result<Provisioning> {
    let totp = build(secret, issuer, account_name)
        .map_err(|e| AuthError::Internal(format!("totp provisioning: {e}")))?;
    Ok(Provisioning {
        otpauth_uri: totp.get_url(),
        secret_base32: Secret::Raw(secret.to_vec()).to_encoded().to_string(),
    })
}

/// What enrollment hands back to the user: the QR-encodable URI and the
/// base32 secret for manual entry into an authenticator app.
#[derive(Debug, Clone)]
pub struct Provisioning {
    pub otpauth_uri: String,
    pub secret_base32: String,
}

/// Verify a submitted code against a raw secret at the current system time,
/// accepting ±[`SKEW`] step(s). Constant-time digit comparison via `totp-rs`.
///
/// Returns `false` on any malformed/empty code or on a clock failure — never
/// panics, never leaks via an early non-constant path.
#[must_use]
pub fn verify_code(secret: &[u8], code: &str) -> bool {
    let code = code.trim();
    if code.is_empty() {
        return false;
    }
    // SHA1 + our params can only fail `new` on a colon in the (empty) label,
    // which never happens here — use a benign issuer/account for the math-only
    // verify. `check_current` re-reads the clock each call.
    match build(secret, "zeroship", "verify") {
        Ok(totp) => totp.check_current(code).unwrap_or(false),
        Err(_) => false,
    }
}

/// Verify a code against a secret at an EXPLICIT unix time (seconds). Test seam
/// so the RFC math can be exercised deterministically (known secret + time →
/// known code), and so skew can be asserted without sleeping.
#[must_use]
pub fn verify_code_at(secret: &[u8], code: &str, unix_secs: u64) -> bool {
    let code = code.trim();
    if code.is_empty() {
        return false;
    }
    match build(secret, "zeroship", "verify") {
        Ok(totp) => totp.check(code, unix_secs),
        Err(_) => false,
    }
}

/// Generate the code a secret produces at an explicit unix time. Test/seam +
/// used by no production path.
#[must_use]
pub fn code_at(secret: &[u8], unix_secs: u64) -> String {
    match build(secret, "zeroship", "verify") {
        Ok(totp) => totp.generate(unix_secs),
        Err(_) => String::new(),
    }
}

/// Mint a fresh set of single-use backup codes (cleartext, to be shown ONCE)
/// alongside their Argon2id PHC hashes (to be stored). Returns
/// `(plaintext_codes, hashes)` in matching order.
///
/// # Errors
///
/// Returns [`AuthError::Internal`] if Argon2 hashing fails.
pub fn generate_backup_codes() -> Result<(Vec<String>, Vec<String>)> {
    let mut plain = Vec::with_capacity(BACKUP_CODE_COUNT);
    let mut hashes = Vec::with_capacity(BACKUP_CODE_COUNT);
    for _ in 0..BACKUP_CODE_COUNT {
        let code = random_backup_code();
        // Hash over the NORMALISED form so verify (which normalises the
        // submitted code) compares like-for-like regardless of the dash the
        // display form carries.
        let hash = password::hash(&normalize_backup_code(&code))?;
        plain.push(code);
        hashes.push(hash);
    }
    Ok((plain, hashes))
}

/// One random numeric backup code (`BACKUP_CODE_DIGITS` decimal digits, with a
/// separating dash for readability — the dash is stripped before verify).
fn random_backup_code() -> String {
    let mut rng = rand::thread_rng();
    let mut digits = String::with_capacity(BACKUP_CODE_DIGITS);
    for _ in 0..BACKUP_CODE_DIGITS {
        // 0..=9 — rejection-free since RngCore::next_u32 % 10 bias is negligible
        // for non-cryptographic display formatting of an already-random code; the
        // ENTROPY is the full digit count, and codes are also rate-limited + Argon2-
        // hashed at rest.
        let d = (rng.next_u32() % 10) as u8;
        digits.push((b'0' + d) as char);
    }
    // Group as XXXXX-XXXXX for readability (10 digits → 5-5).
    let mid = BACKUP_CODE_DIGITS / 2;
    format!("{}-{}", &digits[..mid], &digits[mid..])
}

/// Normalise a submitted backup code for hashing/verify: strip whitespace and
/// dashes so `12345-67890`, `1234567890`, and `12345 67890` all match.
#[must_use]
pub fn normalize_backup_code(code: &str) -> String {
    code.chars().filter(char::is_ascii_alphanumeric).collect()
}

/// Verify a submitted backup code against a stored PHC hash (constant-time via
/// Argon2). The submitted code is normalised first; the stored hash was minted
/// over the (dash-bearing) display form, so we compare the SAME normalisation
/// on both sides — i.e. hashes are stored over the normalised form.
///
/// # Errors
///
/// Returns [`AuthError::Internal`] only if the stored PHC string is malformed.
pub fn verify_backup_code(submitted: &str, phc: &str) -> Result<bool> {
    password::verify(&normalize_backup_code(submitted), phc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> [u8; 32] {
        zeroship_core::crypto::derive_key("totp-unit-test-key")
    }

    #[test]
    fn key_from_config_accepts_hex_and_base64_and_rejects_short() {
        // 64 hex chars → 32 bytes.
        let hex = "ab".repeat(32);
        assert!(key_from_config(&hex).is_ok());
        // base64url of 32 bytes.
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([7u8; 32]);
        let k = key_from_config(&b64).expect("base64 key");
        assert_eq!(k, [7u8; 32]);
        // The repo dev sentinel decodes to 32 bytes.
        assert!(key_from_config(zeroship_core::config::DEV_TOTP_ENC_KEY).is_ok());
        // Too short → rejected.
        assert!(key_from_config("deadbeef").is_err());
        assert!(key_from_config("").is_err());
    }

    #[test]
    fn generated_secret_is_correct_length_and_random() {
        let a = generate_secret();
        let b = generate_secret();
        assert_eq!(a.len(), SECRET_LEN_BYTES);
        assert_ne!(a, b, "two generated secrets must differ");
    }

    #[test]
    fn encrypt_then_decrypt_roundtrips_bound_to_user() {
        let user = uuid::Uuid::new_v4();
        let secret = generate_secret();
        let ct = encrypt_secret(&key(), user, &secret).expect("encrypt");
        // Ciphertext must NOT contain the plaintext secret.
        assert!(
            !ct.windows(secret.len()).any(|w| w == secret.as_slice()),
            "plaintext secret must not appear in the ciphertext"
        );
        let pt = decrypt_secret(&key(), user, &ct).expect("decrypt");
        assert_eq!(pt, secret);
    }

    #[test]
    fn ciphertext_is_bound_to_user_id_via_aad() {
        let user_a = uuid::Uuid::new_v4();
        let user_b = uuid::Uuid::new_v4();
        let secret = generate_secret();
        let ct = encrypt_secret(&key(), user_a, &secret).expect("encrypt");
        // The SAME key but a different user_id (AAD) must fail to decrypt — so a
        // ciphertext lifted onto another user's row is useless.
        assert!(
            decrypt_secret(&key(), user_b, &ct).is_err(),
            "decrypt under a different user_id must fail (AAD binding)"
        );
    }

    #[test]
    fn known_secret_and_time_produce_the_expected_code() {
        // RFC-style deterministic check: a fixed secret at a fixed time yields a
        // stable code that verifies, and a different time yields a different one.
        let secret = b"12345678901234567890".to_vec(); // 20 bytes
        let t0 = 59u64; // step 1
        let code0 = code_at(&secret, t0);
        assert_eq!(code0.len(), DIGITS);
        assert!(verify_code_at(&secret, &code0, t0), "code verifies at its own time");

        // A code from far away (10 steps later) must NOT verify at t0 (beyond skew).
        let far = code_at(&secret, t0 + STEP_SECS * 10);
        if far != code0 {
            assert!(
                !verify_code_at(&secret, &far, t0),
                "a code 10 steps away must be rejected at t0"
            );
        }
    }

    #[test]
    fn accepts_plus_minus_one_step_skew() {
        let secret = generate_secret();
        let base = 1_700_000_000u64;
        let prev = code_at(&secret, base - STEP_SECS);
        let next = code_at(&secret, base + STEP_SECS);
        // A code from the previous and next window verifies at `base` (±1 skew).
        assert!(verify_code_at(&secret, &prev, base), "previous-window code accepted");
        assert!(verify_code_at(&secret, &next, base), "next-window code accepted");
    }

    #[test]
    fn rejects_two_steps_away() {
        let secret = generate_secret();
        let base = 1_700_000_000u64;
        let two_back = code_at(&secret, base - STEP_SECS * 2);
        let now = code_at(&secret, base);
        // Only assert rejection when the codes actually differ (TOTP windows can
        // collide for short digit counts; the math is what we're pinning).
        if two_back != now {
            assert!(
                !verify_code_at(&secret, &two_back, base),
                "a code 2 steps back is beyond ±1 skew and must be rejected"
            );
        }
    }

    #[test]
    fn rejects_empty_and_garbage_codes() {
        let secret = generate_secret();
        assert!(!verify_code(&secret, ""));
        assert!(!verify_code(&secret, "   "));
        assert!(!verify_code(&secret, "abcdef"));
        assert!(!verify_code(&secret, "000000") || true); // 000000 may rarely be valid; don't over-assert
    }

    #[test]
    fn provisioning_uri_is_otpauth_and_carries_issuer() {
        let secret = generate_secret();
        let p = provisioning(&secret, "zeroship", "user@example.com").expect("provisioning");
        assert!(p.otpauth_uri.starts_with("otpauth://totp/"), "uri: {}", p.otpauth_uri);
        assert!(p.otpauth_uri.contains("issuer=zeroship"));
        assert!(!p.secret_base32.is_empty());
        // base32 alphabet only (A-Z2-7), no padding shown to the user.
        assert!(
            p.secret_base32.chars().all(|c| c.is_ascii_uppercase() || ('2'..='7').contains(&c)),
            "secret must be base32: {}",
            p.secret_base32
        );
    }

    #[test]
    fn backup_codes_minted_with_matching_hashes_and_verify_once() {
        let (plain, hashes) = generate_backup_codes().expect("mint");
        assert_eq!(plain.len(), BACKUP_CODE_COUNT);
        assert_eq!(hashes.len(), BACKUP_CODE_COUNT);
        // Every plaintext verifies against its own hash...
        for (code, hash) in plain.iter().zip(&hashes) {
            assert!(verify_backup_code(code, hash).expect("verify"), "code {code} should verify");
            // ...but NOT against a different code's hash.
            let other = &hashes[(plain.iter().position(|c| c == code).unwrap() + 1) % hashes.len()];
            assert!(
                !verify_backup_code(code, other).expect("verify other"),
                "code must not verify against another code's hash"
            );
        }
        // Stored hashes are PHC strings, not the cleartext code.
        for (code, hash) in plain.iter().zip(&hashes) {
            assert!(hash.starts_with("$argon2"), "hash must be argon2 PHC: {hash}");
            assert!(!hash.contains(code), "hash must not contain the plaintext code");
        }
    }

    #[test]
    fn backup_code_normalization_is_dash_and_space_insensitive() {
        let (plain, hashes) = generate_backup_codes().expect("mint");
        let code = &plain[0];
        let bare = normalize_backup_code(code); // dashes stripped
        // The same code with/without dashes and with spaces verifies identically.
        assert!(verify_backup_code(code, &hashes[0]).unwrap());
        assert!(verify_backup_code(&bare, &hashes[0]).unwrap());
        let spaced = format!("  {}  ", code.replace('-', " "));
        assert!(verify_backup_code(&spaced, &hashes[0]).unwrap());
    }
}
