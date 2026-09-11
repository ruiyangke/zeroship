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
    // Class `platform`, like the other three knobs in this file.
    positive_u64(zeroship_core::declared_env!(
        platform,
        "ZEROSHIP_STORAGE_MAX_OBJECT_BYTES",
        crate::StorageConsumer
    ))
    .unwrap_or(DEFAULT_MAX_OBJECT_BYTES)
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
    // Class `platform`, like the other three knobs in this file.
    positive_u64(zeroship_core::declared_env!(
        platform,
        "ZEROSHIP_STORAGE_MAX_STREAM_BYTES",
        crate::StorageConsumer
    ))
    .unwrap_or(DEFAULT_MAX_STREAM_OBJECT_BYTES)
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
// 4 matches lib-storage's default queueSize. Verified on a CLEAN (uncontended)
// box: 5 GiB conc=8 e2e completed in 244s (vs 268s sequential), memory bounded
// (401 MiB), object finalized + checksum-matched. Override via
// ZEROSHIP_STORAGE_UPLOAD_CONCURRENCY (clamped 1..=64).
pub const DEFAULT_UPLOAD_CONCURRENCY: usize = 4;

/// Environment variable overriding [`DEFAULT_UPLOAD_CONCURRENCY`]. Clamped to
/// `1..=64` (1 reproduces the old strictly-sequential behaviour).
pub const UPLOAD_CONCURRENCY_ENV: &str = "ZEROSHIP_STORAGE_UPLOAD_CONCURRENCY";

/// Resolve the in-flight multipart part-upload concurrency: the
/// `ZEROSHIP_STORAGE_UPLOAD_CONCURRENCY` env var (clamped to `1..=64`) if a
/// valid positive integer, else [`DEFAULT_UPLOAD_CONCURRENCY`].
#[must_use]
pub fn upload_concurrency() -> usize {
    // Class `platform`: zeroship-owned and read on the SERVER path (the worker
    // links this crate), so it is neither `cli` nor `dev` nor `test`. It is a
    // conversion candidate - it deserves a generated declaration with a flag,
    // a TOML path and a `--check-config` row, and has none yet.
    positive_u64(zeroship_core::declared_env!(
        platform,
        "ZEROSHIP_STORAGE_UPLOAD_CONCURRENCY",
        crate::StorageConsumer
    ))
        .map(|n| n.clamp(1, 64) as usize)
        .unwrap_or(DEFAULT_UPLOAD_CONCURRENCY)
}

// ---------------------------------------------------------------------------
// List pagination
// ---------------------------------------------------------------------------

/// Default `list` page size when the caller doesn't specify `limit`.
///
/// Mirrors `zeroship_kv::limits::LIST_DEFAULT_LIMIT` — the same
/// question (how many keys does one `list` call return) answered the same way,
/// so a creator moving between `env.kv.list` and `env.storage.list` does not
/// meet two different defaults.
pub const LIST_DEFAULT_LIMIT: usize = 1000;

/// Hard ceiling on a `list` page. A caller-supplied `limit` above this is
/// **clamped**, not rejected — and the clamp is never silent, because a
/// clamped page that does not exhaust the listing still reports a
/// [`crate::backend::ListPage::cursor`]. Also mirrors kv-v8.
pub const LIST_MAX_LIMIT: usize = 10_000;

/// Normalise a caller-supplied `list` limit into the effective page size.
/// `None` → [`LIST_DEFAULT_LIMIT`]; anything above [`LIST_MAX_LIMIT`] is
/// clamped; a non-positive or non-finite value falls back to the default.
#[must_use]
pub fn resolve_list_limit(limit: Option<f64>) -> usize {
    match limit {
        Some(l) if l.is_finite() && l >= 1.0 => (l as usize).min(LIST_MAX_LIMIT),
        _ => LIST_DEFAULT_LIMIT,
    }
}

/// Parse an already-read value as a positive `u64`.
///
/// This takes the VALUE, not the name. As `env_u64(name: &str)` it performed
/// the read itself, so the two names this file governs were invisible to any
/// scan of it: the literals sat at the call sites and the read sat here. The
/// read now lives next to its literal in each resolver.
fn positive_u64(raw: Option<String>) -> Option<u64> {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
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
    fn resolve_list_limit_defaults_and_clamps() {
        assert_eq!(resolve_list_limit(None), LIST_DEFAULT_LIMIT);
        assert_eq!(resolve_list_limit(Some(50.0)), 50);
        assert_eq!(resolve_list_limit(Some(1e9)), LIST_MAX_LIMIT);
        assert_eq!(resolve_list_limit(Some(0.0)), LIST_DEFAULT_LIMIT);
        assert_eq!(resolve_list_limit(Some(-5.0)), LIST_DEFAULT_LIMIT);
        assert_eq!(resolve_list_limit(Some(f64::NAN)), LIST_DEFAULT_LIMIT);
        assert_eq!(resolve_list_limit(Some(f64::INFINITY)), LIST_DEFAULT_LIMIT);
    }

    #[test]
    fn upload_concurrency_default_is_4() {
        // No env override in this test process → default.
        assert!(zeroship_core::test_env!("ZEROSHIP_STORAGE_UPLOAD_CONCURRENCY").is_none());
        assert_eq!(upload_concurrency(), DEFAULT_UPLOAD_CONCURRENCY);
        assert_eq!(DEFAULT_UPLOAD_CONCURRENCY, 4);
    }
}
