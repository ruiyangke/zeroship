//! Size ceilings for the buffered `env.storage` convenience surface.
//!
//! Streaming (`putStream` / `getStream`) is unbounded by design — memory is
//! bounded by the part size on upload and the consumer's pull rate on
//! download. The buffered `put` / `get` conveniences, however, materialise the
//! whole object in RAM (and `get` base64-encodes it on the way back to JS,
//! ~1.33× the raw size, plus the decoded `Vec` → ~2.3× peak). An unbounded
//! buffered `get` driven by an attacker-controlled `Content-Length` is an
//! OOM-DoS, so the buffered path is capped.
//!
//! The cap mirrors the proposal's `ZEROSHIP_STORAGE_MAX_OBJECT_BYTES`
//! (default 16 MiB) and is the same ceiling the buffered `put` enforces.

/// Default buffered-object cap (16 MiB). Applies to the buffered `get`/`put`
/// conveniences only; streaming bypasses it.
pub const DEFAULT_MAX_OBJECT_BYTES: u64 = 16 * 1024 * 1024;

/// Environment variable that overrides [`DEFAULT_MAX_OBJECT_BYTES`].
pub const MAX_OBJECT_BYTES_ENV: &str = "ZEROSHIP_STORAGE_MAX_OBJECT_BYTES";

/// Resolve the buffered-object cap: the `ZEROSHIP_STORAGE_MAX_OBJECT_BYTES`
/// env var if set to a valid positive integer, else [`DEFAULT_MAX_OBJECT_BYTES`].
#[must_use]
pub fn max_object_bytes() -> u64 {
    env_u64(MAX_OBJECT_BYTES_ENV).unwrap_or(DEFAULT_MAX_OBJECT_BYTES)
}

// ---------------------------------------------------------------------------
// Streaming-upload ceiling
// ---------------------------------------------------------------------------

/// S3's hard ceiling on multipart parts (1..=10000). A `put_stream` that would
/// emit more than this can never `complete`, so the upload fails fast at the
/// offending part rather than wasting every prior `UploadPart`.
pub const MAX_MULTIPART_PARTS: u32 = 10_000;

/// Default ceiling for a single streamed object (32 GiB). Streaming is
/// unbounded *relative to RAM* (memory is bounded by the part size), but an
/// unbounded *total* lets a single app write without limit; this is the
/// fail-fast guard. Configurable via [`MAX_STREAM_OBJECT_BYTES_ENV`].
pub const DEFAULT_MAX_STREAM_OBJECT_BYTES: u64 = 32 * 1024 * 1024 * 1024;

/// Environment variable overriding [`DEFAULT_MAX_STREAM_OBJECT_BYTES`].
pub const MAX_STREAM_OBJECT_BYTES_ENV: &str = "ZEROSHIP_STORAGE_MAX_STREAM_BYTES";

/// Resolve the streamed-object size ceiling: the
/// `ZEROSHIP_STORAGE_MAX_STREAM_BYTES` env var if a valid positive integer,
/// else [`DEFAULT_MAX_STREAM_OBJECT_BYTES`].
#[must_use]
pub fn max_stream_object_bytes() -> u64 {
    env_u64(MAX_STREAM_OBJECT_BYTES_ENV).unwrap_or(DEFAULT_MAX_STREAM_OBJECT_BYTES)
}

// ---------------------------------------------------------------------------
// Multipart upload concurrency
// ---------------------------------------------------------------------------

/// Default number of multipart `UploadPart` requests in flight at once.
///
/// The source stream is still read strictly sequentially into one part buffer
/// at a time; only the network PUTs overlap. Bounded concurrency is the S3
/// throughput lever (the official `@aws-sdk/lib-storage` runs ~4 parallel
/// parts and is ~2× a sequential `upload_part().await` loop) while keeping
/// memory bounded: at most `UPLOAD_CONCURRENCY × PART_SIZE` of part buffers
/// can be in flight (4 × 8 MiB = 32 MiB), never the whole object.
pub const DEFAULT_UPLOAD_CONCURRENCY: usize = 4;

/// Environment variable overriding [`DEFAULT_UPLOAD_CONCURRENCY`]. Clamped to
/// `1..=64` (1 reproduces the old strictly-sequential behaviour).
pub const UPLOAD_CONCURRENCY_ENV: &str = "ZEROSHIP_STORAGE_UPLOAD_CONCURRENCY";

/// Resolve the in-flight multipart part-upload concurrency: the
/// `ZEROSHIP_STORAGE_UPLOAD_CONCURRENCY` env var (clamped to `1..=64`) if a
/// valid positive integer, else [`DEFAULT_UPLOAD_CONCURRENCY`].
#[must_use]
pub fn upload_concurrency() -> usize {
    env_u64(UPLOAD_CONCURRENCY_ENV)
        .map(|n| n.clamp(1, 64) as usize)
        .unwrap_or(DEFAULT_UPLOAD_CONCURRENCY)
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_16_mib() {
        assert_eq!(DEFAULT_MAX_OBJECT_BYTES, 16 * 1024 * 1024);
    }

    #[test]
    fn s3_part_ceiling_is_10000() {
        assert_eq!(MAX_MULTIPART_PARTS, 10_000);
    }

    #[test]
    fn upload_concurrency_default_is_4() {
        // No env override in this test process → default.
        assert!(std::env::var(UPLOAD_CONCURRENCY_ENV).is_err());
        assert_eq!(upload_concurrency(), DEFAULT_UPLOAD_CONCURRENCY);
        assert_eq!(DEFAULT_UPLOAD_CONCURRENCY, 4);
    }
}
