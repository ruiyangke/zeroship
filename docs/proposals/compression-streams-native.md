# Native `CompressionStream` / `DecompressionStream` design

**Date:** 2026-05-01
**Status:** Draft — round 2 (architectural pivot to pure native)
**Spec:** WHATWG Compression Standard — https://compression.spec.whatwg.org/
**Depends on:** native-streams proposal (TransformStream / ReadableStream / WritableStream as Rust classes); native-fetch proposal (response-body construction in Rust)

## Revision history

- **r1 (2026-05-01)** — wrapped polyfill `TransformStream` inside a `#[v8_class]`. Critic round 1 surfaced five blockers, ten majors, ten missing concepts. Two of the blockers (V8 finalization ordering between two Globals; `pipeThrough` on a body that the fetch body-bridge has already locked) are *architectural* — they exist only because the codec lives in a Rust class while its host TransformStream lives in JS, and because fetch body extraction routes through a bridge that locks the native ReadableStream before user code can attach a transform.
- **r2 (2026-05-01) — pivot to pure native.** Native `TransformStream` / `ReadableStream` / `WritableStream` are being landed as a sibling project; this design now assumes they exist (see "Dependencies" below) and `CompressionStream` / `DecompressionStream` are themselves native Rust classes that *construct* a native TransformStream with a codec-backed transformer slot. Internal `Content-Encoding` decompression in fetch is wired during native response-body construction with direct codec hand-off — it does not call public `pipeThrough`. Both blockers disappear under this architecture.

## Scope

Two interrelated bodies of work that share a single native `Codec` layer:

1. **`CompressionStream` / `DecompressionStream`** — the WHATWG public
   API. Pure-native Rust classes that internally build a native
   TransformStream whose transformer's `transform`/`flush`/`cancel`
   slots run codec code directly, with no JS hop.
2. **Internal `Content-Encoding` decompression in `fetch`** — when a
   response arrives with `Content-Encoding: gzip|deflate|deflate-raw|br`,
   the native fetch layer attaches a codec adapter to the body source
   *before* the user-visible `Response.body` ReadableStream is exposed.
   Today our fetch passes encoded bytes through unchanged; user code
   that calls `.text()` / `.json()` gets garbage on real-world API
   responses.

Both consume the same Rust-side `Codec` trait. Shipping them together is
materially cheaper than shipping them separately because the codec layer
is the bulk of the work.

### IDL surface

```webidl
enum CompressionFormat {
  "brotli",
  "deflate",
  "deflate-raw",
  "gzip",
};

[Exposed=*]
interface CompressionStream {
  constructor(CompressionFormat format);
};
CompressionStream includes GenericTransformStream;

[Exposed=*]
interface DecompressionStream {
  constructor(CompressionFormat format);
};
DecompressionStream includes GenericTransformStream;

interface mixin GenericTransformStream {
  readonly attribute ReadableStream readable;
  readonly attribute WritableStream writable;
};
```

`brotli` was added to the spec on 2026-04-02
(https://github.com/whatwg/compression/pull/80). All four formats ship
in v1. Per the spec's WebIDL, neither buffer-source argument uses
`[AllowShared]`, so `SharedArrayBuffer`-backed views must reject at the
IDL boundary (see Decision D-5 below).

## Decisions

Settled choices, in priority order. Each is followed by a one-line
rationale and a deeper section reference.

| # | Decision | Rationale | Section |
|---|---|---|---|
| **D-1** | Pure native: `CompressionStream` / `DecompressionStream` are Rust `#[v8_class]` types that construct a native TransformStream with a codec-backed transformer. No JS polyfill wrap. | Eliminates two-Global GC ordering and locked-stream `pipeThrough` problems by construction. | Architecture §A,§E |
| **D-2** | Codec lifecycle: `create → write* → finish-or-cancel`. `finish` consumes; `cancel` is idempotent; second `finish` after cancel/finish is a no-op. | Required for spec-correct fetch integration (cancel) + safe Drop (idempotency). | Architecture §B |
| **D-3** | Codec output of `write` is `(Vec<u8>, usize)` — bytes produced and bytes consumed from input. Decompressors fail the stream when `consumed < input.len()` after `write` returns and the input side has signalled close. Mirrors WHATWG `decompress-and-enqueue` step 6. | flate2 returns `Ok(0)` for trailing bytes; `finish()` does not detect them. Verified at `/tmp/flate2-rs/src/zlib/write.rs:357-383` and `/tmp/flate2-rs/src/gz/write.rs:614-640`. | Architecture §B |
| **D-4** | Codec is a struct field of the native TransformStream's transformer object, owned by Rust. No HashMap, no `u32` handle, no per-isolate handle table. GC of the TransformStream drops the transformer, which drops the codec via Rust `Drop`. | Eliminates handle-exhaustion (MAJOR-13), eliminates double-free risk, mirrors workerd's `IoOwn` request-scoped ownership. | Architecture §C |
| **D-5** | `SharedArrayBuffer`-backed views are rejected with `TypeError` at the IDL boundary inside the macro-emitted argument extraction, before any codec byte is touched. The codec layer never sees SAB. | Compression spec doesn't use `[AllowShared]`; per WebIDL §3.2.21 the binding rejects. WPT `compression-bad-chunks.any.js` verifies. | Architecture §F |
| **D-6** | Public `DecompressionStream("deflate")` is **strict** zlib-wrapped (RFC 1950). Internal-fetch `Content-Encoding: deflate` is **lenient**: try zlib first, fall back to raw DEFLATE on header parse failure. | Public API matches WPT; internal fetch matches Chrome/Firefox real-world behavior (~20% of `Content-Encoding: deflate` is raw). | Architecture §D, §G |
| **D-7** | Multi-coding `Content-Encoding` (e.g. `gzip, br`) decodes in **reverse** order — un-br first, then un-gzip. RFC 9110 §8.4.1: codings listed in application order. | Single-coding-only would silently break consumers when a server flips to `gzip, br`. Build a chain of codec adapters during response-body construction. | Architecture §D |
| **D-8** | After transparent decoding, native fetch strips both `Content-Encoding` and `Content-Length` from the user-visible `Response.headers`. | Matches undici (`lib/web/fetch/index.js` `handleResponseBody`) and workerd (`global-scope.c++`). User code reading `Content-Length` to size buffers gets the *decoded* size or no header. | Architecture §D |
| **D-9** | `identity` is treated as "no transformation" (no-op, no error). Unknown codings produce a fetch network error (the response promise rejects). Matches Chrome/curl behavior. | RFC 9110 §16.6: server cannot legally send a coding we didn't accept; if it does, the body is unreadable. Better to error loudly than feed garbage to `.json()`. | Architecture §D |
| **D-10** | `Accept-Encoding: gzip, deflate, br` is sent on every outbound fetch unless user code provides an explicit `Accept-Encoding` header (including the empty string to opt out). | Without this, the test plan's gzip endpoint silently sends plain JSON and the test passes for the wrong reason. Wired in native fetch request-prep. | Architecture §D |
| **D-11** | Codec backend: `flate2 = "1"` with default `miniz_oxide` for v1; revisit `zlib-rs` (pure-Rust port of `zlib-ng`) when it stabilises. `brotli = "8"` (Dropbox port) with `default-features = false` plus the audited feature list. | At 200K req/s the 30% throughput delta of `zlib-ng` matters, but `zlib-rs` is still pre-1.0. Track the upgrade as a follow-up; not a v1 blocker. | Architecture §B / refs |
| **D-12** | Brotli encoder defaults: `quality=4`, `lgwin=22`. Trades ~2% ratio for ~3x throughput vs `quality=6`. Only affects encoder output (decoders accept any quality). | At 200K req/s outbound encoding load, CPU dominates. Quality=4 still beats gzip default. | Architecture §B |
| **D-13** | Per-isolate concurrent-decoder cap of 1024 (32 MB worst-case at 32 KB per gzip context, 8 GB worst-case at 8 MB per brotli context). Excess constructions throw `TypeError("too many concurrent codecs")`. | Brotli decoder context = 8 MB worst case; without a cap, 20K concurrent × 8 MB = 160 GB RSS. The number is conservative; tunable via `ZEROSHIP_MAX_CONCURRENT_CODECS` env. | Missing-3 |
| **D-14** | Per-decoder max-output cap of 100 MB by default for internal-fetch decompression; user-constructed `DecompressionStream` is uncapped (user controls input size). | Zip-bomb defense for transparent decoding. workerd uses a similar `maxOutputBytes` knob. | Missing-9 |
| **D-15** | Error type: a single Rust `CodecError` variant set, but the JS-facing surface has *two* wrapper functions — `throw_input_type_error` (always `TypeError`) and `throw_decode_data_error` (currently `TypeError`, switchable to `DOMException("DataError")` via a single `DECODE_ERROR_USES_DOMEXCEPTION` constant when whatwg/compression#51 lands). | Tracks the open spec issue without bleeding it into the codec layer. | Architecture §B |

## Dependencies

This design is one node in a three-design dependency graph. The other two
are sibling projects that must land first or alongside.

### From the native-streams project (sibling proposal, must land first)

This compression design reads, but does not own, the following surface:

```rust
// crates/runtime/src/web/streams.rs — sibling project
pub struct TransformStream { /* opaque */ }

impl TransformStream {
    /// Construct from a Rust transformer. The transformer's start/transform/
    /// flush/cancel run as Rust closures; no JS callback hop. The internal
    /// queues, controllers, and backpressure follow the WHATWG Streams spec.
    pub fn new_native<T: Transformer + 'static>(
        scope: &mut v8::PinScope,
        transformer: T,
        writable_strategy: QueuingStrategy,
        readable_strategy: QueuingStrategy,
    ) -> v8::Local<'_, v8::Object>;

    /// Read the [[readable]] internal slot. NOT the public `.readable`
    /// accessor — slot read, no JS-side property lookup. Required for
    /// GenericTransformStream getter spec compliance (MINOR-20).
    pub fn readable_slot<'s>(scope: &mut v8::PinScope<'s, '_>, ts: v8::Local<v8::Object>)
        -> v8::Local<'s, v8::Value>;
    pub fn writable_slot<'s>(scope: &mut v8::PinScope<'s, '_>, ts: v8::Local<v8::Object>)
        -> v8::Local<'s, v8::Value>;
}

pub trait Transformer: Send {
    /// Called per chunk. Use `controller.enqueue` to push output;
    /// inspect `controller.desired_size()` for backpressure.
    fn transform(
        &mut self,
        chunk: &[u8],
        controller: &mut TransformStreamController,
    ) -> Result<(), JsError>;

    /// Called when the writable side closes cleanly.
    fn flush(&mut self, controller: &mut TransformStreamController) -> Result<(), JsError>;

    /// Called when either side is cancelled. Must release resources.
    fn cancel(&mut self, _reason: v8::Local<v8::Value>) {}
}

pub struct TransformStreamController { /* opaque */ }

impl TransformStreamController {
    pub fn enqueue(&mut self, scope: &mut v8::PinScope, bytes: &[u8]) -> Result<(), JsError>;
    pub fn error(&mut self, scope: &mut v8::PinScope, exc: v8::Local<v8::Value>);
    pub fn terminate(&mut self);
    pub fn desired_size(&self) -> Option<f64>;  // None == errored; -inf..1 typical range
}

pub struct QueuingStrategy { pub high_water_mark: f64, pub size: SizeFn }
```

The native-streams project commits to:

1. Calling `Transformer::transform` from a borrow-checker-friendly stack
   so the codec can be a `&mut self` field of the transformer struct.
2. Calling `Transformer::cancel` exactly once, before drop, when either
   the readable or writable side errors or is cancelled.
3. Honouring `controller.desired_size() <= 0` as a soft signal — the
   stream still accepts enqueues; it just stops pulling new chunks
   from the writable side until size > 0. Backpressure is automatic.
4. Surfacing `[[readable]]` / `[[writable]]` as internal slots
   (V8 private symbols), readable from Rust without going through
   public accessors. (Per Streams §GenericTransformStream — see MINOR-20.)
5. Constructing the wrapper object with internal-field count ≥ 2 so we
   can store the codec-backed transformer pointer next to the standard
   transform-stream slots.

If any of those don't materialise, this design needs revisits in:
- §C (transformer plumbing, if (1) shifts)
- §B cancel section (if (2) shifts)
- Missing-1 backpressure (if (3) shifts)
- §E getters (if (4) shifts — fall back to public accessor with an explicit "spec-divergent" note)

### From the native-fetch project (sibling proposal, must land first)

This compression design needs an explicit hook in native-fetch's response-body
construction:

```rust
// crates/runtime/src/fetch/response.rs — sibling project
pub struct ResponseBuilder<'a> {
    /* opaque */
}

impl<'a> ResponseBuilder<'a> {
    /// Called between "headers parsed" and "Response.body exposed to JS".
    /// The compression layer registers a hook here that:
    ///   1. Reads `Content-Encoding` from headers, lowercase + OWS-trim,
    ///      parse as comma-separated #content-coding list per RFC 9110.
    ///   2. If non-empty and not [identity], constructs a chain of native
    ///      transformers (one per coding, reversed per RFC 9110 §8.4.1)
    ///      and pipes the inbound body bytes through them. The OUTPUT of
    ///      the chain becomes the `Response.body` ReadableStream.
    ///   3. Strips `Content-Encoding` and `Content-Length` from the
    ///      user-visible Headers (kept on a private slot for inspection).
    ///   4. On unknown coding, errors the response — promise rejects with
    ///      a `TypeError` ("network error: unsupported Content-Encoding").
    pub fn with_response_body_hook(self, hook: ResponseBodyHook) -> Self;
}

pub type ResponseBodyHook = Box<dyn Fn(
    &mut v8::PinScope,
    &Headers,
    BodySource,
) -> Result<(BodySource, Headers), JsError>>;
```

Native fetch also commits to:

6. Sending `Accept-Encoding: gzip, deflate, br` on every outbound request
   unless user code supplied an explicit `Accept-Encoding` header (including
   the empty string for opt-out). See D-10.
7. Parsing the response `Content-Encoding` header *byte-faithfully* (so the
   compression hook sees exactly what the wire said), with no premature
   normalisation that would drop multi-coding ordering.

## Architecture

### A. Native TransformStream contract this design assumes

(Summarised from the native-streams proposal; spelled out here so review
of this doc doesn't require flipping between docs.)

- `class TransformStream { constructor(transformer, writableStrategy?, readableStrategy?) }`
  is exposed to JS. Internally it accepts either a JS-side `transformer`
  object (`{ transform, flush, cancel }`) or — via `TransformStream::new_native`
  in Rust — a Rust `impl Transformer`.
- Internal slots `[[readable]]` / `[[writable]]` are V8 private symbols on
  the constructed object, populated during construction.
- The transformer's `transform(chunk, controller)` is called per inbound
  chunk; it can `controller.enqueue(bytes)` zero, one, or many times.
- `controller.desiredSize` exposes backpressure: ≤ 0 means "consumer is
  saturated; producer should pause." Native streams pause the writable
  side automatically; the transformer can also voluntarily defer enqueues.
- `transformer.cancel(reason)` is invoked exactly once when either side
  errors or is cancelled. (Required; was added to Streams spec in 2024.)

### B. Codec layer (Rust)

A trait, format-specific impls, and an explicit lifecycle. **No HashMap.
No `u32` handle.** The codec is a `Box<dyn Codec>` that lives in the
transformer struct's field; its lifetime is the transformer's lifetime.

```rust
pub enum CompressionFormat { Brotli, Deflate, DeflateRaw, Gzip }
pub enum CodecMode { Compress, Decompress }

#[derive(Debug)]
pub enum CodecError {
    /// Decompression encountered malformed input. Maps to the spec's
    /// "throw a TypeError" inside decompress-and-enqueue. Will become
    /// DOMException("DataError") if whatwg/compression#51 lands —
    /// gated by `DECODE_ERROR_USES_DOMEXCEPTION` (see D-15).
    DecodeData(&'static str),
    /// Decompression: input ended mid-stream. Spec
    /// `decompress-flush-and-enqueue` step 3.
    Truncated,
    /// Decompression: extra bytes after stream-end marker. Spec
    /// `decompress-and-enqueue` step 6.
    TrailingBytes,
    /// Backend allocation / IO failure. Always surfaced as TypeError
    /// regardless of D-15.
    Backend(String),
}

pub trait Codec: Send {
    /// Push input. Returns (produced bytes, input bytes consumed).
    /// `consumed < chunk.len()` means the codec hit stream-end with
    /// trailing data left over — caller fails the stream with
    /// CodecError::TrailingBytes after enqueueing the produced bytes.
    /// Per spec decompress-and-enqueue step 6. <!-- Added in round 2: addressing BLOCKER-1 -->
    fn write(&mut self, chunk: &[u8]) -> Result<(Vec<u8>, usize), CodecError>;

    /// Final call; emit any trailer bytes (gzip CRC + ISIZE; deflate
    /// adler32; brotli last-block bit). After Ok return, `finished`
    /// becomes true and subsequent finish/write calls return
    /// CodecError::DecodeData("codec consumed").
    /// Decompressors return CodecError::Truncated if the stream-end
    /// marker was never reached.
    /// <!-- Added in round 2: addressing BLOCKER-2, MAJOR-8 -->
    fn finish(&mut self) -> Result<Vec<u8>, CodecError>;

    /// Idempotent. Called once on cancel; called again on Drop. After
    /// cancel, all subsequent write/finish are no-ops returning
    /// empty / Ok. <!-- Added in round 2: addressing BLOCKER-2, Missing-2 -->
    fn cancel(&mut self);

    fn finished(&self) -> bool;
}
```

#### Per-codec implementations

All eight (4 formats × 2 directions) wrap a streaming backend writer/decoder
and track `finished: bool`. The shapes:

- **`GzipEncoder`** — `flate2::write::GzEncoder<Vec<u8>>` (single-member;
  matches spec). Produces gzip wire format (RFC 1952).
- **`DeflateEncoder` (zlib-wrapped)** — `flate2::write::ZlibEncoder` (RFC 1950).
- **`DeflateRawEncoder`** — `flate2::write::DeflateEncoder` (RFC 1951).
- **`BrotliEncoder`** — `brotli::CompressorWriter::new(Vec::new(), 4096, 4, 22)`
  (RFC 7932; quality=4 per D-12, lgwin=22).
- **`GzipDecoder`** — `flate2::write::GzDecoder<Vec<u8>>` (single-member only —
  `MultiGzDecoder` is opt-in in flate2 and we deliberately don't use it; spec
  rejects multi-member, see whatwg/compression#42).
- **`DeflateDecoder` (zlib-wrapped)** — `flate2::write::ZlibDecoder` (RFC 1950).
  Strict zlib header; rejects raw DEFLATE.
- **`DeflateRawDecoder`** — `flate2::write::DeflateDecoder` (RFC 1951).
- **`BrotliDecoder`** — `brotli::DecompressorWriter::new(Vec::new(), 4096)`.

#### `write` consumed-bytes tracking (BLOCKER-1)

This is the load-bearing detail the previous draft hand-waved. flate2's
streaming writers have the contract documented in their own test fixtures:

> *"ZlibDecoder consumes one zlib archive and then returns 0 for
> subsequent writes, allowing any additional data to be consumed by
> the caller."* — `/tmp/flate2-rs/src/zlib/write.rs:357-358`

The same comment exists verbatim at `/tmp/flate2-rs/src/gz/write.rs:614-615`.
Their unit tests `decode_extra_data` (lines 359-383 zlib, 616-640 gz)
explicitly drive a write-loop until `n == 0` then assert `finish().unwrap()`
*succeeds* with extra bytes still in the input. This means **`finish()`
does not error on trailing bytes**. The caller has to track `consumed_bytes`.

Our `write` impl:

```rust
fn write(&mut self, chunk: &[u8]) -> Result<(Vec<u8>, usize), CodecError> {
    if self.finished { return Ok((Vec::new(), 0)); }   // post-cancel/finish
    let mut consumed = 0;
    let mut produced = Vec::new();
    while consumed < chunk.len() {
        let n = self.inner.write(&chunk[consumed..])
            .map_err(|e| map_io_err::<DECODER>(e))?;
        // Drain inner's buffered output into `produced`.
        produced.extend_from_slice(self.inner.get_mut().drain(..));
        if n == 0 {
            // Stream-end marker reached; remaining bytes are post-stream
            // trailing data. The TransformStream-level closer will fail
            // with TrailingBytes if the writable side later closes
            // without further consumption — but we report the partial
            // consumed count NOW so the caller can detect it.
            break;
        }
        consumed += n;
    }
    Ok((produced, consumed))
}
```

The transformer-level wrapper enforces step 6 of
`decompress-and-enqueue` (https://compression.spec.whatwg.org/#decompress-and-enqueue-a-chunk):

```rust
fn transform(&mut self, chunk: &[u8], ctl: &mut TransformStreamController) -> Result<(), JsError> {
    // Spec step 3: "If buffer is empty, return." Empty chunk is a valid
    // no-op. Header bytes only emerge from flush. <!-- Added in round 2: addressing MAJOR-10 -->
    if chunk.is_empty() { return Ok(()); }

    // SAB rejection (D-5) happens at the IDL boundary, so by the time
    // we're here, `chunk` is guaranteed non-SAB-backed.
    let (out, consumed) = self.codec.write(chunk).map_err(map_codec_err)?;
    if !out.is_empty() {
        ctl.enqueue(scope, &out)?;
    }
    if consumed < chunk.len() {
        // Stream-end was reached with bytes left over. Per spec step 6,
        // throw TypeError. <!-- Added in round 2: addressing BLOCKER-1 -->
        return Err(JsError::type_error("trailing bytes after end of stream"));
    }
    Ok(())
}
```

Brotli's writer is friendlier — `brotli::DecompressorWriter::write`
returns `Ok(input_offset)` (the consumed count) directly per
`/tmp/brotli-decompressor/src/writer.rs:337-368`. We adopt the same
`(produced, consumed)` shape uniformly so the transformer is
codec-agnostic.

#### `finish` and the truncation case (MAJOR-11)

For decompressors:

```rust
fn finish(&mut self) -> Result<Vec<u8>, CodecError> {
    if self.finished { return Ok(Vec::new()); }
    self.finished = true;
    // flate2: writing zero bytes with stream-end already reached is a
    // no-op. If stream-end was NOT reached (truncated input), the
    // backend's internal state will surface Z_BUF_ERROR; we map to
    // CodecError::Truncated, which the transformer flush translates to
    // TypeError per https://compression.spec.whatwg.org/#decompress-flush-and-enqueue
    // step 3. Empty input → DecompressionStream falls through this same
    // path: the codec was constructed but never fed any bytes, stream-end
    // was never reached. <!-- Added in round 2: addressing MAJOR-11 -->
    match self.inner.try_finish() {
        Ok(()) => {
            let trailer = self.inner.get_mut().drain(..).collect();
            Ok(trailer)
        }
        Err(e) if is_z_buf_error(&e) => Err(CodecError::Truncated),
        Err(e) => Err(CodecError::Backend(e.to_string())),
    }
}
```

For compressors `finish` always succeeds (it just flushes any pending
output and emits the trailer — gzip CRC+ISIZE, zlib ADLER32, brotli
last-block bit).

#### Idempotent `cancel` and `Drop` (BLOCKER-2)

```rust
fn cancel(&mut self) {
    self.finished = true;  // poison further writes
    // No need to call inner.try_finish(); we drop the backend writer
    // when the codec is itself dropped. Backend's own Drop handles
    // releasing C-side state (none, since miniz_oxide and brotli are
    // pure Rust).
}

// Implicit Drop runs when the transformer is dropped, which runs when
// the TransformStream's transformer slot is dropped, which runs when
// the TransformStream is GC'd. No double-free path because there's no
// HashMap to remove from. <!-- Added in round 2: addressing BLOCKER-2, MAJOR-13 -->
```

### C. Transformer plumbing — how the codec attaches to native TransformStream

```rust
struct CodecTransformer {
    codec: Box<dyn Codec>,
    /// For internal-fetch decompression only; None for the public class.
    /// Once produced bytes exceed this, transform errors with
    /// CodecError::DecodeData("output too large"). <!-- Added in round 2: addressing Missing-9 -->
    max_output: Option<usize>,
    output_so_far: usize,
}

impl Transformer for CodecTransformer {
    fn transform(&mut self, chunk: &[u8], ctl: &mut TransformStreamController) -> Result<(), JsError> {
        if chunk.is_empty() { return Ok(()); }
        let (out, consumed) = self.codec.write(chunk).map_err(map_codec_err)?;
        if let Some(max) = self.max_output {
            self.output_so_far = self.output_so_far.saturating_add(out.len());
            if self.output_so_far > max {
                return Err(JsError::type_error("decompressed output exceeded limit"));
            }
        }
        if !out.is_empty() {
            // Backpressure (Missing-1): we don't gate enqueue on
            // desired_size — the streams spec says enqueue is always
            // legal; pulling stops at the writable side automatically
            // when desired_size <= 0. workerd uses the same approach;
            // the controller's internal queue absorbs the burst.
            ctl.enqueue(scope, &out)?;
        }
        if consumed < chunk.len() {
            return Err(JsError::type_error("trailing bytes after end of stream"));
        }
        Ok(())
    }

    fn flush(&mut self, ctl: &mut TransformStreamController) -> Result<(), JsError> {
        let trailer = self.codec.finish().map_err(map_codec_err)?;
        if !trailer.is_empty() {
            ctl.enqueue(scope, &trailer)?;
        }
        Ok(())
    }

    fn cancel(&mut self, _reason: v8::Local<v8::Value>) {
        self.codec.cancel();
    }
}
```

#### Lifetime model (replaces previous BLOCKER-3 section)

There is no second JS Global. The transformer struct is owned by the
TransformStream's `[[transformer]]` slot. When the TransformStream is
GC'd, V8 invokes the wrapper object's finalizer, which the native-streams
project hooks to drop the boxed `Box<dyn Transformer>`. That drop runs
`CodecTransformer::drop`, which drops `self.codec`, which runs the
codec's `Drop` impl. No cross-Global ordering, no `v8::Global::drop`
mutating V8 state from a finalizer, no double-free.

### D. fetch integration (internal decompression)

The hook registered with `ResponseBuilder::with_response_body_hook` runs
between "headers parsed" and "Response.body exposed":

```rust
fn compression_response_hook(
    scope: &mut v8::PinScope,
    headers: &Headers,
    body: BodySource,
) -> Result<(BodySource, Headers), JsError> {
    let raw = match headers.get_byte_str(b"content-encoding") {
        Some(v) => v,
        None => return Ok((body, headers.clone())),
    };
    // OWS-trim and lowercase per RFC 9110 ABNF + §8.4.1. <!-- Added in round 2: addressing MINOR-18 -->
    let codings = parse_content_codings(raw);  // Vec<&[u8]>, ordered.

    if codings.is_empty() || codings == [b"identity"] {
        // identity is no-op, never an error. <!-- Added in round 2: addressing MAJOR-7 -->
        return strip_and_clone(headers, body);
    }

    // RFC 9110 §8.4.1: "the content codings MUST be listed in the order
    // in which they were applied" — decode in REVERSE.
    // <!-- Added in round 2: addressing MAJOR-6 -->
    let mut current_body = body;
    for coding in codings.iter().rev() {
        let format = match coding.as_slice() {
            b"gzip" | b"x-gzip" => CompressionFormat::Gzip,
            b"br"   => CompressionFormat::Brotli,
            b"deflate" => {
                // D-6: lenient "try zlib then raw" for internal fetch.
                // Public DecompressionStream("deflate") stays strict.
                // <!-- Added in round 2: addressing MAJOR-14 -->
                current_body = pipe_through_lenient_deflate(scope, current_body)?;
                continue;
            }
            b"identity" => continue,
            _ => {
                // D-9: unknown coding => fetch network error.
                // <!-- Added in round 2: addressing MAJOR-7 -->
                return Err(JsError::type_error(
                    format!("unsupported Content-Encoding: {}",
                            String::from_utf8_lossy(coding))));
            }
        };
        let codec = make_codec(format, CodecMode::Decompress);
        let transformer = CodecTransformer {
            codec,
            max_output: Some(MAX_FETCH_DECODE_BYTES),  // D-14
            output_so_far: 0,
        };
        let ts = TransformStream::new_native(scope, transformer,
            QueuingStrategy::default(), QueuingStrategy::default());
        current_body = pipe_body_through(scope, current_body, ts)?;
    }

    // D-8: strip both headers post-decode. Match undici's
    // handleResponseBody and workerd's removeContentEncoding.
    // <!-- Added in round 2: addressing MAJOR-12 -->
    let mut new_headers = headers.clone();
    new_headers.delete_byte_str(b"content-encoding");
    new_headers.delete_byte_str(b"content-length");
    Ok((current_body, new_headers))
}
```

`pipe_body_through` is implemented in native-fetch using internal slots
of the `BodySource` and the TransformStream — it does *not* call public
`pipeThrough` and *cannot* be defeated by the body bridge locking the
ReadableStream because there is no public ReadableStream yet at this
point in construction. That eliminates the previous BLOCKER-4.

`pipe_through_lenient_deflate` builds two transformers — one strict-zlib,
one raw-DEFLATE — and a small adapter that probes the first ~4 bytes of
the input. Per zlib RFC 1950, a valid zlib stream's first byte CMF has
low nibble 0x8 and the (CMF*256 + FLG) is divisible by 31; if the probe
fails, switch to raw. Mirrors Chromium's
`net/filter/gzip_source_stream.cc` (the
`ZlibInflate::Init` fallback branch).

### E. Public API: `CompressionStream` / `DecompressionStream`

These are `#[v8_class]` types whose constructor builds a native
TransformStream with a `CodecTransformer` and stashes a reference to
the resulting object's `[[readable]]` / `[[writable]]` slots.

```rust
#[v8_class]
pub struct CompressionStream {
    /// The native TransformStream object. We hold a v8::Global to keep
    /// it alive as long as the JS-side CompressionStream is alive.
    transform_stream: v8::Global<v8::Object>,
}

#[v8_class]
impl CompressionStream {
    #[v8_constructor]
    fn new(scope: &mut v8::PinScope, format: String) -> Result<Self, OpError> {
        let format = parse_format(&format)?;  // throws TypeError on unknown
        // Per-isolate concurrent-codec cap (D-13).
        // <!-- Added in round 2: addressing Missing-3 -->
        check_codec_budget(scope)?;
        let codec = make_codec(format, CodecMode::Compress);
        let transformer = CodecTransformer { codec, max_output: None, output_so_far: 0 };
        let ts = TransformStream::new_native(
            scope, transformer,
            QueuingStrategy::default(),
            QueuingStrategy::default(),
        );
        Ok(CompressionStream {
            transform_stream: v8::Global::new(scope, ts),
        })
    }

    /// Per https://streams.spec.whatwg.org/#generictransformstream :
    /// "The readable getter steps are to return this's transform.[[readable]]."
    /// We read the internal slot directly via TransformStream::readable_slot,
    /// NOT via public .readable accessor. <!-- Added in round 2: addressing MINOR-20 -->
    #[v8_getter]
    fn readable<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        let ts = v8::Local::new(scope, &self.transform_stream);
        TransformStream::readable_slot(scope, ts)
    }

    #[v8_getter]
    fn writable<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        let ts = v8::Local::new(scope, &self.transform_stream);
        TransformStream::writable_slot(scope, ts)
    }
}
```

`DecompressionStream` is structurally identical with `CodecMode::Decompress`.

#### Macro lifetime threading proof (MAJOR-9)

The getter signature `fn readable<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value>`
ties the return Local's lifetime to the *original* PinScope's `'s`
parameter, not to a reborrow. The macro at
`crates/runtime-macros/src/lib.rs:504` already handles the `Local`
return type via `quote! { rv.set(#call.into()); }` — no intermediate
`let` binding. To make the lifetime survive the callback's body, the
macro's `gen_param_extractions` (currently emitting `let scope = &mut *scope;`
to reborrow) needs to be updated for getters/methods that return a
lifetime-tied `Local<'s, …>`: the reborrow shadow must be skipped, or
the call expression must be inlined directly using the original scope
binding.

The minimal macro change:

```rust
// In gen_param_extractions, when emitting the synthetic scope arg:
if method_returns_lifetime_tied_local(method) {
    quote! { /* no reborrow; pass `scope` as-is */ }
} else {
    quote! { let scope = &mut *scope; }
}
```

Where `method_returns_lifetime_tied_local` checks the return type
syntactically for `Local<'lt, _>` with `'lt` referencing one of the
method's generic lifetime params. If the method takes its own `'s`,
that's a tied Local; the reborrow is skipped to preserve the lifetime
chain.

Borrow-checker reasoning: with reborrow skipped, the call
`<CompressionStream>::readable(__instance, scope)` returns
`v8::Local<'s, v8::Value>` where `'s` is the outer callback's
`v8::PinScope` lifetime. `rv.set(__local.into())` consumes the Local
in the same expression — no intermediate borrow that could outlive
the scope. Compiles cleanly. (We can verify with a `compile_test`
fixture in `crates/runtime-macros/tests/`.)

### F. SAB rejection at the IDL boundary (BLOCKER-5)

Per WebIDL §3.2.21, buffer-source types reject SAB-backed views by
default; only `[AllowShared]` opts in. The Compression IDL does NOT
use `[AllowShared]`. So the `chunk` argument in
`transform(chunk, controller)` must reject SAB before the codec sees it.

The codec layer receives `&[u8]` via the TransformStream's transform
callback — the slice came from a `Vec<u8>` extraction inside the
macro-emitted argument code (or, under native streams, from an internal
chunk-as-bytes conversion). Either way, the rejection has to be one
level *up*: when the writable side accepts a chunk and decides whether
to convert it to bytes for the transformer.

We extend the `#[v8_class]` macro's `Vec<u8>`-from-BufferSource
extraction with an opt-in `#[reject_shared]` attribute:

```rust
// crates/runtime-macros/src/lib.rs gen_extract_vec_u8 — extended.
fn gen_extract_vec_u8(reject_shared: bool) -> TokenStream2 {
    let sab_check = if reject_shared {
        quote! {
            // Per WebIDL §3.2.21 + Compression spec absence of [AllowShared].
            // <!-- Added in round 2: addressing BLOCKER-5 -->
            if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(__arg) {
                if let Some(buf) = view.buffer(scope) {
                    if buf.is_shared() {
                        let __msg = v8::String::new(scope, "SharedArrayBuffer not allowed").unwrap();
                        let __exc = v8::Exception::type_error(scope, __msg);
                        scope.throw_exception(__exc);
                        return;
                    }
                }
            }
            if let Ok(buf) = v8::Local::<v8::ArrayBuffer>::try_from(__arg) {
                if buf.is_shared() {
                    let __msg = v8::String::new(scope, "SharedArrayBuffer not allowed").unwrap();
                    let __exc = v8::Exception::type_error(scope, __msg);
                    scope.throw_exception(__exc);
                    return;
                }
            }
        }
    } else { quote! {} };

    quote! {
        #sab_check
        let __vec: Vec<u8> = /* existing extraction */;
    }
}
```

The transform-callback site (in native-streams' generic chunk-to-bytes
adapter, or the public-class transform method) opts in via
`#[reject_shared]` on the `chunk: Vec<u8>` parameter.

### G. Telemetry (Missing-4)

Codec metrics, surfaced via the existing runtime metrics layer:

| Metric | Type | Notes |
|---|---|---|
| `compression_codec_creations_total{format,mode}` | counter | per format × {compress,decompress} |
| `compression_codec_failures_total{reason}` | counter | reason ∈ {truncated, trailing_bytes, decode_data, backend, output_too_large, sab_rejected} |
| `compression_input_bytes_total{format,mode}` | counter | matches workerd metric name |
| `compression_output_bytes_total{format,mode}` | counter | matches workerd metric name |
| `compression_codec_active` | gauge | for D-13 budget visibility |
| `fetch_content_encoding_observed_total{coding}` | counter | one per inbound response, before strip |
| `fetch_content_encoding_unsupported_total{coding}` | counter | for D-9 unknown-coding errors |

Names mirror workerd's `compression_*_total` family.

## Spec corner cases the impl must handle

Per the research findings, the following are spec-mandated and tested
by WPT.

### Compression

- **Unknown format** in constructor → `TypeError` (synchronous from
  constructor). `parse_format` rejects.
- **Non-`BufferSource` chunk** written → `TypeError` from the transform
  algorithm. The TransformStream's writable-side chunk-to-bytes adapter
  validates; the macro layer enforces `Vec<u8>`-from-BufferSource.
- **`SharedArrayBuffer`-backed view** → **rejected** with `TypeError`
  at the IDL boundary (§F above). WPT `compression-bad-chunks.any.js` verifies.
- **Empty input** through `transform` → no output (spec step 3:
  *"If buffer is empty, return"*). Header bytes only emerge from
  `flush` if and only if the writer closes. Test must wait for close
  before asserting bytes.

### Decompression

- **Garbage in compressed bytes** → `TypeError` (errors stream).
- **Trailing bytes after end** → `TypeError` via per-chunk consumed-byte
  tracking (§B).
- **Truncated input** (close before EOS marker) → `TypeError` via
  `Codec::finish` returning `CodecError::Truncated`, mapped from
  `Z_BUF_ERROR`.
- **Empty input → DecompressionStream** → `TypeError` via the *flush*
  algorithm (`decompress-flush-and-enqueue` step 3 — *"If the end of
  the compressed input has not been reached"*), not via header-byte
  validation.
- **Multi-member gzip** → reject. We use single-member `GzDecoder`.
  Spec rejects (whatwg/compression#42).
- **`deflate` (zlib wrapper) vs `deflate-raw`** — public API strict per
  spec; internal-fetch lenient per D-6.

## Open spec questions (don't block v1)

- **`zstd`** — open issue #54 (https://github.com/whatwg/compression/issues/54).
  Not in spec. Some browsers ship `Accept-Encoding: zstd` for HTTP but
  not via CompressionStream API. Skip; revisit when whatwg/compression#54 lands.
- **Multi-member gzip** — open issue #42. Spec rejects single-member
  only. We match.
- **`TypeError` vs `DOMException("DataError")`** — open issue #51.
  Spec currently says TypeError. We track via D-15: a single
  `DECODE_ERROR_USES_DOMEXCEPTION` constant flips the JS-side
  error wrapper without touching the codec layer.

## Concrete spec citations, in algorithm order

For implementer reference; each step number must be matchable to a code
site in the impl.

| Spec hook | Code site (anticipated) |
|---|---|
| https://compression.spec.whatwg.org/#compress-and-enqueue-a-chunk step 3 ("buffer empty → return") | `CodecTransformer::transform` early-return on `chunk.is_empty()` |
| https://compression.spec.whatwg.org/#compress-and-enqueue-a-chunk step 5 ("enqueue") | `ctl.enqueue(scope, &out)` |
| https://compression.spec.whatwg.org/#decompress-and-enqueue-a-chunk step 6 ("fully consumed check") | `consumed < chunk.len()` branch |
| https://compression.spec.whatwg.org/#compress-flush-and-enqueue | `CodecTransformer::flush` for compressors |
| https://compression.spec.whatwg.org/#decompress-flush-and-enqueue step 3 ("end not reached") | `Codec::finish` returning `Truncated` for decompressors |
| https://streams.spec.whatwg.org/#generictransformstream readable getter | `TransformStream::readable_slot` |
| RFC 9110 §8.4.1 reverse-order decode | for-loop `codings.iter().rev()` |
| RFC 9110 §16.6 unsupported coding error path | unknown-coding match arm |

## Missing concepts addressed

### Missing-1: Backpressure

Native streams' controller exposes `desired_size`. The transformer does
**not** gate enqueue on it (per Streams spec, enqueue is always legal);
instead, native streams pause the writable side when `desired_size <= 0`
and resume when the consumer reads. This matches workerd's pause/resume
model (`workerd/api/streams/transform.c++`). Our code reads
`desired_size` only for telemetry; backpressure is automatic.

### Missing-2: Cancellation

`Transformer::cancel(reason)` is part of the trait surface (§A); our
`CodecTransformer::cancel` calls `self.codec.cancel()`, which sets
`finished = true` and poisons further writes. Codec-internal buffers
are released by the codec's own Drop. Native streams guarantees `cancel`
is called exactly once before drop.

### Missing-3: Worker memory budgets

D-13: per-isolate cap of 1024 concurrent codecs (`ZEROSHIP_MAX_CONCURRENT_CODECS`).
Brotli decoder context is ~8 MB worst case (per RFC 7932 LZ77 window
of `1 << lgwin`); 1024 × 8 MB = 8 GB worst case, vs the 160 GB
unbounded scenario. Excess constructions throw `TypeError("too many
concurrent codecs")` synchronously. The cap is per-isolate, and one
isolate hosts one app (per `crates/worker/src/cache.rs` invariant), so
this is also the per-app cap.

### Missing-4: Telemetry

§G above.

### Missing-5: CRIME/BREACH

Security note in the SDK doc:

> Compressing user-controlled data alongside secrets is a known
> side-channel attack class (CRIME, BREACH). If you compress a payload
> that interleaves an attacker-controllable string with a secret token,
> the compressed length leaks information about the secret. The runtime
> exposes `CompressionStream` for any creator app, but the platform's
> auth tokens and session cookies are managed by the gateway and never
> flow through user code; user-side compression of *only* response
> bodies (not auth headers) is safe. We do not gate the API on this.

Documented but not gated. Workerd takes the same stance.

### Missing-6: Accept-Encoding egress

D-10. Native fetch sends `Accept-Encoding: gzip, deflate, br` on every
outbound request unless user code supplied an explicit value (including
the empty string for opt-out). Without this, the test plan's gzip
endpoint silently sends plain JSON.

### Missing-7: Streaming POST request bodies

Outbound compression is symmetric: `fetch(url, { body: stream.pipeThrough(new CompressionStream("gzip")), headers: { "content-encoding": "gzip" } })`
works because the public `CompressionStream` is a real native
TransformStream (§E). The user must set `Content-Encoding: gzip`
themselves on the request — native fetch does **not** auto-compress
outbound bodies (no spec requirement, no real-world expectation;
servers don't always accept it). The gateway forwards the compressed
body bytes unchanged.

### Missing-8: HTTP/3 qpack

Out of scope. QPACK is HTTP/3 *header* compression; `Content-Encoding`
is *body* coding. They're orthogonal; the gateway terminates HTTP/3 and
passes decoded headers + body bytes to the worker. No conflation.

### Missing-9: Zip-bomb defense

D-14. Internal-fetch decompression is capped at 100 MB output per
decoder (`MAX_FETCH_DECODE_BYTES`). User-constructed `DecompressionStream`
is uncapped because the user controls input size and may have a
legitimate reason (e.g. decompressing a 1 GB archive); user code can
self-cap by counting enqueued bytes.

### Missing-10: Safari/iOS WebView brotli compat

Out of scope for the runtime (we control the runtime, not user-agent
clients). Documented in the public-facing platform docs: if a creator
app emits `Content-Encoding: br` to a Safari ≤17 client, the response
body is unreadable downstream. Recommend `gzip` for compatibility-
critical paths until brotli ubiquity (currently >95% of evergreen
browsers, but iOS WebViews lag).

## Why pure native (replaces "Why wrap a JS TransformStream")

The platform commits to native Web API implementations as the
default — Headers (proposed), URL, fetch, streams. The previous
draft wrapped a JS polyfill TransformStream because native streams
weren't yet on the roadmap; round-1 review surfaced two architectural
blockers (V8 finalization ordering between the wrapper Global and the
JS TransformStream Global; the body-bridge-locks-stream chicken-and-egg
problem) that exist *only* in the wrap-JS architecture. Pivoting to
pure native removes both by construction. It also gives us:

- A single GC graph (the codec is a Rust field of a native object;
  no second Global).
- Direct internal-slot reads for `[[readable]]` / `[[writable]]`
  per Streams §GenericTransformStream (MINOR-20).
- A direct codec hand-off for internal-fetch decompression that
  doesn't go through public `pipeThrough` (eliminates BLOCKER-4).
- One `Box<dyn Codec>` per stream instead of a `HashMap<u32, …>`
  with handle-exhaustion risk at 200K req/s (MAJOR-13 / D-4).

The only cost is the dependency on the native-streams project, which
is already on the critical path for performance reasons unrelated to
compression.

## Comparison table

| Component | Lines (est.) | Notes |
|---|---|---|
| Codec layer (Rust) — trait + 8 codec impls + error mapping | ~360 | Pure Rust, no JS interop. The bulk of the work. |
| `CodecTransformer` (Rust) — Transformer trait impl, telemetry hooks | ~80 | Glues codec to native streams. |
| Macro extension: `#[reject_shared]` for `Vec<u8>` extraction | ~40 | One-time macro work; reusable for other AllowShared-sensitive APIs. |
| Macro extension: lifetime-tied Local return | ~30 | One-time; reusable for other GenericTransformStream-style getters. |
| `#[v8_class] CompressionStream` + `DecompressionStream` | ~120 | Constructor + two getters each. |
| Fetch integration — `compression_response_hook` + Content-Encoding parser + lenient deflate fallback + Accept-Encoding egress | ~180 | Multi-coding chain, header strip, identity handling, unknown-coding error. |
| **Total** | **~810** | Vs workerd's 640 LOC C++ (no macro extensions; their language is more verbose). |

For comparison, Deno's wrap-JS approach is `ext/web/compression.rs`
(204 lines Rust) + `ext/web/14_compression.js` (~140 lines JS). They
have all of the truncation/trailing-byte detection (~80 lines just for
that) plus their own backpressure and cancel handling.

## File layout

```
crates/runtime/src/
├── compression/
│   ├── mod.rs                  (new) public Codec trait + factory
│   ├── codec_gzip.rs           (new) GzipEncoder/GzipDecoder
│   ├── codec_deflate.rs        (new) Zlib + Raw variants of both
│   ├── codec_brotli.rs         (new) BrotliEncoder/BrotliDecoder
│   ├── transformer.rs          (new) CodecTransformer impl Transformer
│   ├── classes.rs              (new) #[v8_class] CompressionStream + DecompressionStream
│   ├── fetch_hook.rs           (new) compression_response_hook + Content-Encoding parser
│   └── budget.rs               (new) D-13 concurrent-codec cap, telemetry counters
├── lib.rs                      (modified) +pub mod compression;
└── init.rs                     (modified) install both classes; register
                                fetch_hook with native-fetch builder.

crates/runtime-macros/src/
└── lib.rs                      (modified) +#[reject_shared] arg attr,
                                +lifetime-tied Local return handling.

crates/runtime/Cargo.toml        flate2 = "1" (default features),
                                brotli = { version = "8", default-features = false,
                                           features = ["std", "ffi-api"] }
                                                                    /* see MINOR-21 */

crates/runtime/tests/
├── compression.rs              (new) hand-written smoke tests
├── wpt_compression.rs          (new) WPT runner
└── wpt/compression/            (vendored from web-platform-tests)
```

## Test plan

### Hand-written smoke (`tests/compression.rs`)

Per-format, per-direction:

- **Constructor**: each of `gzip`, `deflate`, `deflate-raw`, `brotli` works.
- **Unknown format**: `new CompressionStream("zstd")` throws `TypeError` synchronously.
- **`readable` / `writable`**: return `ReadableStream` / `WritableStream`
  instances (verify via `instanceof` + `Symbol.toStringTag`).
- **`readable` / `writable` slot read**: tamper with
  `Object.defineProperty(TransformStream.prototype, "readable", { get: throw })`
  and verify our getter still works (slot-read, not accessor — MINOR-20).
- **Round-trip**: compress → decompress → original bytes, for each format.
- **Empty `transform` call**: write `new Uint8Array(0)` → no enqueue
  observable until close. After close, format header bytes appear
  (MAJOR-10 — the test asserts ordering, not content during transform).
- **Empty input → DecompressionStream**: writer closes immediately →
  `TypeError` on flush (MAJOR-11). Verify error type at the readable
  side's `read()` rejection.
- **Truncated gzip** (header only, no trailer) → `TypeError` on
  decompression flush.
- **Trailing garbage** (gzip + extra bytes) → `TypeError` on decompress
  during `transform` (BLOCKER-1 — must fire as soon as the trailing
  byte is fed, not on flush).
- **Wrong format**: gzip bytes through `"deflate"` decoder → `TypeError`.
- **SAB-backed view**: `new Uint8Array(new SharedArrayBuffer(8))` →
  `TypeError` synchronously from the writable side's `write` method
  (BLOCKER-5 / D-5).
- **Idempotent cancel**: cancel the readable side mid-stream; writable
  side's `write` rejects; cancel again → no error, no double-drop
  (BLOCKER-2 / D-2).
- **Idempotent finish**: write some data; close; the codec's `finish`
  ran; force a second close (no-op); no panic.
- **Lenient `deflate` (internal fetch path only)**: feed raw DEFLATE
  bytes through the fetch hook with `Content-Encoding: deflate` →
  decodes successfully (D-6 / MAJOR-14).
- **Strict `deflate` (public class)**: feed raw DEFLATE bytes through
  `new DecompressionStream("deflate")` → `TypeError` (D-6).
- **Concurrent-codec cap**: construct 1025 `CompressionStream`s →
  the 1025th throws `TypeError("too many concurrent codecs")`
  (D-13 / Missing-3).
- **Max-output cap (internal fetch only)**: feed a zip-bomb response
  (small input, > 100 MB output) → fetch promise rejects with
  `TypeError("decompressed output exceeded limit")` (D-14 / Missing-9).

### WPT runner (`tests/wpt_compression.rs`)

Vendor and run all WPT compression tests. The brotli files were folded
into the existing parameterised test files when whatwg/compression PR
#80 landed; we list the actual file inventory rather than "to be added":

| File | Coverage | brotli? |
|---|---|---|
| `compression-bad-chunks.any.js` | Rejects undefined/null/number/SAB/etc | parameterised |
| `compression-constructor-error.any.js` | Bad format → TypeError | parameterised |
| `compression-including-empty-chunk.any.js` | Empty chunks pass through | parameterised |
| `compression-large-flush-output.any.js` | Flush emits all buffered | parameterised |
| `compression-multiple-chunks.any.js` | Sequential chunks valid | parameterised |
| `compression-output-length.any.js` | Round-trip lengths | parameterised |
| `compression-stream.any.js` | E2E roundtrip per format | parameterised |
| `compression-with-detach.window.js` | Detach mid-write handled | parameterised |
| `decompression-bad-chunks.any.js` | Same rejection list | parameterised |
| `decompression-buffersource.any.js` | All BufferSource types accepted | parameterised |
| `decompression-constructor-error.any.js` | Bad format errors | parameterised |
| `decompression-correct-input.any.js` | Known-good fixtures | format-specific arrays |
| `decompression-corrupt-input.any.js` | CMF/FLG/checksum mutations | parameterised |
| `decompression-empty-input.any.js` | Empty → TypeError on close | parameterised |
| `decompression-extra-input.any.js` | Trailing bytes → TypeError | parameterised |
| `decompression-split-chunk.any.js` | Header split works | parameterised |
| `decompression-uint8array-output.any.js` | Output is Uint8Array | parameterised |
| `decompression-with-detach.window.js` | Detached input handling | parameterised |
| `idlharness.https.any.js` | IDL conformance | n/a |

(The brotli format is included in each parameterised test's format
array; the WPT convention is to drive `compression-stream.any.js`
through `["gzip", "deflate", "deflate-raw", "br"]`. <!-- Added in round 2: addressing MINOR-22 -->)

Same harness pattern as `wpt_text_encoding.rs`:
- Inject testharness shim
- Run each file, classify pass/fail/skip
- Report per-file totals

Target: 100% pass on all 19 files.

### Internal-fetch verification

Add to `examples/ai-chat/e2e/`:

1. **Egress header asserted**: hit a controlled local mock that *records*
   inbound request headers. Assert `Accept-Encoding: gzip, deflate, br`
   was sent. (D-10 / Missing-6 / MAJOR-15.)
2. **Response coding asserted + decoded**: that same mock responds with
   `Content-Encoding: gzip` + gzipped JSON. Assert `response.headers.get("content-encoding")`
   returns `null` after our strip (D-8), `response.headers.get("content-length")`
   returns `null` (D-8), and `response.json()` returns the decoded
   object. Without the egress header AND without the coding header, the
   test fails (vs the previous draft where both could be missing and the
   test would pass for the wrong reason — MAJOR-15).
3. **Multi-coding**: mock responds with `Content-Encoding: gzip, br`
   and a body that's gzip-then-br-encoded. Assert decoded JSON.
   (MAJOR-6.)
4. **Identity coding**: mock responds with `Content-Encoding: identity`
   and plain JSON. Assert decoded JSON, no error. (MAJOR-7 / D-9.)
5. **Unknown coding**: mock responds with `Content-Encoding: zstd` and
   zstd-encoded bytes. Assert fetch promise rejects with `TypeError`.
   (D-9.)
6. **Lenient deflate**: mock responds with `Content-Encoding: deflate`
   and *raw* DEFLATE bytes (no zlib wrapper). Assert decoded JSON.
   (D-6 / MAJOR-14.)

## Implementation sequence — honest hour breakdown

Given the work is ~810 LOC across new code, two macro extensions, and
three sibling-project integration points, here's a component-level
breakdown. Numbers reflect this codebase's pace (per the
estimates-hours-not-weeks calibration) and assume the native-streams
and native-fetch projects have landed their stable surfaces (§Dependencies).

| # | Component | Hours |
|---|---|---|
| 1 | Codec layer: trait + 8 codec impls (gzip, deflate, deflate-raw, brotli; encode + decode) with consumed-bytes tracking, idempotent cancel, finish/Truncated mapping | 1.5 |
| 2 | Macro extensions: `#[reject_shared]` on `Vec<u8>` and lifetime-tied `Local<'s, _>` return handling, with compile-test fixtures | 0.75 |
| 3 | `CodecTransformer` impl `Transformer` (native-streams trait): transform/flush/cancel, max-output cap, telemetry counters | 0.5 |
| 4 | `#[v8_class] CompressionStream` + `DecompressionStream`: constructor, slot-read getters, concurrent-codec budget check | 0.5 |
| 5 | Fetch integration: response-body hook, Content-Encoding parser (RFC 9110 OWS/case/comma), reverse-order chain, lenient deflate fallback, identity handling, unknown-coding error, header strip | 1.25 |
| 6 | Accept-Encoding egress in native fetch | 0.25 |
| 7 | Hand-written smoke tests (above) | 1.0 |
| 8 | WPT vendor + runner + iterate to 100% on 19 files | 1.5 |
| 9 | Internal-fetch e2e tests (6 cases above) | 0.5 |
| **Total** | | **~7.75 h** |

(The previous draft's 2-hour estimate was wildly optimistic. <!-- Added in round 2: addressing NIT-23 -->
The new estimate is honest about the macro work, the WPT iteration,
and the multi-coding / lenient-deflate corner cases. It still excludes
the native-streams and native-fetch sibling work, which is on its own
critical path.)

## References

- Compression spec: https://compression.spec.whatwg.org/
  - https://compression.spec.whatwg.org/#compress-and-enqueue-a-chunk
  - https://compression.spec.whatwg.org/#decompress-and-enqueue-a-chunk
  - https://compression.spec.whatwg.org/#compress-flush-and-enqueue
  - https://compression.spec.whatwg.org/#decompress-flush-and-enqueue
- Brotli merge PR: https://github.com/whatwg/compression/pull/80
- Spec issue #51 (TypeError vs DOMException): https://github.com/whatwg/compression/issues/51
- WPT tests: https://github.com/web-platform-tests/wpt/tree/master/compression
- Streams §GenericTransformStream: https://streams.spec.whatwg.org/#generictransformstream
- Fetch §4.6 / Content-Encoding handling: https://fetch.spec.whatwg.org/
- RFC 9110 §8.4 / §8.4.1 (Content-Encoding): https://www.rfc-editor.org/rfc/rfc9110.html#section-8.4
- RFC 9110 §16.6 (Content-Coding registration): https://www.rfc-editor.org/rfc/rfc9110.html#section-16.6
- WebIDL `[AllowShared]` / BufferSource: https://webidl.spec.whatwg.org/#idl-buffer-source-types
- RFCs: 1950 (zlib), 1951 (deflate), 1952 (gzip), 7932 (brotli)
- flate2 source confirming trailing-byte behavior:
  - `/tmp/flate2-rs/src/zlib/write.rs:357-383` (`decode_extra_data` test)
  - `/tmp/flate2-rs/src/gz/write.rs:614-640` (same pattern)
- brotli-decompressor source: `/tmp/brotli-decompressor/src/writer.rs:337-368`
  (`write` returns consumed offset; `close` at L257-289 returns silently
  on trailing data — same caller-tracks-consumption contract)
- undici Content-Encoding strip: `lib/web/fetch/index.js` `handleResponseBody`
- workerd `Content-Encoding` strip: `src/workerd/api/global-scope.c++`
  `handleResponseBody → removeContentEncoding`
- workerd compression: `src/workerd/api/streams/compression.{h,c++}`
  (640 LOC; reference for `kj::Maybe<Context>` post-finish state and
  `IoOwn`-based codec ownership model)
- Deno compression (wrap-JS reference): `ext/web/14_compression.js`
  (~140 lines) + `ext/web/compression.rs` (204 lines)
- Chromium lenient deflate fallback:
  `net/filter/gzip_source_stream.cc` (`ZlibInflate::Init`)
- zlib FAQ §39 on raw-DEFLATE-as-deflate: https://zlib.net/zlib_faq.html#faq39
