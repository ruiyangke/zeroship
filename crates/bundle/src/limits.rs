//! Size and count limits enforced during `.zship` ingestion.
//! See `docs/reference/zship.md` "Limits" section.

/// Compressed-body cap. Wire-level limit before any decompression.
pub const MAX_COMPRESSED_BYTES: usize = 256 * 1024 * 1024;

/// Decompressed-body cap. Tracked across tar entries during streaming.
pub const MAX_DECOMPRESSED_BYTES: u64 = 256 * 1024 * 1024;

/// Manifest entry size cap (always the first tar entry).
pub const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// Single-blob size cap.
pub const MAX_BLOB_BYTES: u64 = 16 * 1024 * 1024;

/// Total blob count cap per deploy.
pub const MAX_BLOBS_PER_DEPLOY: usize = 10_000;
