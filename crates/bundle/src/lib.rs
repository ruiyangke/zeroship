//! `.zsapp` deploy artifact format.
//!
//! Wire format: see `docs/reference/zsapp.md`.
//! Storage layer rationale: see `docs/architecture/blob-store.md`.

pub mod asset;
pub mod blob;
pub mod limits;
pub mod manifest;
pub mod rule;
pub mod store;
pub mod unpack;

pub use asset::{AssetEntry, AssetVariant};
pub use blob::{BlobError, BlobStore, LocalDiskBlobStore, PutOutcome, sha256_hex, validate_hash_format};
pub use limits::{
    MAX_BLOBS_PER_DEPLOY, MAX_BLOB_BYTES, MAX_COMPRESSED_BYTES, MAX_DECOMPRESSED_BYTES,
    MAX_MANIFEST_BYTES,
};
pub use manifest::{Manifest, ManifestMetadata, WorkerCode};
pub use rule::{
    Action, AuthLevel, CacheCtl, Cors, HttpMethod, Match, ProcedureKind, RateLimit,
    RateLimitPer, RedirectAction, ResourceEntry, Rule, StaticAction, WorkerMode,
};
pub use store::{BundleStore, LocalFs, VfsError, VfsResult};
pub use unpack::{ingest, IngestError, IngestSuccess};
