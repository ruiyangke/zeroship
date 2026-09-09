//! Cross-backend column encryption — AES-256-GCM at the storage
//! boundary, reached identically whichever backend arm is
//! configured.
//!
//! ## What this module ships
//!
//! Pure-Rust crypto surface, and it is **vendor-blind**: the call sites in
//! `crud/encryption_pass.rs` call [`aead`] directly and get their key material
//! from a [`KeyStore`] borrowed off the backend handle. Both backends went
//! through an `EncryptedColumn` capability trait until 2026-09-02, with
//! identical impls; the trait is deleted, because "which database is this" was
//! never a question column encryption needed answered.
//!
//! ## Layout
//!
//! - [`aead`] — [`AeadKey`] + the `pub(crate)` `encrypt_randomised` /
//!   `encrypt_deterministic` pair behind [`encrypt`], plus [`decrypt`].
//!   Mode-agnostic on the
//!   decrypt side (the synthetic-nonce vs random-nonce distinction
//!   lives only on the write path).
//! - [`keys`] — [`KeyStore`] caches `(app_id, key_id) → AeadKey`,
//!   derived via HKDF-SHA256 from a per-platform root key. One
//!   [`LocalKeySource`], two variants, both in-process: env-var lookup
//!   (`ZEROSHIP_COLUMN_KEY_<KEYID>`) or roots supplied to the process
//!   directly. Postgres and SQLite resolve through the same code.
//! - [`aad`] — canonical, length-prefixed AAD construction.
//!   `Randomised` mode binds `(collection, column, row_pk_bytes)`;
//!   `Deterministic` mode binds `(collection, column)` only.
//!   See `docs/archive/p5-encryption-backup-implementation-plan.md`
//!   §13 (Camp A resolution, 2026-05-24).
//! - [`wire`] — versioned framing: `[version_flag (1B) | nonce (12B)
//!   | ciphertext + tag (NB)]`. Version flag `0x01` is reserved for
//!   the baseline AAD shape; `0x02` is reserved for the post-
//!   system-fields shape that includes version bytes in AAD. The
//!   reservation costs one byte today and avoids a data migration of
//!   the baseline ciphertext later (re-encrypt-on-write suffices).
//!
//! ## Why "always compiled"
//!
//! Both the Postgres and SQLite arms consume this module, so it sits
//! outside the `pg` / `sqlite` Cargo feature gates. Default-feature
//! builds (`--features pg`) compile the module; the column-key store
//! is wired into `PostgresBackend` unconditionally.

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
    target = "zeroship-plugin-db",
    scope = "plugin_db_encryption");

pub mod aad;
pub mod aead;
pub mod keys;
pub mod wire;

#[allow(unused_imports)] // consumed by crud/encryption_pass.rs
pub use aad::canonical_aad;
// `encrypt_randomised` and `encrypt_deterministic` LEFT THIS LIST on
// 2026-09-02, under Phase 0.5's `pub(crate) -> pub` audit. Neither had a
// qualified reader outside this module: `aead::encrypt` is the only caller,
// and it picks between them from the column's `EncryptionMode`. The re-export
// was the entire reason they were public, which is the audit's shape exactly -
// an item public because of where it is listed, not because anything reads it.
//
// `AeadKey` stays public with ZERO named external readers, and that is correct
// rather than an oversight: it is the return type of `keys::KeyStore::resolve`,
// so callers obtain one by inference without ever writing the name. A
// reader-count alone would have narrowed it and broken the public signature.
#[allow(unused_imports)]
pub use aead::{AeadKey, decrypt, encrypt};
#[allow(unused_imports)]
pub use keys::{KeyStore, LocalKeySource, SuppliedRootKeys};
