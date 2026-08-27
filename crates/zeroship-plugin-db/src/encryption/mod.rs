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
//!   derived via HKDF-SHA256 from a per-platform root key. Two
//!   [`KeySource`] variants: env-var lookup (`ZEROSHIP_COLUMN_KEY_<KEYID>`)
//!   for the SQLite tier and PG dev parity, and the PG source that
//!   tries `__zeroship_admin.get_column_key` -- a getter with no
//!   installer since the admin schema was deleted, so it always falls
//!   back to the local source.
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

pub mod aad;
pub mod aead;
pub mod keys;
pub mod wire;

#[allow(unused_imports)] // consumed by crud/encryption_pass.rs
pub use aad::canonical_aad;
#[allow(unused_imports)]
pub use aead::{decrypt, encrypt_deterministic, encrypt_randomised, AeadKey};
#[allow(unused_imports)]
pub use keys::{KeySource, KeyStore, LocalKeySource, SuppliedRootKeys};
