//! Streaming static-asset chunk readers.
//!
//! Large blobs (≥ `STREAM_THRESHOLD_BYTES`) bypass the in-memory
//! `BlobCache` and stream off the disk LRU in fixed-size chunks. The
//! readers here turn a `compio::fs::File` (or, in tests, an in-memory
//! `Vec<u8>`) into an mpsc receiver of `Bytes` chunks that ntex's
//! `SizedStream` body can consume.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use ntex::util::Bytes;
use uuid::Uuid;

use zeroship_metering::Meter;

// ---------------------------------------------------------------------------
// Streaming static-asset serving
// ---------------------------------------------------------------------------

// Blobs at or above this size skip the in-memory cache and stream from
// disk in fixed-size chunks. Holding 5 MB-plus assets in `BlobCache`
// would either evict everything else (single-entry budget bypass) or
// be silently dropped (single-entry budget exceeded), so streaming is
// both a correctness and a footprint win for large blobs. 1 MiB
// matches the value cited in `docs/architecture/blob-store.md` and is a
// clean cut-off between "small enough to share
// via Bytes refcounting" and "large enough to pay the cost of
// chunked file I/O".
pub(super) const STREAM_THRESHOLD_BYTES: u64 = 1024 * 1024;

// Chunk size for the streaming reader. 64 KiB is the historical
// `sendfile`-friendly default — large enough that read overhead
// amortises, small enough that one slow client can't pin tens of MB
// of RSS while a fast disk feeds it.
pub(super) const STREAM_CHUNK_BYTES: usize = 64 * 1024;

// Incremental flush threshold for streamed-static egress metering. The
// drain accrues delivered bytes locally and only bumps the meter once a
// chunk of accrued bytes reaches this size (plus a final bump on
// completion / disconnect), bounding in-memory un-recorded egress without
// touching the meter on every 64 KiB chunk. Mirrors the worker's
// `STREAM_FLUSH_BYTES`.
const STREAM_EGRESS_FLUSH_BYTES: u64 = 1024 * 1024;

/// Per-stream metering context threaded into the streamed-static drain so the
/// gateway bills `gateway_egress_bytes` for the bytes ACTUALLY delivered to
/// the client (incremental accrual + a final delta on completion/disconnect),
/// not the intended asset length recorded up front. Without this, a client
/// aborting a large-asset download is billed the whole file (finding #2 /
/// over-bill on disconnect).
///
/// `app_id` is the route's server-resolved id (never a client value). The
/// recorded metric is disjoint from the worker-owned `egress_bytes` by
/// construction — the gateway only meters bodies the worker never sees.
pub(super) struct StreamEgressMeter {
    meter: Arc<Meter>,
    app_id: Uuid,
}

impl StreamEgressMeter {
    pub(super) fn new(meter: Arc<Meter>, app_id: Uuid) -> Self {
        Self { meter, app_id }
    }

    /// Record `n` delivered bytes as `gateway_egress_bytes` for this stream's
    /// app. A no-op for `n == 0`. Cheap: the same `Meter::increment` the
    /// buffered path uses (an `RwLock` read + a per-app `Mutex` for the custom
    /// metric — uncontended, no await, no blocking I/O).
    fn record(&self, n: u64) {
        if n > 0 {
            self.meter
                .increment(&self.app_id.to_string(), "gateway_egress_bytes", n);
        }
    }
}

/// Spawn a compio task that reads `path` in `chunk_bytes`-sized chunks
/// starting at offset 0 and pushes each chunk into the returned mpsc
/// receiver. The receiver yields `Result<Bytes, Rc<dyn Error>>` so it
/// can be plumbed straight into ntex's `SizedStream` body type.
///
/// The task drops the file (and stops reading) the moment the receiver
/// goes away — a slow-client disconnect won't keep reading bytes from
/// disk indefinitely. Reads use `compio::fs::File::read_at`, which on
/// Linux maps to `IORING_OP_READ` (real positional async I/O, no
/// blocking thread pool).
///
/// Splitting the read loop into a separate function makes the test
/// surface narrow: `chunk_stream_from_file` is generic over the source
/// type, so we can substitute an in-memory `Vec<u8>` (which already
/// implements `compio_io::AsyncReadAt`) and verify chunk sizing without
/// touching the filesystem.
pub(super) fn chunk_stream_from_path(
    path: PathBuf,
    size: u64,
    chunk_bytes: usize,
    egress: Option<StreamEgressMeter>,
) -> ntex::channel::mpsc::Receiver<Result<Bytes, Rc<dyn std::error::Error>>> {
    chunk_stream_from_path_range(path, 0, size, chunk_bytes, egress)
}

/// Range-aware variant of [`chunk_stream_from_path`]. Reads `length`
/// bytes starting at `start` from the file, in `chunk_bytes`-sized
/// chunks. Used for `Range:` requests on the streaming path so we
/// only ship the requested slice.
pub(super) fn chunk_stream_from_path_range(
    path: PathBuf,
    start: u64,
    length: u64,
    chunk_bytes: usize,
    egress: Option<StreamEgressMeter>,
) -> ntex::channel::mpsc::Receiver<Result<Bytes, Rc<dyn std::error::Error>>> {
    let (tx, rx) = ntex::channel::mpsc::channel();
    compio::runtime::spawn(async move {
        let file = match compio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) => {
                let _ = tx.send(Err::<Bytes, Rc<dyn std::error::Error>>(Rc::new(e)));
                return;
            }
        };
        chunk_stream_from_file_range(&file, start, length, chunk_bytes, &tx, egress.as_ref()).await;
    })
    .detach();
    rx
}

/// Drain a chunk-by-chunk read of `source` into `tx`. Pulled out of
/// `chunk_stream_from_path` so tests can drive it with an in-memory
/// `Vec<u8>` (which implements `compio_io::AsyncReadAt`) without
/// spinning up a real file. The function exits early when:
///
/// * `tx.send` returns `Err` — the client disconnected, no point
///   reading more.
/// * The source returns 0 bytes — short read; assume EOF.
/// * The source returns an error — propagate it as the final stream
///   item, then close.
#[cfg(test)]
pub(super) async fn chunk_stream_from_file<R>(
    source: &R,
    size: u64,
    chunk_bytes: usize,
    tx: &ntex::channel::mpsc::Sender<Result<Bytes, Rc<dyn std::error::Error>>>,
) where
    R: compio::io::AsyncReadAt,
{
    chunk_stream_from_file_range(source, 0, size, chunk_bytes, tx, None).await
}

/// Range-aware variant of [`chunk_stream_from_file`]. Starts reading
/// at `start` and emits exactly `length` bytes (or fewer on a short
/// read / error). Same exit conditions as the non-range version.
pub(super) async fn chunk_stream_from_file_range<R>(
    source: &R,
    start: u64,
    length: u64,
    chunk_bytes: usize,
    tx: &ntex::channel::mpsc::Sender<Result<Bytes, Rc<dyn std::error::Error>>>,
    egress: Option<&StreamEgressMeter>,
) where
    R: compio::io::AsyncReadAt,
{
    use compio::buf::BufResult;
    let record = |n: u64| {
        if let Some(m) = egress {
            m.record(n);
        }
    };
    let mut sent: u64 = 0;
    // Delivered bytes accrued since the last meter bump. A chunk counts as
    // delivered only AFTER `tx.send` succeeds (the body writer accepted it),
    // so a client disconnect bills the bytes actually written, not the
    // intended asset length (finding #2). Flushed at `STREAM_EGRESS_FLUSH_BYTES`
    // mid-stream and once more at every exit below.
    let mut since_flush: u64 = 0;
    while sent < length {
        let want = std::cmp::min(chunk_bytes as u64, length - sent) as usize;
        let buf = vec![0u8; want];
        let BufResult(res, returned) = source.read_at(buf, start + sent).await;
        match res {
            // Short read / EOF: land the trailing delivered delta before exit.
            Ok(0) => {
                record(since_flush);
                return;
            }
            Ok(n) => {
                // Trim the buffer to what was actually read — short
                // reads are legal and we don't want to ship trailing
                // zeros to the client.
                let mut chunk = returned;
                chunk.truncate(n);
                // Bytes::from(Vec<u8>) copies in this version of
                // ntex_bytes. That's the residual user-space copy
                // A future sendfile-style path would eliminate this. The
                // win at this
                // layer is that we never hold the full file in RAM.
                if tx.send(Ok(Bytes::from(chunk))).is_err() {
                    // Client disconnected: bill only what was delivered so far.
                    record(since_flush);
                    return;
                }
                sent += n as u64;
                since_flush += n as u64;
                if since_flush >= STREAM_EGRESS_FLUSH_BYTES {
                    record(since_flush);
                    since_flush = 0;
                }
            }
            Err(e) => {
                let _ = tx.send(Err::<Bytes, Rc<dyn std::error::Error>>(Rc::new(e)));
                record(since_flush);
                return;
            }
        }
    }
    // Full delivery: land the final delta.
    record(since_flush);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drain the chunk stream into a Vec of `Bytes` via the public
    /// Stream interface (mirrors how ntex's body writer would consume
    /// it). Closes the receiver when the senders drop, so a sender
    /// task that exits cleanly terminates the loop.
    async fn drain_chunks(
        rx: ntex::channel::mpsc::Receiver<Result<Bytes, Rc<dyn std::error::Error>>>,
    ) -> Vec<Bytes> {
        let mut out = Vec::new();
        loop {
            match rx.recv().await {
                Some(Ok(b)) => out.push(b),
                Some(Err(e)) => panic!("chunk stream error: {e}"),
                None => return out,
            }
        }
    }

    #[compio::test]
    async fn chunk_reader_emits_full_chunks_then_remainder() {
        // 200 bytes of payload, 64-byte chunks → expect 64+64+64+8.
        let payload: Vec<u8> = (0..200).map(|i| i as u8).collect();
        let (tx, rx) = ntex::channel::mpsc::channel::<Result<Bytes, Rc<dyn std::error::Error>>>();
        chunk_stream_from_file(&payload, payload.len() as u64, 64, &tx).await;
        drop(tx);

        let chunks = drain_chunks(rx).await;
        assert_eq!(chunks.len(), 4, "200 bytes / 64-byte chunks → 4 chunks");
        assert_eq!(chunks[0].len(), 64);
        assert_eq!(chunks[1].len(), 64);
        assert_eq!(chunks[2].len(), 64);
        assert_eq!(chunks[3].len(), 8, "trailing partial chunk");

        // Concatenated chunks must reproduce the source byte-for-byte.
        let mut joined = Vec::with_capacity(payload.len());
        for c in &chunks {
            joined.extend_from_slice(c);
        }
        assert_eq!(joined, payload, "stream output reassembles to source");
    }

    #[compio::test]
    async fn chunk_reader_handles_size_smaller_than_chunk() {
        // Edge case: payload smaller than one chunk → single short chunk.
        let payload = vec![7u8; 100];
        let (tx, rx) = ntex::channel::mpsc::channel::<Result<Bytes, Rc<dyn std::error::Error>>>();
        chunk_stream_from_file(&payload, payload.len() as u64, 64 * 1024, &tx).await;
        drop(tx);

        let chunks = drain_chunks(rx).await;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 100);
        assert_eq!(&chunks[0][..], payload.as_slice());
    }

    #[compio::test]
    async fn chunk_stream_from_path_reads_real_file_chunk_by_chunk() {
        // End-to-end: write a file under a fresh tmpdir, stream it,
        // assert chunks come back in order and recombine to the source.
        let mut dir = std::env::temp_dir();
        dir.push(format!("zsstream-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("payload.bin");
        // 5 * 1 KiB so we get multiple chunks of 1 KiB plus a small
        // tail when chunk_bytes = 1024.
        let payload: Vec<u8> = (0..5 * 1024 + 7).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &payload).expect("write");

        let rx = chunk_stream_from_path(path.clone(), payload.len() as u64, 1024, None);
        let chunks = drain_chunks(rx).await;

        let total: usize = chunks.iter().map(|c| c.len()).sum();
        assert_eq!(total, payload.len(), "total streamed = file size");
        // Most chunks should be 1024 bytes; the tail one is whatever's
        // left. We don't assert exact chunk count because compio is
        // free to short-read on a single-syscall basis.
        assert!(chunks.len() >= 5, "got at least 5 chunks");
        let mut joined = Vec::with_capacity(payload.len());
        for c in &chunks {
            joined.extend_from_slice(c);
        }
        assert_eq!(joined, payload, "round-trip identity");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[compio::test]
    async fn chunk_stream_from_file_range_reads_offset_correctly() {
        // Verify the range-aware chunk reader skips to the offset and
        // emits exactly `length` bytes — no overshoot.
        let payload: Vec<u8> = (0..200).map(|i| i as u8).collect();
        let (tx, rx) = ntex::channel::mpsc::channel::<Result<Bytes, Rc<dyn std::error::Error>>>();
        chunk_stream_from_file_range(&payload, 50, 30, 16, &tx, None).await;
        drop(tx);

        let chunks = drain_chunks(rx).await;
        let mut joined = Vec::new();
        for c in &chunks {
            joined.extend_from_slice(c);
        }
        assert_eq!(joined.len(), 30, "exactly 30 bytes streamed");
        assert_eq!(joined, payload[50..80], "bytes match offset/length");
    }
}
