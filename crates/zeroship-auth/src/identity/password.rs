//! Argon2id password hashing + enumeration-resistant verification.
//!
//! Real and dummy credentials use the same hashing configuration. The resulting
//! PHC strings carry the algorithm, parameters, salt and password hash.
//!
//! Argon2 is CPU-bound and synchronous; callers running on the ntex
//! event loop wrap calls in `compio::runtime::spawn_blocking` to avoid
//! parking the loop.

use argon2::{Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version};
use password_hash::{rand_core::OsRng, SaltString};
use std::sync::OnceLock;

use crate::error::{AuthError, Result};

/// Minimum password length, in CHARACTERS (not bytes, so a passphrase of
/// non-ASCII graphemes is not penalised for its encoding).
///
/// One value, because this policy has to agree in four places: the `/signup`
/// and `/reset` handlers, which are the enforcement, and the `minlength`
/// attribute on both forms, which is the promise made to the user before they
/// type. The two Rust sites now read this; `template_password_policy_test.rs`
/// is what holds the two HTML attributes to it, since a template cannot import
/// a constant.
///
/// A split between them is not merely untidy. If the attribute is lower than
/// the check, the form accepts a password the server then rejects, and the user
/// is told "at least 15 characters" by a page that just let them submit 8. If
/// it is higher, the advertised policy is stricter than the enforced one.
pub const MIN_PASSWORD_CHARS: usize = 15;

fn argon2() -> Argon2<'static> {
    let params = Params::new(19_456, 2, 1, None).expect("argon2 params");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// Hash a password, returning a PHC string suitable for `zeroship.users.password_hash`.
///
/// # Errors
///
/// Returns `AuthError::Internal` on argon2 misconfiguration (shouldn't happen
/// in practice — params are fixed at compile time).
pub fn hash(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let phc = argon2()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| AuthError::Internal(format!("argon2 hash: {e}")))?;
    Ok(phc.to_string())
}

/// Verify a password against a stored PHC string.
///
/// Returns `Ok(true)` on match, `Ok(false)` on mismatch. Only returns `Err`
/// when the PHC string itself is malformed.
///
/// # Errors
///
/// Returns `AuthError::Internal` on argon2 parse/verify failure.
pub fn verify(password: &str, phc: &str) -> Result<bool> {
    let parsed =
        PasswordHash::new(phc).map_err(|e| AuthError::Internal(format!("argon2 parse: {e}")))?;
    match argon2().verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(password_hash::Error::Password) => Ok(false),
        Err(e) => Err(AuthError::Internal(format!("argon2 verify: {e}"))),
    }
}

/// Pre-computed dummy hash for the missing-user branch of `/login`.
///
/// Used by the login handler when no user matches the submitted email, so the
/// failure path verifies a hash with the same parameters as a real credential.
/// The credential verifier must still refuse an absent or ineligible account
/// even when the submitted password matches this padding hash.
///
/// Hashed once on first call and memoised.
///
/// # Panics
///
/// Panics if Argon2 hashing of the padding constant fails — which would
/// indicate the global Argon2 configuration is corrupt, an unrecoverable
/// invariant violation.
#[must_use]
pub fn dummy_hash() -> &'static str {
    static D: OnceLock<String> = OnceLock::new();
    D.get_or_init(|| hash("absent-user-padding").expect("dummy hash"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independently_salted_hashes_verify_only_the_matching_password() {
        let password = "password hash roundtrip phrase";
        let first = hash(password).unwrap();
        let second = hash(password).unwrap();
        assert_ne!(first, second);
        for phc in [&first, &second] {
            let parsed = PasswordHash::new(phc).unwrap();
            assert_eq!(parsed.algorithm.as_str(), "argon2id");
            assert!(verify(password, phc).unwrap());
            assert!(!verify("incorrect password phrase", phc).unwrap());
        }
    }

    #[test]
    fn dummy_hash_uses_the_current_password_hash_parameters() {
        let real = hash("real account password phrase").unwrap();
        let real = PasswordHash::new(&real).unwrap();
        let dummy = PasswordHash::new(dummy_hash()).unwrap();
        assert_eq!(dummy.algorithm, real.algorithm);
        assert_eq!(dummy.version, real.version);
        assert_eq!(dummy.params, real.params);
        assert_ne!(dummy.salt, real.salt);
        assert_eq!(dummy.hash.unwrap().len(), real.hash.unwrap().len());
        assert!(!verify("real account password phrase", dummy_hash()).unwrap());
    }
}
