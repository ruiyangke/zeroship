//! Cross-backend column encryption — AES-256-GCM at the storage
//! boundary, used by both the Postgres and SQLite [`crate::backend`]
//! impls via the [`crate::backend::EncryptedColumn`] capability trait.
//!
//! ## What this module ships
//!
//! Pure-Rust crypto surface. `PostgresBackend` / `SqliteBackend` both
//! implement [`crate::backend::EncryptedColumn`]; the call sites live in
//! `crud/encryption_pass.rs`.
//!
//! ## Layout
//!
//! - [`aead`] — [`AeadKey`] + [`encrypt_randomised`] /
//!   [`encrypt_deterministic`] / [`decrypt`]. Mode-agnostic on the
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
#[allow(unused_imports)]
pub use aead::{decrypt, encrypt_deterministic, encrypt_randomised, AeadKey};
#[allow(unused_imports)]
pub use keys::{KeyStore, LocalKeySource, SuppliedRootKeys};
