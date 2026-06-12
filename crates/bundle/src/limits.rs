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

/// Default number of multipart `UploadPart` PUTs the S3 blob store keeps in
/// flight at once.
///
/// The source reader is still drained strictly sequentially (and hashed in
/// read order); only the network PUTs overlap. Bounded concurrency is the S3
/// throughput lever (the official `@aws-sdk/lib-storage` runs ~4 parallel
/// parts, ~2× a sequential `upload_part().await` loop) while keeping memory
/// bounded at `N × PART_SIZE`.
// Default 1 (SEQUENTIAL) until parallel multipart is verified at scale. A clean
// 5 GiB conc=8 e2e run crashed the worker (HTTP 000) at ~part 48 with RSS
// climbing past the bounded N x PART_SIZE — a resource-accumulation bug in the
// parallel path the pooled-client fix did not fully resolve. Opt into parallel
// via ZEROSHIP_*_UPLOAD_CONCURRENCY once verified on a clean machine.
pub const DEFAULT_UPLOAD_CONCURRENCY: usize = 1;

/// Environment variable overriding [`DEFAULT_UPLOAD_CONCURRENCY`]. Clamped to
/// `1..=64` (1 reproduces the old strictly-sequential behaviour).
pub const UPLOAD_CONCURRENCY_ENV: &str = "ZEROSHIP_BLOB_UPLOAD_CONCURRENCY";

/// Resolve the in-flight multipart part-upload concurrency for the S3 blob
/// store.
///
/// Reads the `ZEROSHIP_BLOB_UPLOAD_CONCURRENCY` env var (clamped to `1..=64`)
/// if a valid positive integer, else [`DEFAULT_UPLOAD_CONCURRENCY`].
#[must_use]
pub fn upload_concurrency() -> usize {
    std::env::var(UPLOAD_CONCURRENCY_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .map_or(DEFAULT_UPLOAD_CONCURRENCY, |n| n.clamp(1, 64))
}
