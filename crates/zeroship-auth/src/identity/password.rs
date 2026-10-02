//! Argon2id password hashing + enumeration-resistant verification.
//!
//! Real and padding credentials use the same hashing configuration. The
//! resulting PHC strings carry the algorithm, parameters, salt and password
//! hash.
//!
//! Argon2 is CPU-bound and synchronous, and every caller runs on an event loop
//! that other connections share. So the public surface is async and each
//! evaluation runs on the runtime's blocking pool; the synchronous core is
//! private to this module. A caller cannot park its event loop on a hash,
//! because there is no blocking entry point to call.

use argon2::{Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version};
use password_hash::{rand_core::OsRng, SaltString};
use std::sync::OnceLock;

use crate::error::{AuthError, Result};

/// Minimum password length enforced by signup and password reset.
///
/// Length is measured in Unicode scalar values through `str::chars`, so UTF-8
/// encoding width does not affect eligibility. Password forms should advertise
/// the same policy as their handlers.
pub const MIN_PASSWORD_CHARS: usize = 15;

/// The password the padding credential is a hash of.
const PADDING_PASSWORD: &str = "absent-user-padding";

fn argon2() -> Argon2<'static> {
    let params = Params::new(19_456, 2, 1, None).expect("argon2 params");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// Run Argon2 work on the current runtime's blocking pool.
async fn off_loop<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    compio::runtime::spawn_blocking(work)
        .await
        .map_err(|_| AuthError::Internal("argon2 worker panicked".to_owned()))?
}

fn hash_blocking(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let phc = argon2()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| AuthError::Internal(format!("argon2 hash: {e}")))?;
    Ok(phc.to_string())
}

fn verify_blocking(password: &str, phc: &str) -> Result<bool> {
    let parsed =
        PasswordHash::new(phc).map_err(|e| AuthError::Internal(format!("argon2 parse: {e}")))?;
    match argon2().verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(password_hash::Error::Password) => Ok(false),
        Err(e) => Err(AuthError::Internal(format!("argon2 verify: {e}"))),
    }
}

/// Return the value in `cell`, computing and storing it once when the cell is
/// empty.
///
/// The compute closure runs inside the one-time initializer, so callers that
/// arrive together race to a single evaluation: one closure runs, and every
/// other caller blocks until that value is published and observes it.
fn memoised<T>(cell: &OnceLock<T>, compute: impl FnOnce() -> T) -> &T {
    cell.get_or_init(compute)
}

/// The padding credential, hashed exactly once and memoised.
///
/// Reached only from inside [`off_loop`] work, so the one-time hash runs on the
/// blocking pool like every other evaluation.
fn padding_blocking() -> &'static str {
    static PADDING: OnceLock<String> = OnceLock::new();
    memoised(&PADDING, || {
        // `hash_blocking` fails only on argon2 misconfiguration, and the
        // parameters are constants in `argon2`; there is no input to vary them.
        hash_blocking(PADDING_PASSWORD).expect("padding hash parameters are constants")
    })
    .as_str()
}

/// Hash a password, returning a PHC string suitable for `zeroship.users.password_hash`.
///
/// # Errors
///
/// Returns `AuthError::Internal` on argon2 misconfiguration (shouldn't happen
/// in practice - params are fixed at compile time) or if the blocking worker
/// panics.
pub async fn hash(password: &str) -> Result<String> {
    let password = password.to_owned();
    off_loop(move || hash_blocking(&password)).await
}

/// Verify a password against a stored PHC string.
///
/// Returns `Ok(true)` on match, `Ok(false)` on mismatch. Only returns `Err`
/// when the PHC string itself is malformed or the blocking worker panics.
///
/// # Errors
///
/// Returns `AuthError::Internal` on argon2 parse/verify failure.
pub async fn verify(password: &str, phc: &str) -> Result<bool> {
    let (password, phc) = (password.to_owned(), phc.to_owned());
    off_loop(move || verify_blocking(&password, &phc)).await
}

/// Verify a sign-in password against the account's stored hash, or spend the
/// same Argon2 work against a padding credential when there is no hash the
/// caller may check.
///
/// `phc` is `None` for an absent, ineligible or password-less account. The
/// caller still pays one full verify with the real parameters, so the wall time
/// does not reveal which case it was, and the answer is `false` whatever the
/// submitted password: padding can never admit anyone, even a password that
/// happens to match it.
///
/// # Errors
///
/// Returns `AuthError::Internal` when the PHC string is malformed or the
/// blocking worker panics.
pub async fn verify_or_pad(password: &str, phc: Option<&str>) -> Result<bool> {
    let (password, phc) = (password.to_owned(), phc.map(str::to_owned));
    off_loop(move || {
        phc.map_or_else(
            || verify_blocking(&password, padding_blocking()).map(|_| false),
            |phc| verify_blocking(&password, &phc),
        )
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[compio::test]
    async fn independently_salted_hashes_verify_only_the_matching_password() {
        let password = "password hash roundtrip phrase";
        let first = hash(password).await.unwrap();
        let second = hash(password).await.unwrap();
        assert_ne!(first, second);
        for phc in [&first, &second] {
            let parsed = PasswordHash::new(phc).unwrap();
            assert_eq!(parsed.algorithm.as_str(), "argon2id");
            assert!(verify(password, phc).await.unwrap());
            assert!(!verify("incorrect password phrase", phc).await.unwrap());
        }
    }

    #[compio::test]
    async fn padding_uses_the_current_password_hash_parameters() {
        let real = hash("real account password phrase").await.unwrap();
        let real = PasswordHash::new(&real).unwrap();
        let padding = padding_blocking();
        let padding = PasswordHash::new(padding).unwrap();
        assert_eq!(padding.algorithm, real.algorithm);
        assert_eq!(padding.version, real.version);
        assert_eq!(padding.params, real.params);
        assert_ne!(padding.salt, real.salt);
        assert_eq!(padding.hash.unwrap().len(), real.hash.unwrap().len());
    }

    /// The control is the padding's own password: it DOES match the padding
    /// credential, and `verify_or_pad` still answers `false` for it.
    #[compio::test]
    async fn padding_never_verifies_even_the_password_it_was_hashed_from() {
        let padding = padding_blocking();
        assert!(
            verify(PADDING_PASSWORD, padding).await.unwrap(),
            "the control must submit the password the padding actually matches"
        );
        assert!(!verify_or_pad(PADDING_PASSWORD, None).await.unwrap());
        assert!(!verify_or_pad("any other password", None).await.unwrap());

        let real = hash("real account password phrase").await.unwrap();
        assert!(verify_or_pad("real account password phrase", Some(&real))
            .await
            .unwrap());
        assert!(!verify_or_pad("wrong password phrase", Some(&real))
            .await
            .unwrap());
    }

    /// A cold cell hit by many callers at once evaluates its closure once, and
    /// every caller observes that one value.
    ///
    /// A compute that ran before the one-time initializer would evaluate once
    /// per racing caller; the count pins the work to the initializer where it
    /// belongs.
    #[test]
    fn memoised_computes_once_for_callers_that_race() {
        use std::sync::Barrier;
        use std::sync::atomic::{AtomicUsize, Ordering};

        const CALLERS: usize = 16;
        let cell = OnceLock::new();
        let evaluations = AtomicUsize::new(0);
        let start = Barrier::new(CALLERS);
        let seen: Vec<String> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..CALLERS)
                .map(|_| {
                    scope.spawn(|| {
                        start.wait();
                        memoised(&cell, || {
                            evaluations.fetch_add(1, Ordering::SeqCst);
                            // Let the other racers reach their empty check
                            // before this evaluation publishes, so a closure
                            // that runs outside the initializer is counted.
                            std::thread::yield_now();
                            "padding credential".to_owned()
                        })
                        .clone()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });
        assert_eq!(
            evaluations.load(Ordering::SeqCst),
            1,
            "the closure must run once for concurrent cold callers"
        );
        assert!(
            seen.iter().all(|value| value == "padding credential"),
            "every caller must observe the one computed value: {seen:?}"
        );
    }
}
