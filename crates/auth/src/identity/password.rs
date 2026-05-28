//! Argon2id password hashing + enumeration-resistant verification.
//!
//! Per proposal §8.1: OWASP 2026 second-recommended params
//! (m = 19 MiB / 19456 KiB, t = 2, p = 1). Returns PHC strings.
//!
//! Argon2 is CPU-bound and synchronous; callers running on the ntex
//! event loop wrap calls in `compio::runtime::spawn_blocking` to avoid
//! parking the loop.

use argon2::{Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version};
use password_hash::{rand_core::OsRng, SaltString};
use std::sync::OnceLock;

use crate::error::{AuthError, Result};

fn argon2() -> Argon2<'static> {
    let params = Params::new(19_456, 2, 1, None).expect("argon2 params");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// Hash a password, returning a PHC string suitable for `auth.users.password_hash`.
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
    let parsed = PasswordHash::new(phc)
        .map_err(|e| AuthError::Internal(format!("argon2 parse: {e}")))?;
    match argon2().verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(password_hash::Error::Password) => Ok(false),
        Err(e) => Err(AuthError::Internal(format!("argon2 verify: {e}"))),
    }
}

/// Pre-computed dummy hash for the missing-user branch of `/login`.
///
/// Used by the login handler when no user matches the submitted email, so the
/// failure path runs the same code and spends the same wall time as the real
/// verify path. Defeats login-side email enumeration.
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

/// Verify against the dummy hash. Always returns `Ok(false)` but spends the
/// same wall time as a real verify call.
///
/// # Errors
///
/// Returns `AuthError::Internal` on argon2 misconfiguration (should not occur).
pub fn verify_against_dummy(password: &str) -> Result<bool> {
    verify(password, dummy_hash())
}
