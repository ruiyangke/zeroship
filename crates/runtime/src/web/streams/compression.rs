//! Native `CompressionStream` and `DecompressionStream` per WHATWG
//! Compression Standard (https://compression.spec.whatwg.org/).
//!
//! Mirrors `web/encoding/streams.rs` (TextEncoderStream / TextDecoderStream):
//! the class is a `#[v8_class]` wrapper that constructs an underlying
//! `globalThis.TransformStream` with a synthetic underlyingTransformer
//! whose `transform`/`flush` callbacks are FunctionTemplate-backed C
//! functions delegating to a heap-resident `Box<dyn Codec>`. Reusing the
//! JS-from-transformer construction path in `streams::transform` is
//! straightforward and avoids the still-stub native-transformer driver
//! (see `transform_controller.rs::set_up_transform_stream_default_controller_native`).
//!
//! Spec corner cases handled:
//!   - Unknown format (constructor) → `TypeError`.
//!   - Non-BufferSource chunk (transform) → `TypeError`.
//!   - SharedArrayBuffer-backed view (transform) → `TypeError`.
//!   - Empty chunk → no-op (no enqueue).
//!   - Trailing bytes after stream-end (decompression) → `TypeError`
//!     on the offending `transform` call (per `decompress-and-enqueue`
//!     step 6).
//!   - Truncated input (decompression flush) → `TypeError` (per
//!     `decompress-flush-and-enqueue` step 3).
//!   - Empty input → DecompressionStream → `TypeError` on flush
//!     (no stream-end marker ever observed).

use std::cell::RefCell;
use std::rc::Rc;

use zeroship_runtime_macros::v8_class;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_constructor, v8_getter};

use crate::codec::{make_codec, Codec, CodecError, CodecMode, CompressionFormat};
use crate::state::OpError;
use crate::streams::slots;

// ---------------------------------------------------------------------------
// Format parsing
// ---------------------------------------------------------------------------

/// Per spec, the constructor accepts: `gzip`, `deflate`, `deflate-raw`,
/// `brotli`. Any other string throws `TypeError`.
fn parse_format(s: &str) -> Result<CompressionFormat, OpError> {
    match s {
        "gzip" => Ok(CompressionFormat::Gzip),
        "deflate" => Ok(CompressionFormat::Deflate),
        "deflate-raw" => Ok(CompressionFormat::DeflateRaw),
        "brotli" => Ok(CompressionFormat::Brotli),
        _ => Err(OpError::type_error(format!(
            "unsupported compression format: '{s}'"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Shared codec payload + transformer plumbing
// ---------------------------------------------------------------------------

/// Heap-resident codec state. Shared between the transformer's
/// `transform` and `flush` callbacks via the FunctionTemplate's
/// External-data slot. `RefCell` because the codec is `&mut self` and
/// the two callbacks fire on the same thread serially.
struct CodecState {
    codec: Box<dyn Codec>,
    /// Set true once `flush` has run; subsequent transform calls become
    /// no-ops (defensive — spec only flushes once, but a misbehaving
    /// transformer could be re-driven by user code holding the writable
    /// half).
    flushed: bool,
}

/// FunctionTemplate data payload — owns the shared `Rc<RefCell<CodecState>>`.
/// The Rc is dropped (and the codec finalised via `Drop`) when V8 GCs
/// the function, reclaiming the External via the guaranteed weak
/// finalizer.
struct CodecPayload {
    state: Rc<RefCell<CodecState>>,
}

fn build_payload_function<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: Rc<RefCell<CodecState>>,
    callback: impl v8::MapFnTo<v8::FunctionCallback>,
) -> v8::Local<'s, v8::Function> {
    let boxed = Box::new(CodecPayload { state });
    let raw_ptr = Box::into_raw(boxed);
    let raw_addr = raw_ptr as usize;
    let ext = v8::External::new(scope, raw_ptr as *mut std::ffi::c_void);

    let tmpl = v8::FunctionTemplate::builder(callback)
        .data(ext.into())
        .build(scope);
    let f = tmpl.get_function(scope).unwrap();

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        f,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut CodecPayload));
        }),
    );
    std::mem::forget(weak);

    f
}

fn recover_payload<'a>(
    args: &'a v8::FunctionCallbackArguments<'_>,
) -> Option<&'a CodecPayload> {
    let data = args.data();
    let ext = v8::Local::<v8::External>::try_from(data).ok()?;
    let raw = ext.value() as *const CodecPayload;
    if raw.is_null() {
        return None;
    }
    Some(unsafe { &*raw })
}

/// Convert a JS chunk to bytes, rejecting non-BufferSource and SAB-backed
/// views with `TypeError`.
fn read_buffer_source(
    scope: &mut v8::PinScope,
    val: v8::Local<v8::Value>,
) -> Result<Vec<u8>, &'static str> {
    if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(val) {
        if let Some(buf) = view.buffer(scope) {
            if buf.is_shared_array_buffer() {
                return Err("SharedArrayBuffer-backed views are not allowed");
            }
        }
        let mut out = vec![0u8; view.byte_length()];
        view.copy_contents(&mut out);
        return Ok(out);
    }
    if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(val) {
        if ab.is_shared_array_buffer() {
            return Err("SharedArrayBuffer is not allowed");
        }
        let store = ab.get_backing_store();
        let mut out = vec![0u8; ab.byte_length()];
        for (i, b) in out.iter_mut().enumerate() {
            *b = store[i].get();
        }
        return Ok(out);
    }
    Err("chunk must be a BufferSource")
}

fn throw_type_error(scope: &mut v8::PinScope, msg: &str) {
    let m = v8::String::new(scope, msg).unwrap();
    let exc = v8::Exception::type_error(scope, m);
    scope.throw_exception(exc);
}

fn map_codec_error(err: &CodecError) -> String {
    err.to_string()
}

/// Build a `Uint8Array` from a Rust byte slice and enqueue it on the
/// controller. Goes through the JS-visible `controller.enqueue(chunk)`
/// so the brand-check inside `TransformStreamDefaultController.enqueue`
/// runs (and surfaces TypeError if the readable side has been errored).
fn enqueue_bytes(
    scope: &mut v8::PinScope,
    controller_v: v8::Local<v8::Value>,
    bytes: &[u8],
) {
    if bytes.is_empty() {
        return;
    }
    let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) else {
        return;
    };
    let n = bytes.len();
    let ab = v8::ArrayBuffer::new(scope, n);
    {
        let store = ab.get_backing_store();
        for (i, b) in bytes.iter().enumerate() {
            store[i].set(*b);
        }
    }
    let chunk = v8::Uint8Array::new(scope, ab, 0, n).unwrap();
    let key = v8::String::new(scope, "enqueue").unwrap();
    let Some(enqueue_v) = controller.get(scope, key.into()) else {
        return;
    };
    let Ok(enqueue_fn) = v8::Local::<v8::Function>::try_from(enqueue_v) else {
        return;
    };
    let _ = enqueue_fn.call(scope, controller.into(), &[chunk.into()]);
}

/// `transform(chunk, controller)` — feed bytes to the codec and enqueue
/// any produced output. Surfaces TypeError on bad input or trailing
/// bytes (decompression).
fn codec_transform_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let Some(payload) = recover_payload(&args) else {
        return;
    };
    let chunk_v = args.get(0);
    let controller_v = args.get(1);

    // BufferSource (rejects SAB).
    let bytes = match read_buffer_source(scope, chunk_v) {
        Ok(b) => b,
        Err(msg) => {
            throw_type_error(scope, msg);
            return;
        }
    };
    if bytes.is_empty() {
        return;
    }

    let mut state = payload.state.borrow_mut();
    if state.flushed {
        return;
    }
    let result = state.codec.write(&bytes);
    let (produced, consumed) = match result {
        Ok(p) => p,
        Err(e) => {
            let msg = map_codec_error(&e);
            drop(state);
            throw_type_error(scope, &msg);
            return;
        }
    };
    drop(state);

    if !produced.is_empty() {
        enqueue_bytes(scope, controller_v, &produced);
    }
    if consumed < bytes.len() {
        // Spec `decompress-and-enqueue` step 6: trailing bytes after
        // stream end → TypeError.
        throw_type_error(scope, "trailing bytes after end of compressed stream");
    }
}

/// `flush(controller)` — finalise the codec, emit trailer bytes (or
/// surface truncation TypeError for decompressors).
fn codec_flush_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let Some(payload) = recover_payload(&args) else {
        return;
    };
    let controller_v = args.get(0);

    let mut state = payload.state.borrow_mut();
    if state.flushed {
        return;
    }
    state.flushed = true;
    let trailer = match state.codec.finish() {
        Ok(t) => t,
        Err(e) => {
            let msg = map_codec_error(&e);
            drop(state);
            throw_type_error(scope, &msg);
            return;
        }
    };
    drop(state);

    if !trailer.is_empty() {
        enqueue_bytes(scope, controller_v, &trailer);
    }
}

/// Build the underlyingTransformer object `{ transform, flush }` and
/// construct the JS-visible TransformStream from it.
fn construct_transform_stream<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: Rc<RefCell<CodecState>>,
) -> Option<v8::Local<'s, v8::Object>> {
    let transformer = v8::Object::new(scope);

    let transform_fn = build_payload_function(scope, state.clone(), codec_transform_callback);
    let key = v8::String::new(scope, "transform").unwrap();
    transformer.set(scope, key.into(), transform_fn.into());

    let flush_fn = build_payload_function(scope, state, codec_flush_callback);
    let key = v8::String::new(scope, "flush").unwrap();
    transformer.set(scope, key.into(), flush_fn.into());

    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, "TransformStream").unwrap();
    let class_v = global.get(scope, key.into())?;
    let class_fn = v8::Local::<v8::Function>::try_from(class_v).ok()?;
    class_fn.new_instance(scope, &[transformer.into()])
}

// ---------------------------------------------------------------------------
// CompressionStream
// ---------------------------------------------------------------------------

/// `CompressionStream` — WHATWG Compression §2.
///
/// IDL:
/// ```webidl
/// [Exposed=*]
/// interface CompressionStream {
///   constructor(CompressionFormat format);
/// };
/// CompressionStream includes GenericTransformStream;
/// ```
pub struct CompressionStream {
    inner: v8::Global<v8::Object>,
}

#[v8_class]
impl CompressionStream {
    #[v8_constructor]
    fn new(scope: &mut v8::PinScope, format: String) -> Result<Self, OpError> {
        let format = parse_format(&format)?;
        let codec = make_codec(format, CodecMode::Compress);
        let state = Rc::new(RefCell::new(CodecState {
            codec,
            flushed: false,
        }));
        let ts = construct_transform_stream(scope, state).ok_or_else(|| {
            OpError::error("CompressionStream: failed to construct underlying TransformStream")
        })?;
        Ok(CompressionStream {
            inner: v8::Global::new(scope, ts),
        })
    }

    #[v8_getter]
    fn readable<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        let ts = v8::Local::new(scope, &self.inner);
        slots::read_slot(scope, ts, slots::READABLE)
    }

    #[v8_getter]
    fn writable<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        let ts = v8::Local::new(scope, &self.inner);
        slots::read_slot(scope, ts, slots::WRITABLE)
    }
}

// ---------------------------------------------------------------------------
// DecompressionStream
// ---------------------------------------------------------------------------

/// `DecompressionStream` — WHATWG Compression §3.
///
/// IDL:
/// ```webidl
/// [Exposed=*]
/// interface DecompressionStream {
///   constructor(CompressionFormat format);
/// };
/// DecompressionStream includes GenericTransformStream;
/// ```
pub struct DecompressionStream {
    inner: v8::Global<v8::Object>,
}

#[v8_class]
impl DecompressionStream {
    #[v8_constructor]
    fn new(scope: &mut v8::PinScope, format: String) -> Result<Self, OpError> {
        let format = parse_format(&format)?;
        let codec = make_codec(format, CodecMode::Decompress);
        let state = Rc::new(RefCell::new(CodecState {
            codec,
            flushed: false,
        }));
        let ts = construct_transform_stream(scope, state).ok_or_else(|| {
            OpError::error("DecompressionStream: failed to construct underlying TransformStream")
        })?;
        Ok(DecompressionStream {
            inner: v8::Global::new(scope, ts),
        })
    }

    #[v8_getter]
    fn readable<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        let ts = v8::Local::new(scope, &self.inner);
        slots::read_slot(scope, ts, slots::READABLE)
    }

    #[v8_getter]
    fn writable<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        let ts = v8::Local::new(scope, &self.inner);
        slots::read_slot(scope, ts, slots::WRITABLE)
    }
}

// ---------------------------------------------------------------------------
// install_globals
// ---------------------------------------------------------------------------

/// Wire `globalThis.CompressionStream` and `globalThis.DecompressionStream`.
/// Must run AFTER `install_native_streams` because the constructors call
/// `new globalThis.TransformStream(...)`.
pub fn install_globals<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    // #198 — bare template + globalThis bind for both classes.
    crate::register_native_classes!(scope, global, [
        CompressionStream,
        DecompressionStream,
    ]);
}
