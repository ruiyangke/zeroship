//! `.zship` deploy artifact format.
//!
//! Wire format: see `docs/reference/zship.md`.
//! Storage layer rationale: see `docs/architecture/blob-store.md`.

pub mod asset;
pub mod blob;
pub mod blob_config;
pub mod limits;
pub mod manifest;
pub mod rule;
pub mod s3_blob;
pub mod store;
pub mod unpack;

pub use asset::{AssetEntry, AssetVariant};
pub use blob::{BlobError, BlobStore, LocalDiskBlobStore, PutOutcome, sha256_hex, validate_hash_format};
pub use blob_config::{
    build_blob_store, s3_credentials_from_env, BlobStoreConfigError, StoreUrl,
};
pub use s3_blob::{S3BlobStore, PART_SIZE};
pub use limits::{
    MAX_BLOBS_PER_DEPLOY, MAX_BLOB_BYTES, MAX_COMPRESSED_BYTES, MAX_DECOMPRESSED_BYTES,
    MAX_MANIFEST_BYTES,
};
pub use manifest::{
    AuthConfig, HandlerEntry, Manifest, ManifestExports, ManifestMetadata, ScopeDef, WorkerCode,
};
pub use rule::{
    Action, AuthLevel, CacheCtl, Cors, HttpMethod, Match, ProcedureKind, RateLimit,
    RateLimitPer, RedirectAction, ResourceEntry, Rule, StaticAction, WorkerMode,
};
pub use store::{BundleStore, LocalFs, VfsError, VfsResult};
pub use unpack::{ingest, IngestError, IngestSuccess};
