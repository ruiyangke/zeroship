//! V8 private-symbol helpers for stream internal slots.
//!
//! Per design §V.3: each WHATWG-spec internal slot that must preserve JS
//! identity (e.g. `[[reader]]`, `[[controller]]`, `[[storedError]]`) lives
//! in a V8 private symbol on the wrapper object. These helpers wrap
//! `Private::for_api` access so callsites read like spec text.
//!
//! `Private::for_api(scope, Some(name))` is keyed by name within the
//! isolate, so repeated calls return the same private symbol — a slot
//! access is constant-time without any per-isolate cache.

/// Get-or-create the V8 private symbol for the given name. The same
/// `name` returns the same private within an isolate.
pub fn private_sym<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    name: &'static str,
) -> v8::Local<'s, v8::Private> {
    let key = v8::String::new(scope, name).unwrap();
    v8::Private::for_api(scope, Some(key))
}

/// Read a slot's current value. Returns `undefined` if the slot has
/// never been set (or was deleted).
///
/// Callers should treat `undefined` as "slot empty" — that's how the
/// spec's `IsEmpty([[…]])` checks render in our model.
pub fn read_slot<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    obj: v8::Local<v8::Object>,
    name: &'static str,
) -> v8::Local<'s, v8::Value> {
    let priv_ = private_sym(scope, name);
    obj.get_private(scope, priv_).unwrap_or_else(|| v8::undefined(scope).into())
}

/// Write a slot. Pass `undefined` to clear (analogous to deleting the
/// slot — `delete_slot` is the explicit form; both are observably
/// indistinguishable to spec algorithms that test `is_undefined()`).
pub fn write_slot(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    name: &'static str,
    value: v8::Local<v8::Value>,
) {
    let priv_ = private_sym(scope, name);
    obj.set_private(scope, priv_, value);
}

/// Delete a slot. After this call, `read_slot` returns `undefined`.
///
/// Used by `InvalidateBYOBRequest` (§3.7) and `releaseLock` (§3.4) which
/// the spec describes as "set X.[[Y]] to undefined" / "set X.[[Y]] to null".
pub fn delete_slot(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    name: &'static str,
) {
    let priv_ = private_sym(scope, name);
    obj.delete_private(scope, priv_);
}

/// Convenience: `true` if the slot is not currently set.
pub fn slot_is_empty(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    name: &'static str,
) -> bool {
    read_slot(scope, obj, name).is_undefined()
}

// ---------------------------------------------------------------------------
// Canonical slot names (spec § double-bracket slot → string literal)
// ---------------------------------------------------------------------------
//
// Centralising the strings here keeps spec call-sites readable and prevents
// typos from causing silent slot-mismatch bugs (a typo creates a different
// private, so the read returns undefined — which a spec algorithm would
// happily continue past).

/// `ReadableStream.[[reader]]` — the active default/BYOB reader, or
/// undefined if the stream is unlocked.
pub const READER: &str = "[[reader]]";

/// `ReadableStream.[[controller]]` — the controller wrapper. Set once
/// at construction.
pub const CONTROLLER: &str = "[[controller]]";

/// `ReadableStream.[[storedError]]` — the error reason, set when the
/// stream transitions to "errored". Always a JS value (objects, primitives).
pub const STORED_ERROR: &str = "[[storedError]]";

/// `WritableStream.[[writer]]` — the active writer, or undefined if
/// unlocked.
pub const WRITER: &str = "[[writer]]";

/// Generic mixin slot: `ReadableStreamGenericReader.[[stream]]` and
/// `WritableStreamDefaultWriter.[[stream]]` — the parent stream the
/// reader/writer is bound to. Set undefined on releaseLock.
pub const STREAM: &str = "[[stream]]";

/// Reader's `[[closedPromise]]` — the Promise returned by `reader.closed`.
pub const CLOSED_PROMISE: &str = "[[closedPromise]]";

/// Writer's `[[readyPromise]]` — backpressure-driven; resolves when the
/// stream is willing to accept more data.
pub const READY_PROMISE: &str = "[[readyPromise]]";

/// `TransformStream.[[readable]]` and `[[writable]]` — the public-side
/// stream wrappers exposed via the `readable` / `writable` getters.
pub const READABLE: &str = "[[readable]]";
pub const WRITABLE: &str = "[[writable]]";

/// `ReadableByteStreamController.[[byobRequest]]` — current pending
/// BYOB request wrapper, or undefined.
pub const BYOB_REQUEST: &str = "[[byobRequest]]";

/// `ReadableStreamBYOBRequest.[[view]]` — the user-supplied view that
/// the byte controller is currently filling.
pub const VIEW: &str = "[[view]]";
