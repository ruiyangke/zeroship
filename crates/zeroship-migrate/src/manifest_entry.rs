//! The deploy-bundle migration-file record + content-addressed hash.
//!
//! Vendored byte-identically from `zeroship_bundle` (extraction Phase B) so the
//! migrate engine's build front-end can emit bundle migration entries over
//! committed `.ir.json` bytes without a normal-graph dependency on
//! `zeroship-bundle`:
//!
//! - [`MigrationFileEntry`] — the manifest entry type, copied from
//!   `zeroship_bundle::manifest::MigrationFileEntry` (identical fields + serde).
//! - [`sha256_hex`] — the canonical content-addressed hash, copied from
//!   `zeroship_bundle::sha256_hex`.
//!
//! The serde shape and the hash bytes must stay identical to the originals: the
//! committed migration `.ir.json` hash is content-addressed, so any drift would
//! change the recorded entry hash.

use serde::{Deserialize, Serialize};

/// One logical migration file entry produced by the migration build frontend.
///
/// Migration documents are applied through the standalone migration service and
/// are not carried in the deploy manifest. The type is vendored here because the
/// migration build front-end uses it as a shared build-artifact record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigrationFileEntry {
    /// Logical migration filename, e.g. `20240617123000_create_users.ir.json`.
    pub name: String,
    /// sha256 hash (lowercase, 64 hex chars) of the migration file body.
    pub hash: String,
}

/// SHA-256 of `data`, lowercase hex.
#[must_use]
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(data);
    hex::encode(digest)
}
