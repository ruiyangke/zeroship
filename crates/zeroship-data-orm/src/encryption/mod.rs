//! Randomised column encryption, row-bound authentication and host key resolution.

pub mod aad;
pub mod aead;
pub mod keys;
pub(crate) mod plaintext;
pub mod wire;

#[allow(unused_imports)] // consumed by the protection write pass
pub use aad::canonical_aad;
pub use aead::{AeadKey, decrypt, encrypt};
pub use keys::{KeyStore, ProjectKeySource, SuppliedProjectKeys, derive_key};

use crate::binding::DbBinding;
use crate::error::DbError;
use zeroship_core::DatabaseId;

/// The database whose key and authenticated context a binding's encrypted
/// columns are protected under.
///
/// A platform store - auth, control's catalog, the workflow manager - narrows
/// to nothing and names no database, so there is no salt to expand its column
/// key from and no id to bind its tags to. That is REFUSED rather than
/// defaulted: a default would key every trusted service's columns alike, and
/// would do it silently.
///
/// # Errors
///
/// [`DbError::Configuration`] with code `encryption_requires_a_database`.
pub fn encryption_database(binding: &DbBinding) -> Result<&DatabaseId, DbError> {
    binding.database().ok_or_else(|| DbError::Configuration {
        code: "encryption_requires_a_database",
        message: format!(
            "db: '{}' opened a store that addresses no database, and an encrypted \
             column has no key without one",
            binding.app_id()
        ),
        hint: Some(
            "Encrypted columns belong to a creator database reached through a binding, \
             not to a platform schema opened under a service login."
                .to_owned(),
        ),
    })
}
