//! Native `structuredClone` per WHATWG HTML §2.7.3
//! (https://html.spec.whatwg.org/multipage/structured-data.html#dom-structuredclone).
//!
//! Replaces the JS polyfill that lived in `embed/fetch.js`. The polyfill
//! used `JSON.parse(JSON.stringify(obj))` which is wrong on every count
//! that the spec actually exercises:
//!
//!   - `structuredClone(new Map([["a", 1]]))` → JSON-roundtrip yields
//!     `{}` (Map serialises to "[object Map]" which JSON skips). Spec
//!     says: clone the Map, preserving entries.
//!   - `structuredClone(new Date())` → roundtrip yields a string. Spec
//!     says: clone the Date.
//!   - `structuredClone(new ArrayBuffer(8))` → roundtrip yields `{}`.
//!     Spec says: clone the buffer.
//!   - `structuredClone(circular)` → roundtrip throws "circular
//!     structure". Spec says: clone, with shared identity preserved.
//!   - `structuredClone(() => 1)` → roundtrip silently returns
//!     `undefined`. Spec says: throw `DataCloneError`.
//!
//! The native implementation drives V8's `ValueSerializer` /
//! `ValueDeserializer` (the WHATWG structured-clone algorithm
//! reference impl). All the above cases work correctly.
//!
//! ## v1 scope
//!
//! Optional `options.transfer` (ArrayBuffer / MessagePort / etc.
//! transferable handoff) is deferred — `structuredClone(value)`
//! always clones, never transfers. Transferable support requires
//! threading the transfer list through ValueSerializer's
//! `transferArrayBuffer`/`getSharedArrayBufferId` machinery; future
//! work.

use v8::{ValueDeserializerHelper, ValueSerializerHelper};

use super::dom::exception;

// ---------------------------------------------------------------------------
// Serializer / Deserializer impls
// ---------------------------------------------------------------------------

/// V8 ValueSerializer delegate. We forward `throw_data_clone_error`
/// to a real native `DOMException("DataCloneError")` so user code
/// observes the spec-mandated error shape (instanceof DOMException +
/// `code === 25`).
struct CloneSerializer;

impl v8::ValueSerializerImpl for CloneSerializer {
    fn throw_data_clone_error<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        message: v8::Local<'s, v8::String>,
    ) {
        let msg = message.to_rust_string_lossy(scope);
        exception::throw(scope, &msg, "DataCloneError");
    }
}

/// V8 ValueDeserializer delegate. Default impls throw on every host
/// hook — we don't expose a host-object protocol, so the defaults
/// are correct.
struct CloneDeserializer;

impl v8::ValueDeserializerImpl for CloneDeserializer {}

// ---------------------------------------------------------------------------
// `structuredClone(value, options?)` callback
// ---------------------------------------------------------------------------

/// `structuredClone(value, options?)` — invokes the WHATWG
/// structured-clone algorithm by round-tripping through V8's
/// ValueSerializer/Deserializer. `options.transfer` is parsed but
/// unused in v1 (see module docs).
pub fn structured_clone_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let value = args.get(0);

    // Spec: undefined input clones to undefined; null clones to null.
    // V8's serializer handles these natively.

    let context = scope.get_current_context();

    // Serialize.
    let buf = {
        let serializer = v8::ValueSerializer::new(scope, Box::new(CloneSerializer));
        serializer.write_header();
        let wrote = serializer.write_value(context, value);
        match wrote {
            Some(true) => {}
            // `None` or `Some(false)` means an exception was set on
            // the scope (DataCloneError, etc.) — return without
            // setting `rv`. The pending exception propagates.
            _ => return,
        }
        serializer.release()
    };

    // Deserialize. The wire format starts with a header byte that
    // ValueDeserializer::read_value won't auto-consume — we MUST call
    // read_header first or ReadValue interprets the header byte as a
    // host-object marker and dispatches into the (unimplemented)
    // read_host_object hook.
    let deserializer = v8::ValueDeserializer::new(scope, Box::new(CloneDeserializer), &buf);
    match deserializer.read_header(context) {
        Some(true) => {}
        _ => return,
    }
    match deserializer.read_value(context) {
        Some(v) => rv.set(v),
        None => {
            // V8 has set a pending exception. Don't overwrite it.
        }
    }
}

// ---------------------------------------------------------------------------
// install_global
// ---------------------------------------------------------------------------

/// Install `structuredClone` on `globalThis`. Called from
/// `init::setup_globals`.
pub fn install_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    let f = v8::Function::new(scope, structured_clone_callback).unwrap();
    let key = v8::String::new(scope, "structuredClone").unwrap();
    global.set(scope, key.into(), f.into());
}
