//! Randomised column encryption, row-bound authentication and host key resolution.

zeroship_core::declare_env_consumer!(
    /// This tier's own identity for the environment it reads.
    ///
    /// **The encryption tier reads `ZEROSHIP_COLUMN_KEY_<KEYID>` and therefore
    /// names itself as the reader.** It used to name `crate::PluginDbConsumer`,
    /// which is declared in `lib.rs` - the ADAPTER - so the lowest tier in the
    /// crate reached the highest one for an identity, and
    /// `tests/lib/tier_direction_census.sh` reported it as
    /// `ENCRYPT -> ADAPTER`. Nothing about reading a key needs the adapter to
    /// exist.
    ///
    /// `target` is the cargo PACKAGE, not a tier, so it is correct today and
    /// stays correct without edit when this module moves into its own crate -
    /// the macro takes the literal, but the literal is a build fact that the
    /// move updates once, here.
    ///
    /// `scope` is deliberately NOT `plugin_db`: the scope is recorded with
    /// every read, and two components sharing one scope makes the record unable
    /// to say which of them read the key.
    pub EncryptionConsumer,
    target = "zeroship-data-v8",
    scope = "plugin_db_encryption");

pub mod aad;
pub mod aead;
pub mod keys;
pub mod wire;

#[allow(unused_imports)] // consumed by crud/encryption_pass.rs
pub use aad::canonical_aad;
pub use aead::{AeadKey, decrypt, encrypt};
pub use keys::{KeyStore, LocalKeySource, SuppliedRootKeys};
