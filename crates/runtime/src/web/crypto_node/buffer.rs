//! Buffer / Uint8Array / DataView / ArrayBuffer / string input
//! coercion + Buffer-shaped output emission.
//!
//! Per `docs/proposals/node-crypto-native.md` §I.6 (D-N7).
//!
//! - `extract_input(value, encoding?)` — coerce a JS value to `Vec<u8>`
//!   per Node's documented rule: when the value is a Buffer / TypedArray
//!   / DataView / ArrayBuffer, the encoding is IGNORED; when the value
//!   is a string, the encoding (default `'utf8'`) applies.
//!
//! - `emit_buffer(scope, bytes)` — mint a Buffer wrapping the bytes.
//!   Strategy: allocate a fresh `Uint8Array` (V8 native) backed by
//!   our bytes, then call `Buffer.from(uint8)` to retag. The
//!   Buffer.from path is the documented Node way to convert (see
//!   https://nodejs.org/api/buffer.html#static-method-bufferfromarray).
//!
//! - `emit_string(scope, s)` — convenience for digest('hex') etc.
//!
//! - `emit_output(scope, bytes, encoding?)` — emit Buffer when
//!   `encoding` is None, else the appropriate string encoding.

use super::encoding::{self, Encoding};
use crate::state::OpError;

/// Coerce a JS value to bytes. Per Node's input rule:
///
/// - `Buffer` / `Uint8Array` / `DataView` / `ArrayBufferView` /
///   `ArrayBuffer` → copy raw bytes; `encoding` is IGNORED.
/// - `string` → decode per `encoding` (default `'utf8'`).
///
/// Errors:
/// - `ERR_INVALID_ARG_TYPE` if value isn't one of the above shapes.
/// - `ERR_UNKNOWN_ENCODING` if `encoding` is provided but unknown
///   (only when the value is a string).
pub fn extract_input(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
    encoding: Option<&str>,
) -> Result<Vec<u8>, OpError> {
    // 1. ArrayBufferView (Uint8Array, Buffer, Int8Array, ...) → copy bytes.
    //    Per Node spec, `encoding` is IGNORED for non-string input.
    if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(value) {
        let mut buf = vec![0u8; view.byte_length()];
        view.copy_contents(&mut buf);
        return Ok(buf);
    }
    // 2. ArrayBuffer → copy bytes.
    if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(value) {
        let store = ab.get_backing_store();
        let mut buf = vec![0u8; ab.byte_length()];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = store[i].get();
        }
        return Ok(buf);
    }
    // 3. String → encoding APPLIES; default 'utf8' if not provided.
    if value.is_string() {
        let s = value.to_rust_string_lossy(scope);
        let enc_name = encoding.unwrap_or("utf8");
        let enc = encoding::from_str(enc_name).ok_or_else(|| {
            OpError::node("ERR_UNKNOWN_ENCODING", format!("Unknown encoding: {enc_name}"))
        })?;
        return encoding::decode(&s, enc);
    }
    Err(OpError::node(
        "ERR_INVALID_ARG_TYPE",
        "Argument must be a Buffer, TypedArray, DataView, ArrayBuffer, or string",
    ))
}

/// Mint a `Uint8Array` wrapping `bytes` (a fresh V8 ArrayBuffer with
/// the bytes copied in). This is the bare V8-native shape; the
/// `emit_buffer` variant retags it as a Node Buffer via `Buffer.from`.
pub fn emit_uint8array<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: &[u8],
) -> v8::Local<'s, v8::Uint8Array> {
    let len = bytes.len();
    let ab = v8::ArrayBuffer::new(scope, len);
    if len > 0 {
        let store = ab.get_backing_store();
        for (i, &b) in bytes.iter().enumerate() {
            store[i].set(b);
        }
    }
    v8::Uint8Array::new(scope, ab, 0, len).expect("Uint8Array allocation")
}

/// Emit a Node `Buffer`-shaped value. Strategy: mint a fresh Uint8Array
/// then call `globalThis.Buffer.from(u8)` to retag. The Buffer global
/// is installed by unenv (see `node:buffer`). If `Buffer` isn't
/// available (e.g. tests where unenv hasn't run), we return the
/// Uint8Array directly — npm packages that consume crypto outputs
/// don't strictly require Buffer.prototype methods (the Uint8Array
/// surface is enough for `.toString('hex')` style uses since we
/// return strings on encoding-specified paths).
pub fn emit_buffer<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: &[u8],
) -> v8::Local<'s, v8::Value> {
    let u8a = emit_uint8array(scope, bytes);
    // Look up globalThis.Buffer.from(u8). Cached lookup not yet wired
    // — the cost is one global lookup per crypto call, ~50 ns. Per
    // §I.6 the design notes this is amortised to zero post-bootstrap
    // once we add the cached lookup.
    let context = scope.get_current_context();
    let global = context.global(scope);
    let buffer_key = v8::String::new(scope, "Buffer").unwrap();
    let buffer_val = match global.get(scope, buffer_key.into()) {
        Some(v) => v,
        None => return u8a.into(),
    };
    let buffer_obj = match v8::Local::<v8::Object>::try_from(buffer_val) {
        Ok(o) => o,
        Err(_) => return u8a.into(),
    };
    let from_key = v8::String::new(scope, "from").unwrap();
    let from_val = match buffer_obj.get(scope, from_key.into()) {
        Some(v) => v,
        None => return u8a.into(),
    };
    let from_fn = match v8::Local::<v8::Function>::try_from(from_val) {
        Ok(f) => f,
        Err(_) => return u8a.into(),
    };
    let recv = buffer_obj.into();
    match from_fn.call(scope, recv, &[u8a.into()]) {
        Some(result) => result,
        None => u8a.into(),
    }
}

/// Emit a string value (used for digest('hex') / etc.).
pub fn emit_string<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    s: &str,
) -> v8::Local<'s, v8::String> {
    v8::String::new(scope, s).unwrap_or_else(|| v8::String::empty(scope))
}

/// `emit_output` — emit Buffer or string per Node's "encoding-driven
/// dispatch" rule:
///
/// - `encoding == None` → return Buffer (the default).
/// - `encoding == Some(enc)` → return a string encoded per `enc`.
pub fn emit_output<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: &[u8],
    encoding_name: Option<&str>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    match encoding_name {
        None => Ok(emit_buffer(scope, bytes)),
        Some(name) => {
            let enc = encoding::from_str(name).ok_or_else(|| {
                OpError::node("ERR_UNKNOWN_ENCODING", format!("Unknown encoding: {name}"))
            })?;
            let s = encoding::encode(bytes, enc);
            Ok(emit_string(scope, &s).into())
        }
    }
}
