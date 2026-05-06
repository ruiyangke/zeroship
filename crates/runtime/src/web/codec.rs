//! Codec layer for `CompressionStream` / `DecompressionStream` and
//! internal fetch `Content-Encoding` decoding.
//!
//! This module owns four wire formats × two directions = eight codec
//! impls behind a single `Codec` trait. The JS-facing classes are
//! built on top of these in the native-streams sibling project; this
//! module is *only* the streaming codec engine plus a few helpers
//! (multi-coding chain builder, lenient deflate fallback) that the
//! fetch layer will plug in once it lands.
//!
//! ## Spec sources
//!
//! - WHATWG Compression: https://compression.spec.whatwg.org/
//! - RFC 1950 (zlib), RFC 1951 (raw DEFLATE), RFC 1952 (gzip), RFC 7932 (brotli)
//! - RFC 9110 §8.4 (Content-Encoding) / §8.4.1 (multi-coding ordering)
//!
//! ## Lifecycle
//!
//! ```text
//! make_codec(format, mode) → Box<dyn Codec>
//!     .write(chunk) → Ok((produced, consumed))   // any number of times
//!     .finish()    → Ok(trailer_bytes)           // exactly once on success
//!  or .cancel()                                  // anytime; idempotent
//! ```
//!
//! After `finish` returns `Ok(_)` or `cancel` is called, `finished()`
//! is `true` and subsequent `write/finish` are silent no-ops returning
//! `Ok(empty)`. `Drop` after either is safe.
//!
//! ## Trailing-byte detection (BLOCKER-1)
//!
//! flate2's writers swallow trailing data: `ZlibDecoder` / `GzDecoder`
//! return `Ok(0)` for any bytes past stream-end and `finish()` does
//! NOT detect them (proven by upstream's own
//! `decode_extra_data` test at
//! `flate2/src/zlib/write.rs:357-383`). The trait's `write` therefore
//! returns `(Vec<u8>, usize)` where `usize < chunk.len()` means the
//! call site must surface a `TrailingBytes` error. brotli's writer
//! already returns the consumed-offset directly via the `CustomWrite`
//! contract (`brotli-decompressor/src/writer.rs:337-368`), so we
//! adopt the same `(produced, consumed)` shape uniformly.
//!
//! ## Design references
//!
//! Decisions D-1 through D-15 of
//! `docs/proposals/compression-streams-native.md`. The TODO markers
//! in this file point back at design sections that depend on the
//! native-streams or native-fetch sibling projects.

use std::io::Write;

use flate2::write::{DeflateEncoder, GzDecoder, GzEncoder, ZlibEncoder};
use flate2::{Compression, Decompress, FlushDecompress, Status};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Wire format. Spec values are `gzip`, `deflate`, `deflate-raw`,
/// `brotli` (the latter added by whatwg/compression PR #80 on
/// 2026-04-02). The codec layer also accepts these from the
/// multi-coding chain builder via `parse_format` / lookup tables in
/// `build_codec_chain`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionFormat {
    Gzip,
    /// zlib-wrapped DEFLATE (RFC 1950). The strict public-API
    /// `DecompressionStream("deflate")` uses this; the fetch path's
    /// `Content-Encoding: deflate` falls back to raw on header
    /// mismatch (see `try_zlib_then_raw_decode`).
    Deflate,
    /// Raw DEFLATE (RFC 1951), no zlib wrapper.
    DeflateRaw,
    Brotli,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecMode {
    Compress,
    Decompress,
}

/// Errors raised by the codec layer. Mapped to JS errors at the call
/// site by the wrappers in `error_mapping` below — never directly by
/// the codec impls themselves.
///
/// Per design D-15: a single Rust variant set; the JS side has *two*
/// wrapper functions (`throw_input_type_error` for the always-TypeError
/// cases, `throw_decode_data_error` for the decode-data cases that
/// flip to `DOMException("DataError")` if whatwg/compression issue #51
/// lands and we toggle `DECODE_ERROR_USES_DOMEXCEPTION`).
#[derive(Debug)]
pub enum CodecError {
    /// Decompression encountered malformed input mid-stream. Maps to
    /// the spec's `decompress-and-enqueue` "throw a TypeError". Will
    /// flip to `DOMException("DataError")` if whatwg/compression #51
    /// lands and `DECODE_ERROR_USES_DOMEXCEPTION` is toggled to `true`.
    DecodeData(&'static str),

    /// Decompression: input ended before the stream-end marker. Maps
    /// to spec `decompress-flush-and-enqueue` step 3 "throw a TypeError".
    Truncated,

    /// Decompression: extra bytes after the stream-end marker. Maps
    /// to spec `decompress-and-enqueue` step 6 "throw a TypeError".
    TrailingBytes,

    /// Backend allocation / IO failure. Always surfaced as TypeError
    /// regardless of `DECODE_ERROR_USES_DOMEXCEPTION`.
    Backend(String),

    /// `build_codec_chain` was called with a coding name we don't
    /// support (per design D-9: unknown coding produces a fetch
    /// network error, not a silent passthrough). Carries the offending
    /// coding name so the error surface can repeat it back to the user.
    UnknownCoding(String),
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecError::DecodeData(s) => write!(f, "decode data error: {s}"),
            CodecError::Truncated => f.write_str("truncated input"),
            CodecError::TrailingBytes => f.write_str("trailing bytes after end of stream"),
            CodecError::Backend(s) => write!(f, "codec backend error: {s}"),
            CodecError::UnknownCoding(c) => write!(f, "unsupported Content-Encoding: {c}"),
        }
    }
}

impl std::error::Error for CodecError {}

/// The streaming codec contract. `write` returns
/// `(produced_bytes, input_bytes_consumed)`; if `consumed < chunk.len()`,
/// the codec hit stream-end with bytes left over (decompression-only
/// scenario) and the caller should surface `CodecError::TrailingBytes`.
///
/// `finish` is called exactly once on the success path; subsequent
/// calls return `Ok(empty)` (BLOCKER-2). `cancel` is idempotent.
pub trait Codec: Send {
    fn write(&mut self, chunk: &[u8]) -> Result<(Vec<u8>, usize), CodecError>;
    fn finish(&mut self) -> Result<Vec<u8>, CodecError>;
    fn cancel(&mut self);
    fn finished(&self) -> bool;
}

// ---------------------------------------------------------------------------
// Error-type policy (D-15)
// ---------------------------------------------------------------------------

/// When the WHATWG Compression spec issue #51 lands and decode-data
/// errors flip from `TypeError` to `DOMException("DataError")`, set
/// this constant to `true`. The codec layer never directly throws
/// JS exceptions; this constant is read by the JS-facing wrappers
/// (lives near the V8 class code in the sibling native-classes
/// project) when mapping `CodecError::DecodeData` and
/// `CodecError::Truncated` / `TrailingBytes`.
///
/// As of 2026-05-01 the spec still says `TypeError`, so this is `false`.
/// See https://github.com/whatwg/compression/issues/51 for the issue.
pub const DECODE_ERROR_USES_DOMEXCEPTION: bool = false;

/// Build a JS-side error message for a "wrong input type" condition
/// (e.g. SAB-backed view, non-BufferSource chunk). Always a TypeError
/// regardless of D-15. The actual `throw` lives in the V8 class layer
/// in a sibling project; this is the message + classification helper
/// the codec layer exposes today.
pub fn throw_input_type_error(msg: &str) -> ThrownError {
    ThrownError {
        kind: ThrownErrorKind::TypeError,
        message: msg.to_string(),
    }
}

/// Build a JS-side error for a `CodecError`. Per D-15, the
/// classification flips for `DecodeData` / `Truncated` / `TrailingBytes`
/// based on `DECODE_ERROR_USES_DOMEXCEPTION`.
pub fn throw_decode_data_error(err: &CodecError) -> ThrownError {
    let is_decode = matches!(
        err,
        CodecError::DecodeData(_) | CodecError::Truncated | CodecError::TrailingBytes
    );
    let kind = if is_decode && DECODE_ERROR_USES_DOMEXCEPTION {
        ThrownErrorKind::DataError
    } else {
        ThrownErrorKind::TypeError
    };
    ThrownError {
        kind,
        message: err.to_string(),
    }
}

/// Plain-Rust description of the JS error to throw. The real V8-side
/// `throw_exception` call lives next to `CompressionStream` in the
/// sibling native-classes project; this struct is the codec layer's
/// stable hand-off type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThrownError {
    pub kind: ThrownErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThrownErrorKind {
    /// Plain `TypeError`.
    TypeError,
    /// `DOMException` with name `"DataError"` — only emitted when
    /// `DECODE_ERROR_USES_DOMEXCEPTION` is `true`.
    DataError,
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Build a fresh codec for `format` × `mode`.
pub fn make_codec(format: CompressionFormat, mode: CodecMode) -> Box<dyn Codec> {
    match (format, mode) {
        (CompressionFormat::Gzip, CodecMode::Compress) => Box::new(GzipEncoder::new()),
        (CompressionFormat::Gzip, CodecMode::Decompress) => Box::new(GzipDecoder::new()),
        (CompressionFormat::Deflate, CodecMode::Compress) => Box::new(ZlibDeflateEncoder::new()),
        (CompressionFormat::Deflate, CodecMode::Decompress) => Box::new(zlib_deflate_decoder()),
        (CompressionFormat::DeflateRaw, CodecMode::Compress) => Box::new(RawDeflateEncoder::new()),
        (CompressionFormat::DeflateRaw, CodecMode::Decompress) => Box::new(raw_deflate_decoder()),
        (CompressionFormat::Brotli, CodecMode::Compress) => Box::new(BrotliEncoder::new()),
        (CompressionFormat::Brotli, CodecMode::Decompress) => Box::new(BrotliDecoder::new()),
    }
}

// ---------------------------------------------------------------------------
// flate2 helper — drain output Vec from a writer wrapping `Vec<u8>`
// ---------------------------------------------------------------------------

/// flate2's streaming writers wrap an inner `W: Write`. We use
/// `Vec<u8>` as the sink and drain it after every operation. Take
/// the bytes by mem::replace so we never re-allocate and the
/// writer's invariants stay intact.
fn drain_vec(buf: &mut Vec<u8>) -> Vec<u8> {
    std::mem::take(buf)
}

/// flate2's streaming-decoder error path returns
/// `io::Error::new(io::ErrorKind::InvalidInput, "corrupt deflate stream")`
/// for both genuine garbage AND for "stream not complete at finish".
/// We can't distinguish them by error kind alone — the reliable signal
/// is *did we ever observe stream-end* during a successful `write`.
/// Each decoder tracks `reached_end` based on a write returning `Ok(0)`
/// while input was non-empty; `finish` consults that flag to map to
/// `Truncated` vs `DecodeData`.
fn map_flate_io_err(err: std::io::Error) -> CodecError {
    // Any io error from a decoder is corrupt input. (flate2 normalises
    // its own backend error to ErrorKind::InvalidInput.)
    CodecError::DecodeData("corrupt deflate stream")
        .with_io(err)
}

impl CodecError {
    fn with_io(self, _err: std::io::Error) -> Self {
        // The IO error always carries the same hard-coded message in
        // flate2 ("corrupt deflate stream"); preserving it in the
        // CodecError isn't useful, so we drop it. Kept as a method
        // for future expansion if a richer error API is wanted.
        self
    }
}

// ---------------------------------------------------------------------------
// Gzip encoder
// ---------------------------------------------------------------------------

struct GzipEncoder {
    inner: Option<GzEncoder<Vec<u8>>>,
    finished: bool,
}

impl GzipEncoder {
    fn new() -> Self {
        // Default Compression matches flate2's level 6 — same as
        // gzip(1)'s default and matches what every browser ships.
        Self {
            inner: Some(GzEncoder::new(Vec::new(), Compression::default())),
            finished: false,
        }
    }
}

impl Codec for GzipEncoder {
    fn write(&mut self, chunk: &[u8]) -> Result<(Vec<u8>, usize), CodecError> {
        if self.finished {
            return Ok((Vec::new(), 0));
        }
        let inner = self.inner.as_mut().expect("inner present until finish");
        inner
            .write_all(chunk)
            .map_err(|e| CodecError::Backend(e.to_string()))?;
        let produced = drain_vec(inner.get_mut());
        Ok((produced, chunk.len()))
    }

    fn finish(&mut self) -> Result<Vec<u8>, CodecError> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        let inner = self.inner.take().expect("inner present until finish");
        let trailer = inner
            .finish()
            .map_err(|e| CodecError::Backend(e.to_string()))?;
        Ok(trailer)
    }

    fn cancel(&mut self) {
        self.finished = true;
        // Drop the inner writer; flate2 internally calls `finish` from
        // its Drop impl but errors are swallowed. No leakable state.
        self.inner = None;
    }

    fn finished(&self) -> bool {
        self.finished
    }
}

// ---------------------------------------------------------------------------
// Gzip decoder (single-member; spec rejects multi-member per whatwg/compression#42)
// ---------------------------------------------------------------------------

struct GzipDecoder {
    inner: Option<GzDecoder<Vec<u8>>>,
    /// Tracks whether stream-end was observed during a `write` call.
    /// flate2's decoder writes go through `write_with_status`, which
    /// returns Ok(0) once the gzip CRC+ISIZE trailer is parsed. We use
    /// a Ok(0)-returning write (with non-empty input) as the proxy for
    /// "saw stream end."
    reached_end: bool,
    finished: bool,
}

impl GzipDecoder {
    fn new() -> Self {
        Self {
            inner: Some(GzDecoder::new(Vec::new())),
            reached_end: false,
            finished: false,
        }
    }
}

impl Codec for GzipDecoder {
    fn write(&mut self, chunk: &[u8]) -> Result<(Vec<u8>, usize), CodecError> {
        if self.finished || chunk.is_empty() {
            return Ok((Vec::new(), 0));
        }
        let inner = self.inner.as_mut().expect("inner present until finish");
        let mut consumed = 0usize;
        while consumed < chunk.len() {
            match inner.write(&chunk[consumed..]) {
                Ok(0) => {
                    // Stream-end reached; remaining bytes are post-stream.
                    self.reached_end = true;
                    break;
                }
                Ok(n) => consumed += n,
                Err(e) => return Err(map_flate_io_err(e)),
            }
        }
        let produced = drain_vec(inner.get_mut());
        Ok((produced, consumed))
    }

    fn finish(&mut self) -> Result<Vec<u8>, CodecError> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        let mut inner = self.inner.take().expect("inner present until finish");
        // miniz_oxide's `inflate(.., Finish)` returns Err(MZError::Buf)
        // when the stream-end marker was never reached and no more
        // input is available. flate2's `Writer::finish` propagates that
        // as an io error — so `try_finish() == Err` reliably means
        // "truncated input" for our case (we never feed empty input
        // through `write` after the loop, so any other error path
        // would be backend allocation, which we treat the same).
        let res = inner.try_finish();
        let trailer = drain_vec(inner.get_mut());
        match res {
            Ok(()) => Ok(trailer),
            Err(_) => Err(CodecError::Truncated),
        }
    }

    fn cancel(&mut self) {
        self.finished = true;
        self.inner = None;
    }

    fn finished(&self) -> bool {
        self.finished
    }
}

// ---------------------------------------------------------------------------
// Zlib-wrapped deflate (RFC 1950) encoder + decoder
// ---------------------------------------------------------------------------

struct ZlibDeflateEncoder {
    inner: Option<ZlibEncoder<Vec<u8>>>,
    finished: bool,
}

impl ZlibDeflateEncoder {
    fn new() -> Self {
        Self {
            inner: Some(ZlibEncoder::new(Vec::new(), Compression::default())),
            finished: false,
        }
    }
}

impl Codec for ZlibDeflateEncoder {
    fn write(&mut self, chunk: &[u8]) -> Result<(Vec<u8>, usize), CodecError> {
        if self.finished {
            return Ok((Vec::new(), 0));
        }
        let inner = self.inner.as_mut().expect("inner present");
        inner
            .write_all(chunk)
            .map_err(|e| CodecError::Backend(e.to_string()))?;
        let produced = drain_vec(inner.get_mut());
        Ok((produced, chunk.len()))
    }

    fn finish(&mut self) -> Result<Vec<u8>, CodecError> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        let inner = self.inner.take().expect("inner present");
        inner
            .finish()
            .map_err(|e| CodecError::Backend(e.to_string()))
    }

    fn cancel(&mut self) {
        self.finished = true;
        self.inner = None;
    }

    fn finished(&self) -> bool {
        self.finished
    }
}

/// Decompressor for both zlib-wrapped (RFC 1950) and raw DEFLATE
/// (RFC 1951). Constructed with the `zlib_header` flag passed straight
/// through to `flate2::Decompress::new`. We use the lower-level
/// `Decompress` API rather than the `ZlibDecoder` writer because the
/// writer's `try_finish` returns `Ok(())` for both clean-completion and
/// truncated-input (flate2 maps miniz_oxide's `MZError::Buf` to
/// `Ok(Status::BufError)`, swallowing the truncation signal). With
/// `Decompress::decompress_vec` the caller sees `Status::StreamEnd`
/// directly, which is the only reliable end-of-stream marker we can
/// observe without re-implementing the format.
struct InflateDecoder {
    /// `None` after `cancel`/`finish` to ensure idempotency.
    state: Option<Decompress>,
    /// Set once `Status::StreamEnd` is observed during any `write`.
    /// `finish` consults this — true → Ok, false → Truncated.
    reached_end: bool,
    finished: bool,
}

impl InflateDecoder {
    fn new(zlib_header: bool) -> Self {
        Self {
            state: Some(Decompress::new(zlib_header)),
            reached_end: false,
            finished: false,
        }
    }
}

impl Codec for InflateDecoder {
    fn write(&mut self, chunk: &[u8]) -> Result<(Vec<u8>, usize), CodecError> {
        if self.finished || chunk.is_empty() {
            return Ok((Vec::new(), 0));
        }
        // If we already saw stream-end, any further bytes are trailing
        // data — report zero consumption so the call site surfaces
        // TrailingBytes (per design D-3 / WHATWG decompress-and-enqueue
        // step 6).
        if self.reached_end {
            return Ok((Vec::new(), 0));
        }
        let state = self.state.as_mut().expect("state present");

        // Pre-size output: typical compression ratios for text are 2-5x.
        // We expand below as the codec writes into the Vec's spare
        // capacity (decompress_vec only writes into spare; it does NOT
        // reallocate). The loop continues feeding the decoder — even
        // after input is fully consumed — until the decoder emits no
        // more output (its internal buffer drains into our Vec).
        let mut produced = Vec::with_capacity(chunk.len() * 2);

        let start_in = state.total_in();
        loop {
            let consumed = (state.total_in() - start_in) as usize;
            // `consumed >= chunk.len()` is NOT a termination condition
            // on its own — the decoder may still hold buffered output.
            // Feed it empty input until output also stops growing, or
            // we observe StreamEnd.
            let remaining: &[u8] = if consumed >= chunk.len() {
                &[]
            } else {
                &chunk[consumed..]
            };
            if produced.capacity() == produced.len() {
                let want = (produced.capacity() * 2).max(chunk.len() * 4).max(4096);
                produced.reserve(want.saturating_sub(produced.len()));
            }
            let before_out = state.total_out();
            let before_in = state.total_in();
            let status = state
                .decompress_vec(remaining, &mut produced, FlushDecompress::None)
                .map_err(|_e| CodecError::DecodeData("corrupt deflate stream"))?;
            let made_progress = state.total_out() > before_out || state.total_in() > before_in;
            match status {
                Status::StreamEnd => {
                    self.reached_end = true;
                    break;
                }
                Status::Ok => {
                    if !made_progress {
                        // No-op iteration on empty input — done draining.
                        break;
                    }
                    // else: keep going (more output may still be buffered).
                }
                Status::BufError => {
                    // Output buffer was full mid-block — grow + retry.
                    if !made_progress {
                        let want = (produced.capacity() * 2).max(chunk.len() * 4).max(4096);
                        produced.reserve(want.saturating_sub(produced.len()));
                        if produced.capacity() == produced.len() {
                            break; // allocator refused — defensive bail.
                        }
                        continue;
                    }
                    // Made progress; the next iteration will reassess.
                }
            }
        }
        let consumed = (state.total_in() - start_in) as usize;
        Ok((produced, consumed))
    }

    fn finish(&mut self) -> Result<Vec<u8>, CodecError> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        let mut state = self.state.take().expect("state present");
        // For a clean stream, `reached_end` was set during write — no
        // more work to do beyond an empty trailer. For a stream that
        // never reached end (truncated, or empty input), running
        // decompress(.., Finish) with empty input returns
        // `Ok(Status::BufError)` on miniz_oxide (the truncation signal
        // is "no progress" rather than an error). We surface that as
        // `CodecError::Truncated`.
        let mut tail = Vec::new();
        if !self.reached_end {
            // Try one final decompress-with-finish in case the decoder
            // is sitting at stream-end having just consumed the last
            // byte (rare — Status::StreamEnd should have been seen
            // already, but Decompress::decompress_vec sometimes returns
            // Ok(Status::Ok) at the boundary).
            tail.reserve(64);
            let status = state
                .decompress_vec(&[], &mut tail, FlushDecompress::Finish)
                .map_err(|_e| CodecError::DecodeData("corrupt deflate stream"))?;
            if status == Status::StreamEnd {
                self.reached_end = true;
            }
        }
        if self.reached_end {
            Ok(tail)
        } else {
            Err(CodecError::Truncated)
        }
    }

    fn cancel(&mut self) {
        self.finished = true;
        self.state = None;
    }

    fn finished(&self) -> bool {
        self.finished
    }
}

/// Constructor alias — `Decompress` configured for zlib-wrapped input.
fn zlib_deflate_decoder() -> InflateDecoder {
    InflateDecoder::new(/*zlib_header=*/ true)
}

// ---------------------------------------------------------------------------
// Raw DEFLATE (RFC 1951) encoder + decoder
// ---------------------------------------------------------------------------

struct RawDeflateEncoder {
    inner: Option<DeflateEncoder<Vec<u8>>>,
    finished: bool,
}

impl RawDeflateEncoder {
    fn new() -> Self {
        Self {
            inner: Some(DeflateEncoder::new(Vec::new(), Compression::default())),
            finished: false,
        }
    }
}

impl Codec for RawDeflateEncoder {
    fn write(&mut self, chunk: &[u8]) -> Result<(Vec<u8>, usize), CodecError> {
        if self.finished {
            return Ok((Vec::new(), 0));
        }
        let inner = self.inner.as_mut().expect("inner present");
        inner
            .write_all(chunk)
            .map_err(|e| CodecError::Backend(e.to_string()))?;
        let produced = drain_vec(inner.get_mut());
        Ok((produced, chunk.len()))
    }

    fn finish(&mut self) -> Result<Vec<u8>, CodecError> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        let inner = self.inner.take().expect("inner present");
        inner
            .finish()
            .map_err(|e| CodecError::Backend(e.to_string()))
    }

    fn cancel(&mut self) {
        self.finished = true;
        self.inner = None;
    }

    fn finished(&self) -> bool {
        self.finished
    }
}

// Raw DEFLATE (RFC 1951) decoder shares the same state machine as
// zlib — only the header flag differs. See `InflateDecoder` above.
fn raw_deflate_decoder() -> InflateDecoder {
    InflateDecoder::new(/*zlib_header=*/ false)
}

// ---------------------------------------------------------------------------
// Brotli encoder + decoder
// ---------------------------------------------------------------------------

/// Brotli encoder. Quality=4, lgwin=22 per design D-12 — trades ~2%
/// ratio for ~3x throughput vs quality=6, while still beating gzip's
/// default quality. Buffer size 4096 is the rust-brotli example
/// default; larger buffers don't help at our typical chunk sizes.
struct BrotliEncoder {
    inner: Option<brotli::CompressorWriter<Vec<u8>>>,
    finished: bool,
}

impl BrotliEncoder {
    fn new() -> Self {
        Self {
            inner: Some(brotli::CompressorWriter::new(Vec::new(), 4096, 4, 22)),
            finished: false,
        }
    }
}

impl Codec for BrotliEncoder {
    fn write(&mut self, chunk: &[u8]) -> Result<(Vec<u8>, usize), CodecError> {
        if self.finished {
            return Ok((Vec::new(), 0));
        }
        let inner = self.inner.as_mut().expect("inner present");
        inner
            .write_all(chunk)
            .map_err(|e| CodecError::Backend(e.to_string()))?;
        let produced = drain_vec(inner.get_mut());
        Ok((produced, chunk.len()))
    }

    fn finish(&mut self) -> Result<Vec<u8>, CodecError> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        let inner = self.inner.take().expect("inner present");
        // `CompressorWriter::flush()` calls BROTLI_OPERATION_FLUSH,
        // which does NOT emit the stream-end marker — a decoder fed
        // those bytes would never see ResultSuccess. We need
        // BROTLI_OPERATION_FINISH, which is reachable only via
        // `into_inner()` (or Drop, but Drop swallows errors). See
        // brotli/src/enc/writer.rs:242-248.
        let final_buf = inner.into_inner();
        Ok(final_buf)
    }

    fn cancel(&mut self) {
        self.finished = true;
        self.inner = None;
    }

    fn finished(&self) -> bool {
        self.finished
    }
}

/// Brotli decoder. The underlying `DecompressorWriter::write` returns
/// the consumed-offset directly (per
/// `brotli-decompressor/src/writer.rs:337-368`), so the trailing-byte
/// detection naturally surfaces as `consumed < chunk.len()`.
struct BrotliDecoder {
    inner: Option<brotli::DecompressorWriter<Vec<u8>>>,
    reached_end: bool,
    finished: bool,
}

impl BrotliDecoder {
    fn new() -> Self {
        Self {
            inner: Some(brotli::DecompressorWriter::new(Vec::new(), 4096)),
            finished: false,
            reached_end: false,
        }
    }
}

impl Codec for BrotliDecoder {
    fn write(&mut self, chunk: &[u8]) -> Result<(Vec<u8>, usize), CodecError> {
        if self.finished || chunk.is_empty() {
            return Ok((Vec::new(), 0));
        }
        let inner = self.inner.as_mut().expect("inner present");
        // `DecompressorWriter::write(&[u8])` returns `Ok(consumed)`
        // where consumed <= input.len(). Once the brotli stream-end
        // marker is reached, subsequent writes return Ok(0) — we use
        // that as our reached_end signal.
        let consumed = match inner.write(chunk) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::WriteZero => {
                // brotli writes Err(WriteZero) when it has consumed
                // the entire stream and you push more bytes — this is
                // the trailing-byte case in disguise. Treat it as
                // "we saw end and consumed nothing more."
                self.reached_end = true;
                0
            }
            Err(e) => return Err(CodecError::DecodeData("brotli decode failed").with_io(e)),
        };
        if consumed == 0 && !chunk.is_empty() {
            // Either trailing data or no progress; brotli reports both
            // the same way. Mark end-reached.
            self.reached_end = true;
        }
        let produced = drain_vec(inner.get_mut());
        Ok((produced, consumed))
    }

    fn finish(&mut self) -> Result<Vec<u8>, CodecError> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        let mut inner = self.inner.take().expect("inner present");
        // `close()` returns:
        //   - Ok(()) if the stream completed (ResultSuccess) — even if
        //     the user never fed any bytes? Actually no: for empty
        //     input, brotli decompress returns NeedsMoreInput, and
        //     close maps that to Err. So `close() == Err` reliably
        //     means truncation here.
        //   - Err(InvalidData) on NeedsMoreInput (truncated) or
        //     ResultFailure (corrupt).
        let close_res = inner.close();
        let trailer = drain_vec(inner.get_mut());
        match close_res {
            Ok(()) => Ok(trailer),
            Err(_) => Err(CodecError::Truncated),
        }
    }

    fn cancel(&mut self) {
        self.finished = true;
        self.inner = None;
    }

    fn finished(&self) -> bool {
        self.finished
    }
}

// ---------------------------------------------------------------------------
// Multi-coding chain helper (RFC 9110 §8.4.1, design MAJOR-6)
// ---------------------------------------------------------------------------

/// Build a chain of decoders for a `Content-Encoding` header value.
///
/// Inputs are the codings in *application order* — exactly as the
/// server listed them in the header. RFC 9110 §8.4.1 says decoders
/// MUST run in the reverse order, so the returned `Vec` is reversed:
/// element 0 is the first decoder to run on the wire bytes, and the
/// chain is iterated in order.
///
/// `decode = false` is reserved for symmetry with the encode path
/// (currently unused; the public class doesn't accept multi-coding,
/// only single-coding constructions). Callers passing `decode=false`
/// get the same chain in *application order* (no reverse).
///
/// Recognised coding names (case-insensitive): `gzip`, `x-gzip`,
/// `deflate`, `deflate-raw`, `br`, `identity`. Anything else returns
/// `Err(CodecError::UnknownCoding)` per design D-9 (server-sent
/// unknown coding is a network error, never silent passthrough).
///
/// `identity` is a no-op pass-through codec — see `IdentityCodec`
/// below. Skipped at chain-construction time: an `identity` coding
/// produces no entries in the output.
pub fn build_codec_chain(
    codings: &[&str],
    decode: bool,
) -> Result<Vec<Box<dyn Codec>>, CodecError> {
    let mut entries: Vec<Box<dyn Codec>> = Vec::with_capacity(codings.len());
    for coding in codings {
        let lc = coding.to_ascii_lowercase();
        let format = match lc.as_str() {
            "gzip" | "x-gzip" => Some(CompressionFormat::Gzip),
            "deflate" => Some(CompressionFormat::Deflate),
            "deflate-raw" => Some(CompressionFormat::DeflateRaw),
            "br" => Some(CompressionFormat::Brotli),
            "identity" => None,
            _ => return Err(CodecError::UnknownCoding(coding.to_string())),
        };
        let mode = if decode {
            CodecMode::Decompress
        } else {
            CodecMode::Compress
        };
        match format {
            Some(f) => entries.push(make_codec(f, mode)),
            None => entries.push(Box::new(IdentityCodec::new())),
        }
    }
    if decode {
        // Per RFC 9110 §8.4.1, the codings are listed in application
        // order; to undo them we reverse.
        entries.reverse();
    }
    Ok(entries)
}

/// `identity` coding — bytes pass through unchanged. Defined here so
/// the chain builder can keep a uniform `Box<dyn Codec>` slot type.
struct IdentityCodec {
    finished: bool,
}

impl IdentityCodec {
    fn new() -> Self {
        Self { finished: false }
    }
}

impl Codec for IdentityCodec {
    fn write(&mut self, chunk: &[u8]) -> Result<(Vec<u8>, usize), CodecError> {
        if self.finished {
            return Ok((Vec::new(), 0));
        }
        Ok((chunk.to_vec(), chunk.len()))
    }

    fn finish(&mut self) -> Result<Vec<u8>, CodecError> {
        self.finished = true;
        Ok(Vec::new())
    }

    fn cancel(&mut self) {
        self.finished = true;
    }

    fn finished(&self) -> bool {
        self.finished
    }
}

// ---------------------------------------------------------------------------
// Lenient deflate fallback (design MAJOR-14, fetch path only)
// ---------------------------------------------------------------------------

/// Decode a `Content-Encoding: deflate` body that may be either
/// zlib-wrapped (RFC 1950) or raw DEFLATE (RFC 1951). Per design D-6,
/// this is the *fetch path* policy — public
/// `DecompressionStream("deflate")` stays strict zlib.
///
/// Strategy: try strict zlib first; on failure, try raw DEFLATE.
/// Most real-world `Content-Encoding: deflate` bodies are zlib-wrapped
/// (per HTTP/1.1 RFC 2616 history), but ~20% of servers send raw
/// DEFLATE, which is what the spec originally meant. Chrome and Firefox
/// both fall back to raw on header mismatch. See
/// `net/filter/gzip_source_stream.cc` (`ZlibInflate::Init`) and
/// https://zlib.net/zlib_faq.html#faq39 for the historical accident.
///
/// This is a one-shot whole-buffer decoder for the v1 fetch hook; a
/// streaming probe-the-first-bytes-then-pick variant is a future
/// optimisation. Returns the decompressed bytes or an error.
///
/// Currently `dead_code` because the fetch integration that calls
/// this lives in the sibling native-fetch project — gating the
/// warning so the trunk stays warning-free until that lands.
#[allow(dead_code)]
pub(crate) fn try_zlib_then_raw_decode(input: &[u8]) -> Result<Vec<u8>, CodecError> {
    // Strict zlib first. On any error, fall back to raw.
    let mut zlib_dec = make_codec(CompressionFormat::Deflate, CodecMode::Decompress);
    let zlib_out = (|| -> Result<Vec<u8>, CodecError> {
        let (mut bytes, consumed) = zlib_dec.write(input)?;
        if consumed != input.len() {
            return Err(CodecError::TrailingBytes);
        }
        bytes.extend(zlib_dec.finish()?);
        Ok(bytes)
    })();
    if let Ok(out) = zlib_out {
        return Ok(out);
    }

    let mut raw_dec = make_codec(CompressionFormat::DeflateRaw, CodecMode::Decompress);
    let raw_out = (|| -> Result<Vec<u8>, CodecError> {
        let (mut bytes, consumed) = raw_dec.write(input)?;
        if consumed != input.len() {
            return Err(CodecError::TrailingBytes);
        }
        bytes.extend(raw_dec.finish()?);
        Ok(bytes)
    })();
    raw_out.map_err(|_| CodecError::DecodeData("neither zlib nor raw DEFLATE could decode the body"))
}

// ---------------------------------------------------------------------------
// Tests for `pub(crate)` helpers — `try_zlib_then_raw_decode` is not
// callable from the integration-test file because of the visibility
// restriction. The integration tests in `tests/codec.rs` cover the
// public surface (Codec trait, make_codec, build_codec_chain).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod internal_tests {
    use super::*;

    const SAMPLE: &[u8] = b"Lenient deflate fallback test payload, repeated. \
        Lenient deflate fallback test payload, repeated. \
        Lenient deflate fallback test payload, repeated.";

    #[test]
    fn try_zlib_then_raw_accepts_zlib_wrapped() {
        let mut enc = make_codec(CompressionFormat::Deflate, CodecMode::Compress);
        let (mut bytes, _) = enc.write(SAMPLE).unwrap();
        bytes.extend(enc.finish().unwrap());
        let decoded = try_zlib_then_raw_decode(&bytes).expect("zlib path must accept zlib bytes");
        assert_eq!(decoded, SAMPLE);
    }

    #[test]
    fn try_zlib_then_raw_falls_back_to_raw_deflate() {
        let mut enc = make_codec(CompressionFormat::DeflateRaw, CodecMode::Compress);
        let (mut bytes, _) = enc.write(SAMPLE).unwrap();
        bytes.extend(enc.finish().unwrap());
        let decoded = try_zlib_then_raw_decode(&bytes).expect("raw fallback must succeed");
        assert_eq!(decoded, SAMPLE);
    }

    #[test]
    fn try_zlib_then_raw_errors_on_garbage() {
        let garbage = b"not compressed data at all just plaintext words here";
        let res = try_zlib_then_raw_decode(garbage);
        assert!(res.is_err(), "garbage must not decode as either flavour");
    }
}
