//! Native `TextEncoderStream` and `TextDecoderStream` per WHATWG
//! Encoding §6 (https://encoding.spec.whatwg.org/#interface-textencoderstream
//! and §7).
//!
//! Replaces the 70-LOC `embed/text-streams.js` shim, which closed only
//! the streaming-decode behavior and missed every spec contract a
//! library checks: `Symbol.toStringTag`, brand-checked accessors, the
//! GenericTransformStream `readable` / `writable` getter shape.
//!
//! ## Why classes, not just JS-side wrappers
//!
//! Two real failure modes the shim left open:
//!
//! 1. `Object.prototype.toString.call(new TextEncoderStream())` returned
//!    `"[object Object]"`. Libraries (the AI SDK is one) brand-check via
//!    that exact string when a passed value claims to be a stream — a
//!    raw object survives, but it is detected as foreign. The native
//!    class installs `Symbol.toStringTag` on the prototype.
//!
//! 2. The shim assigned `this.readable = ts.readable; this.writable =
//!    ts.writable` in the constructor. Per WHATWG GenericTransformStream
//!    §6.1, those are accessor properties on the prototype that read
//!    `[[readable]]` / `[[writable]]` internal slots. Libraries that do
//!    `Object.getOwnPropertyDescriptor(Object.getPrototypeOf(ts),
//!    "readable")` to introspect a stream's pipeline see the descriptor
//!    is missing on the shim — resulting in fallback paths that allocate
//!    extra adapters (a measurable hot-path cost in worker tiers).
//!
//! ## Architecture
//!
//! Each stream class wraps an underlying TransformStream constructed via
//! the user-visible `globalThis.TransformStream` constructor with a
//! synthetic underlyingTransformer object. The transformer's `transform`
//! and `flush` callbacks are FunctionTemplate-backed C functions that
//! delegate to a heap-allocated native TextEncoder / TextDecoder kept
//! alive by the External payload. This reuses the well-tested JS-from-
//! transformer construction path in `streams::transform` rather than
//! threading the Rust-side `from_native_transformer` (whose driver
//! algorithms are still placeholders, see comments in
//! `transform_controller.rs:1335`).
//!
//! The TransformStream wrapper is held as a `v8::Global<v8::Object>` in
//! the `#[v8_class]`-generated instance state; `readable` and `writable`
//! getters reborrow it and read its `[[readable]]` / `[[writable]]`
//! private-symbol slots — the same slots the public TransformStream
//! getters expose, so identity is preserved (`tes.readable === tes
//! .readable`).

use std::cell::RefCell;
use std::rc::Rc;

use encoding_rs::{DecoderResult, Encoding};

use zeroship_runtime_macros::v8_class;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_constructor, v8_getter, v8_method};

use crate::state::OpError;
use crate::streams::slots;

// ---------------------------------------------------------------------------
// TextEncoderStream
// ---------------------------------------------------------------------------

/// Heap-resident encoder state shared between the underlying
/// transformer's `transform` callback and the class instance. The
/// callback's data slot owns a raw pointer to the box; the class
/// instance holds a `v8::Global` to the function whose finalizer will
/// reclaim the box on GC. We stick to UTF-8 unconditionally per spec.
struct EncoderState;

/// `TextEncoderStream` — WHATWG Encoding §6.
///
/// IDL:
/// ```webidl
/// [Exposed=*]
/// interface TextEncoderStream {
///   constructor();
///   readonly attribute DOMString encoding;
/// };
/// TextEncoderStream includes GenericTransformStream;
/// ```
pub struct TextEncoderStream {
    /// The wrapped TransformStream. The class's `readable` / `writable`
    /// getters reborrow this Global, then read the spec-mandated
    /// `[[readable]]` / `[[writable]]` priv-sym slots that the underlying
    /// stream sets at construction time.
    inner: v8::Global<v8::Object>,
}

#[v8_class]
impl TextEncoderStream {
    #[v8_constructor]
    fn new(scope: &mut v8::PinScope) -> Result<Self, OpError> {
        // 1. Build the underlyingTransformer object: { transform, flush }.
        //    transform delegates to UTF-8 encode; flush is a no-op (every
        //    chunk is fully encoded inline — UTF-8 has no carryover).
        let state: Rc<RefCell<EncoderState>> = Rc::new(RefCell::new(EncoderState));

        let transformer = v8::Object::new(scope);

        let transform_fn = build_payload_function(
            scope,
            EncoderPayload {
                state: state.clone(),
            },
            encoder_transform_callback,
        );
        let key = v8::String::new(scope, "transform").unwrap();
        transformer.set(scope, key.into(), transform_fn.into());

        // No flush — UTF-8 has no end-of-stream state. Omitting `flush`
        // causes the spec's defaultFlushAlgorithm (a no-op) to apply.

        // 2. Call new TransformStream(transformer).
        let ts = construct_transform_stream(scope, transformer.into()).ok_or_else(|| {
            OpError::error("TextEncoderStream: failed to construct underlying TransformStream")
        })?;

        Ok(TextEncoderStream {
            inner: v8::Global::new(scope, ts),
        })
    }

    /// `readonly attribute ReadableStream readable` (GenericTransformStream §6.1).
    /// Reads `[[readable]]` of the wrapped TransformStream.
    #[v8_getter]
    fn readable<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        let ts = v8::Local::new(scope, &self.inner);
        slots::read_slot(scope, ts, slots::READABLE)
    }

    /// `readonly attribute WritableStream writable` (GenericTransformStream §6.1).
    #[v8_getter]
    fn writable<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        let ts = v8::Local::new(scope, &self.inner);
        slots::read_slot(scope, ts, slots::WRITABLE)
    }

    /// `readonly attribute DOMString encoding` — always `"utf-8"`.
    #[v8_getter]
    fn encoding(&self) -> String {
        "utf-8".into()
    }
}

#[allow(missing_debug_implementations)]
struct EncoderPayload {
    state: Rc<RefCell<EncoderState>>,
}

/// `transform(chunk, controller)` for TextEncoderStream.
///
/// Per WHATWG §6.4: chunk is converted to a USVString, encoded as
/// UTF-8, and the resulting Uint8Array is enqueued on the controller.
/// Empty results (chunk === "") are dropped — the spec says "if the
/// resulting byte sequence is empty, return; otherwise enqueue".
fn encoder_transform_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    // The payload is here only to keep ownership tied to the
    // FunctionTemplate's lifetime. We don't actually need to read it —
    // UTF-8 encode is stateless. Touching the External keeps the
    // finalizer-attached storage live.
    let _ = recover_payload::<EncoderPayload>(&args);

    let chunk_v = args.get(0);
    let controller_v = args.get(1);

    // Convert chunk to USVString (V8's to_string handles ToString;
    // unpaired surrogates substituted by V8's WTF-16 → UTF-8 path).
    let Some(chunk_str) = chunk_v.to_string(scope) else {
        let msg = v8::String::new(
            scope,
            "TextEncoderStream: chunk could not be converted to a string",
        )
        .unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    };
    let utf8 = chunk_str.to_rust_string_lossy(scope);
    if utf8.is_empty() {
        return;
    }

    // Build the Uint8Array.
    let n = utf8.len();
    let ab = v8::ArrayBuffer::new(scope, n);
    {
        let store = ab.get_backing_store();
        for (i, b) in utf8.as_bytes().iter().enumerate() {
            store[i].set(*b);
        }
    }
    let chunk_u8 = v8::Uint8Array::new(scope, ab, 0, n).unwrap();

    enqueue_on_controller(scope, controller_v, chunk_u8.into());
}

// ---------------------------------------------------------------------------
// TextDecoderStream
// ---------------------------------------------------------------------------

/// Heap-resident decoder state. Persists across chunks so the
/// `encoding_rs::Decoder` can carry partial UTF-8 sequences (the AI-SDK
/// SSE bug the JS shim was originally written to address).
struct DecoderState {
    encoding: &'static Encoding,
    /// Lazily-instantiated. Created on first chunk; remains live for
    /// the entire stream lifetime — a TextDecoderStream is a one-shot
    /// session, so unlike `TextDecoder.decode()` we don't reset between
    /// calls. Flushed once on the writable side's close.
    decoder: Option<encoding_rs::Decoder>,
    fatal_flag: bool,
    ignore_bom_flag: bool,
}

impl DecoderState {
    fn ensure_decoder(&mut self) -> &mut encoding_rs::Decoder {
        if self.decoder.is_none() {
            let dec = if self.ignore_bom_flag {
                self.encoding.new_decoder_without_bom_handling()
            } else {
                self.encoding.new_decoder_with_bom_removal()
            };
            self.decoder = Some(dec);
        }
        self.decoder.as_mut().unwrap()
    }
}

/// `TextDecoderStream` — WHATWG Encoding §7.
///
/// IDL:
/// ```webidl
/// [Exposed=*]
/// interface TextDecoderStream {
///   constructor(optional DOMString label = "utf-8",
///               optional TextDecoderOptions options = {});
///   readonly attribute DOMString encoding;
///   readonly attribute boolean fatal;
///   readonly attribute boolean ignoreBOM;
/// };
/// TextDecoderStream includes GenericTransformStream;
/// ```
pub struct TextDecoderStream {
    /// Decoder state, shared with the transform/flush callbacks. The
    /// instance keeps a clone so the getters (`encoding` / `fatal` /
    /// `ignoreBOM`) read consistent values regardless of how many
    /// chunks have been processed.
    state: Rc<RefCell<DecoderState>>,
    inner: v8::Global<v8::Object>,
}

#[v8_class]
impl TextDecoderStream {
    /// `new TextDecoderStream(label = "utf-8", options = {})`.
    ///
    /// Per spec §7.4: rejects unknown labels and the `replacement`
    /// encoding with `RangeError`; `options` follows the WebIDL
    /// dictionary rules (undefined / null / object accepted, anything
    /// else rejects with TypeError).
    #[v8_constructor]
    fn new(
        scope: &mut v8::PinScope,
        label: v8::Local<v8::Value>,
        options: v8::Local<v8::Value>,
    ) -> Result<Self, OpError> {
        // 1. Resolve the encoding label. Per spec the default when
        //    `label` is undefined is "utf-8"; null coerces to the
        //    string "null" via DOMString rules and is rejected as an
        //    unknown encoding.
        let label_str = if label.is_undefined() {
            "utf-8".to_string()
        } else {
            label.to_rust_string_lossy(scope)
        };
        let encoding = match Encoding::for_label(label_str.as_bytes()) {
            Some(enc) => enc,
            None => {
                return Err(OpError::range_error(
                    "TextDecoderStream: unsupported encoding label",
                ));
            }
        };
        if encoding == encoding_rs::REPLACEMENT {
            return Err(OpError::range_error(
                "TextDecoderStream: replacement encoding is not a valid label",
            ));
        }

        // 2. Read fatal / ignoreBOM from options. Delegates to the
        //    parent module's `TextDecoderOptions` derive — same WebIDL
        //    dict as TextDecoder uses.
        let opts = super::TextDecoderOptions::from_v8(scope, options)?;
        let fatal_flag = opts.fatal;
        let ignore_bom_flag = opts.ignore_bom;

        // 3. Build the decoder state, shared between the callbacks and
        //    the class instance's getters.
        let state = Rc::new(RefCell::new(DecoderState {
            encoding,
            decoder: None,
            fatal_flag,
            ignore_bom_flag,
        }));

        // 4. Build the underlyingTransformer = { transform, flush }.
        let transformer = v8::Object::new(scope);

        let transform_fn = build_payload_function(
            scope,
            DecoderPayload {
                state: state.clone(),
            },
            decoder_transform_callback,
        );
        let key = v8::String::new(scope, "transform").unwrap();
        transformer.set(scope, key.into(), transform_fn.into());

        let flush_fn = build_payload_function(
            scope,
            DecoderPayload {
                state: state.clone(),
            },
            decoder_flush_callback,
        );
        let key = v8::String::new(scope, "flush").unwrap();
        transformer.set(scope, key.into(), flush_fn.into());

        // 5. Construct the underlying TransformStream.
        let ts = construct_transform_stream(scope, transformer.into()).ok_or_else(|| {
            OpError::error("TextDecoderStream: failed to construct underlying TransformStream")
        })?;

        Ok(TextDecoderStream {
            state,
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

    /// Spec returns the canonical lowercase form
    /// (`"utf-8"`, `"windows-1252"`, …). encoding_rs's `name()` is
    /// title-case (`"UTF-8"`); we lowercase to match the spec output.
    #[v8_getter]
    fn encoding(&self) -> String {
        self.state.borrow().encoding.name().to_ascii_lowercase()
    }

    #[v8_getter]
    fn fatal(&self) -> bool {
        self.state.borrow().fatal_flag
    }

    /// Spec name is `ignoreBOM`; suppress the snake_case lint locally —
    /// when the macro grows `#[v8_name = "ignoreBOM"]` for getters this
    /// can move back to `ignore_bom`.
    #[v8_getter]
    #[allow(non_snake_case)]
    fn ignoreBOM(&self) -> bool {
        self.state.borrow().ignore_bom_flag
    }
}

#[allow(missing_debug_implementations)]
struct DecoderPayload {
    state: Rc<RefCell<DecoderState>>,
}

/// `transform(chunk, controller)` for TextDecoderStream.
///
/// Decodes the chunk's bytes in streaming mode (`last = false`), so a
/// partial multi-byte UTF-8 sequence at the end is held in the
/// decoder's internal state until the next chunk arrives. Spec §7.4
/// step 3: enqueue the resulting USVString if non-empty.
fn decoder_transform_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let Some(payload) = recover_payload::<DecoderPayload>(&args) else {
        return;
    };

    let chunk_v = args.get(0);
    let controller_v = args.get(1);

    // BufferSource coercion. Anything else throws TypeError.
    let bytes = match read_buffer_source(chunk_v) {
        Ok(b) => b,
        Err(_) => {
            let msg = v8::String::new(
                scope,
                "TextDecoderStream: chunk must be a BufferSource",
            )
            .unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };

    let mut state = payload.state.borrow_mut();
    let fatal = state.fatal_flag;
    let decoder = state.ensure_decoder();

    let max_out = decoder
        .max_utf8_buffer_length(bytes.len())
        .unwrap_or_else(|| bytes.len().saturating_mul(3) + 8);
    let mut out = String::with_capacity(max_out);

    let result_is_err = if fatal {
        let (result, _read) = decoder.decode_to_string_without_replacement(&bytes, &mut out, false);
        matches!(result, DecoderResult::Malformed(_, _))
    } else {
        // Non-fatal: encoding_rs substitutes U+FFFD on bad sequences.
        let (_result, _read, _replaced) = decoder.decode_to_string(&bytes, &mut out, false);
        false
    };

    drop(state);

    if result_is_err {
        let msg = v8::String::new(
            scope,
            "TextDecoderStream: invalid byte sequence (fatal mode)",
        )
        .unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }

    if out.is_empty() {
        return;
    }
    let chunk_out = v8::String::new(scope, &out).unwrap();
    enqueue_on_controller(scope, controller_v, chunk_out.into());
}

/// `flush(controller)` for TextDecoderStream.
///
/// Per spec §7.4 step 4: drain the decoder one final time with
/// `last = true`. In non-fatal mode any pending partial sequence is
/// replaced with U+FFFD; in fatal mode it errors the stream.
fn decoder_flush_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let Some(payload) = recover_payload::<DecoderPayload>(&args) else {
        return;
    };
    let controller_v = args.get(0);

    let mut state = payload.state.borrow_mut();
    let fatal = state.fatal_flag;

    // If the decoder was never instantiated (no chunks ever arrived),
    // there's nothing to flush.
    if state.decoder.is_none() {
        return;
    }

    let decoder = state.decoder.as_mut().unwrap();

    let max_out = decoder.max_utf8_buffer_length(0).unwrap_or(8);
    let mut out = String::with_capacity(max_out);

    let result_is_err = if fatal {
        let (result, _read) = decoder.decode_to_string_without_replacement(&[], &mut out, true);
        matches!(result, DecoderResult::Malformed(_, _))
    } else {
        let (_result, _read, _replaced) = decoder.decode_to_string(&[], &mut out, true);
        false
    };

    // Drop the decoder so any subsequent re-flush on the same callback
    // (defensive — spec says flush runs once) doesn't hit
    // encoding_rs's "use after finalize" panic.
    state.decoder = None;
    drop(state);

    if result_is_err {
        let msg = v8::String::new(
            scope,
            "TextDecoderStream: incomplete byte sequence at end of input (fatal mode)",
        )
        .unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }

    if out.is_empty() {
        return;
    }
    let chunk_out = v8::String::new(scope, &out).unwrap();
    enqueue_on_controller(scope, controller_v, chunk_out.into());
}

// ---------------------------------------------------------------------------
// Helpers — payload-carrying FunctionTemplate, controller dispatch
// ---------------------------------------------------------------------------

/// Build a JS Function whose `data` slot owns a heap `Box<P>`. The
/// underlying box is reclaimed by a guaranteed weak finalizer when V8
/// GCs the function — same pattern Blob.stream's start callback uses.
///
/// The callback signature must match V8's `FunctionCallback`. Inside
/// the callback, `recover_payload::<P>` retrieves a `&P` by reading
/// the External pointer back to the heap box.
fn build_payload_function<'s, P: 'static>(
    scope: &mut v8::PinScope<'s, '_>,
    payload: P,
    callback: impl v8::MapFnTo<v8::FunctionCallback>,
) -> v8::Local<'s, v8::Function> {
    let boxed = Box::new(payload);
    let raw_ptr = Box::into_raw(boxed);
    let raw_addr = raw_ptr as usize;
    let ext = v8::External::new(scope, raw_ptr as *mut std::ffi::c_void);

    let tmpl = v8::FunctionTemplate::builder(callback)
        .data(ext.into())
        .build(scope);
    let f = tmpl.get_function(scope).unwrap();

    // Reclaim the box when the function (and thus the External slot)
    // is no longer reachable. The function is held alive by the
    // underlyingTransformer object, which is held by the
    // TransformStream's controller's algorithm slots — so it stays
    // alive as long as the TransformStream does.
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        f,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut P));
        }),
    );
    std::mem::forget(weak);

    f
}

/// Recover a borrow of the payload box for a callback. Returns `None`
/// if the data slot is not an External or is null (defensive — the
/// build path always populates it).
///
/// The borrow's lifetime is tied to the FunctionTemplate's data slot
/// (held alive by the underlyingTransformer object that the
/// TransformStream's controller retains). For the duration of any
/// single callback invocation the External is necessarily live; the
/// returned `&P` is safe for that scope.
fn recover_payload<'a, P: 'static>(
    args: &'a v8::FunctionCallbackArguments<'_>,
) -> Option<&'a P> {
    let data = args.data();
    let ext = v8::Local::<v8::External>::try_from(data).ok()?;
    let raw = ext.value() as *const P;
    if raw.is_null() {
        return None;
    }
    Some(unsafe { &*raw })
}

/// Look up `globalThis.TransformStream` and call `new
/// TransformStream(transformer)`. Returns the resulting object, or
/// `None` if the constructor threw or the global is missing.
///
/// Pulling from globalThis (rather than calling
/// `streams::transform::install_native_transform_stream` directly)
/// matches the way Blob.stream() builds ReadableStreams — the JS-
/// visible class is the source of truth, and any future replacement
/// (e.g. spec-mandated transferable variants) flows through one place.
fn construct_transform_stream<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    transformer: v8::Local<'s, v8::Value>,
) -> Option<v8::Local<'s, v8::Object>> {
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, "TransformStream").unwrap();
    let class_v = global.get(scope, key.into())?;
    let class_fn = v8::Local::<v8::Function>::try_from(class_v).ok()?;
    class_fn.new_instance(scope, &[transformer])
}

/// Dispatch a chunk via `controller.enqueue(chunk)`. Mirrors the
/// callback dispatch the polyfill used; we go through the JS-visible
/// method so the brand-check inside `TransformStreamDefaultController.
/// enqueue` runs (and surfaces its TypeError correctly if, e.g., the
/// readable side has been errored).
fn enqueue_on_controller(
    scope: &mut v8::PinScope,
    controller_v: v8::Local<v8::Value>,
    chunk: v8::Local<v8::Value>,
) {
    let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) else {
        return;
    };
    let key = v8::String::new(scope, "enqueue").unwrap();
    let Some(enqueue_v) = controller.get(scope, key.into()) else {
        return;
    };
    let Ok(enqueue_fn) = v8::Local::<v8::Function>::try_from(enqueue_v) else {
        return;
    };
    let _ = enqueue_fn.call(scope, controller.into(), &[chunk]);
}

// ---------------------------------------------------------------------------
// Buffer-source helper — local copy of the same shape that lives in
// the parent module's TextDecoder. Decoder-options parsing is shared
// via `super::TextDecoderOptions` (a WebIdlDict derive); only the
// buffer reader stays local because TextDecoderStream's chunk path
// has stricter "required BufferSource" semantics than TextDecoder's
// "optional BufferSource" path (the streams transform algorithm
// makes it required, while TextDecoder.decode allows undefined).
// ---------------------------------------------------------------------------

fn read_buffer_source(val: v8::Local<v8::Value>) -> Result<Vec<u8>, OpError> {
    // Unlike `TextDecoder.decode()` (where `input` is `optional
    // BufferSource`, so `undefined` legitimately means "no input"),
    // the streams transform algorithm specifies the chunk as a
    // required `BufferSource` parameter — undefined / null / number /
    // plain object must reject with TypeError. WPT
    // `decode-bad-chunks.any.js` enforces this.
    if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(val) {
        let mut buf = vec![0u8; view.byte_length()];
        view.copy_contents(&mut buf);
        return Ok(buf);
    }
    if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(val) {
        let store = ab.get_backing_store();
        let mut buf = vec![0u8; ab.byte_length()];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = store[i].get();
        }
        return Ok(buf);
    }
    Err(OpError::type_error(
        "TextDecoderStream: chunk must be ArrayBuffer or ArrayBufferView",
    ))
}
