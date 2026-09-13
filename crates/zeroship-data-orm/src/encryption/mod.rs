//! Randomised column encryption, row-bound authentication and host key resolution.

pub mod aad;
pub mod aead;
pub mod keys;
pub(crate) mod plaintext;
pub mod wire;

#[allow(unused_imports)] // consumed by the protection write pass
pub use aad::canonical_aad;
pub use aead::{AeadKey, decrypt, encrypt};
pub use keys::{KeyStore, ProjectKeySource, SuppliedProjectKeys};
