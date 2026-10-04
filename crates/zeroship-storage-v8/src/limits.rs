//! Limits for V8 upload buffers and isolate-owned download handles.

/// Buffer shared by the V8 producer and the Rust upload consumer.
pub const UPLOAD_STREAM_BUFFER_CAP: usize = 16 * 1024 * 1024;

/// Bytes one `readChunk` resolves with.
///
/// A download gathers backend frames until it holds this many or the object
/// ends, so the V8 round trip is paid once per this many bytes rather than
/// once per network frame. Every chunk but an object's last is exactly this
/// size, and it bounds what an in-flight read holds beyond the remainder of
/// one backend frame.
pub const DOWNLOAD_CHUNK_BYTES: usize = 256 * 1024;

/// How long a download keeps gathering once a chunk holds its first byte.
///
/// The gather ends at the first frame to arrive after this, so a backend
/// that trickles frames hands JavaScript a partial chunk rather than holding
/// one read for a frame wait per frame the chunk needs.
pub const DOWNLOAD_GATHER_BUDGET: std::time::Duration = std::time::Duration::from_millis(100);

pub const DEFAULT_MAX_LIVE_GET_STREAMS_PER_APP: usize = 64;

/// Environment variable overriding [`DEFAULT_MAX_LIVE_GET_STREAMS_PER_APP`].
pub const MAX_LIVE_GET_STREAMS_PER_APP_ENV: &str = "ZEROSHIP_STORAGE_MAX_LIVE_GET_STREAMS";

/// Hard ceiling on the configured cap. Stream ids are `u32`, so a cap at or
/// above `u32::MAX` would let an app fill the whole id space and leave
/// `live_streams::open`'s free-id probe with nothing to find. Clamping here
/// keeps "a free id always exists" a property of the type, not of operator
/// discipline. (Unreachable in practice — each stream also pins an fd.)
const MAX_LIVE_GET_STREAMS_CEILING: usize = (u32::MAX - 1) as usize;

/// Resolve the per-app live-download-stream ceiling: the
/// `ZEROSHIP_STORAGE_MAX_LIVE_GET_STREAMS` env var if a valid positive
/// integer (clamped to [`MAX_LIVE_GET_STREAMS_CEILING`]), else
/// [`DEFAULT_MAX_LIVE_GET_STREAMS_PER_APP`].
#[must_use]
pub fn max_live_get_streams_per_app() -> usize {
    positive_u64(zeroship_core::declared_env!(
        platform,
        "ZEROSHIP_STORAGE_MAX_LIVE_GET_STREAMS",
        crate::StorageV8Consumer
    ))
    .and_then(|n| usize::try_from(n).ok())
    .map(|n| n.min(MAX_LIVE_GET_STREAMS_CEILING))
    .unwrap_or(DEFAULT_MAX_LIVE_GET_STREAMS_PER_APP)
}

fn positive_u64(raw: Option<String>) -> Option<u64> {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
}
