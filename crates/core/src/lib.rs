//! zeroship-common — shared types and abstractions for the zeroship platform.

pub mod types;
pub mod vfs;
pub mod auth;
pub mod typed_id;
pub mod crypto;
pub mod blob;

pub use types::*;
pub use blob::{BlobError, BlobStore, LocalDiskBlobStore, sha256_hex, validate_hash_format};
