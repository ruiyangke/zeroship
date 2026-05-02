//! Body consumer methods per Fetch §3.5
//! (https://fetch.spec.whatwg.org/#body-mixin).
//!
//! Six methods, all returning a Promise:
//!
//! - `text()` — UTF-8 decode (replace invalid bytes with U+FFFD).
//! - `json()` — UTF-8 decode + JSON.parse. Rejects with **SyntaxError**
//!   on parse fail (v2 fix MAJOR-25 — NOT TypeError).
//! - `arrayBuffer()` — return ArrayBuffer over the bytes. Rejects with
//!   **RangeError** if total exceeds 2GB (V8's `safe_integer` ceiling
//!   for ArrayBuffer length).
//! - `bytes()` — return Uint8Array over the same backing store.
//! - `blob()` — defer in v1 (no native Blob class). Throws TypeError.
//! - `formData()` — `application/x-www-form-urlencoded` only in v1;
//!   `multipart/form-data` is deferred (the multipart parser is the
//!   tricky part).
//!
//! ## Lock and disturb checks
//!
//! Per Fetch §3.5 step 1: "If this's body is null, return a promise
//! resolved with an empty byte sequence." — except for `text()` /
//! `json()` etc. which actually settle to the empty-buffer-decoded
//! values.
//!
//! Step 2: "If this's body is unusable (disturbed or locked), return
//! a promise rejected with a TypeError." We rely on
//! `body_stream::read_all_bytes` to surface the spec's lock check —
//! `getReader()` throws TypeError on a locked stream, which we
//! propagate as a rejected promise.
//!
//! ## Installer
//!
//! `install_body_methods_on_proto(scope, proto)` installs all six
//! methods on the given prototype object. Called by Request and
//! Response after they install their own methods.

use std::cell::RefCell;
use std::rc::Rc;

use super::body::Body;
use super::body_stream::read_all_bytes;

/// Maximum body size for `arrayBuffer()` — 2 GiB minus 1 byte. V8's
/// ArrayBuffer length is a `safe_integer` and the practical max for a
/// 32-bit indexed buffer is `i32::MAX`. Per MAJOR-25 the rejection on
/// overflow is `RangeError`, not TypeError.
pub const MAX_ARRAY_BUFFER_BYTES: usize = i32::MAX as usize;

// ---------------------------------------------------------------------------
// Public installer
// ---------------------------------------------------------------------------

/// Install the six body consumer methods on the given prototype. Called
/// by Request::install_global and Response::install_global with the
/// resolved class prototype object.
///
/// `body_kind` is "request" or "response" — used only for error
/// messages (e.g. "body already used"). The method bodies use a
/// generic Body<T: BodyMarker> path so the installer is shared.
pub fn install_body_methods<'s, T: Body + BodyMarker + 'static>(
    scope: &mut v8::PinScope<'s, '_>,
    proto: v8::Local<v8::Object>,
) {
    install_method(scope, proto, "text", consumer_text::<T>);
    install_method(scope, proto, "json", consumer_json::<T>);
    install_method(scope, proto, "arrayBuffer", consumer_array_buffer::<T>);
    install_method(scope, proto, "bytes", consumer_bytes::<T>);
    install_method(scope, proto, "blob", consumer_blob::<T>);
    install_method(scope, proto, "formData", consumer_form_data::<T>);

    // bodyUsed accessor — read-only getter on the prototype.
    install_body_used::<T>(scope, proto);
    // body accessor — read-only getter returning the stream (or null).
    install_body_getter::<T>(scope, proto);
}

/// Marker trait so the type parameter can be used in monomorphized
/// callbacks. `Body` itself is enough but Rust's type-system needs
/// the trait bounded for `'static` callback dispatch.
pub trait BodyMarker {
    /// Class label for error messages ("Request" / "Response").
    const CLASS_LABEL: &'static str;
}

fn install_method<'s, F>(
    scope: &mut v8::PinScope<'s, '_>,
    proto: v8::Local<v8::Object>,
    name: &str,
    cb: F,
) where
    F: v8::MapFnTo<v8::FunctionCallback>,
{
    let key = v8::String::new(scope, name).unwrap();
    let tmpl = v8::FunctionTemplate::new(scope, cb);
    let func = tmpl.get_function(scope).unwrap();
    proto.set(scope, key.into(), func.into());
}

// ---------------------------------------------------------------------------
// bodyUsed getter
// ---------------------------------------------------------------------------

fn install_body_used<'s, T: Body + BodyMarker + 'static>(
    scope: &mut v8::PinScope<'s, '_>,
    proto: v8::Local<v8::Object>,
) {
    let key = v8::String::new(scope, "bodyUsed").unwrap();
    let getter_tmpl = v8::FunctionTemplate::new(scope, body_used_getter::<T>);
    let getter_fn = getter_tmpl.get_function(scope).unwrap();
    let mut desc = v8::PropertyDescriptor::new_from_get_set(getter_fn.into(), v8::undefined(scope).into());
    desc.set_configurable(true);
    desc.set_enumerable(true);
    proto.define_property(scope, key.into(), &desc);
}

fn body_used_getter<T: Body + BodyMarker + 'static>(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    let Some(body) = T::body_state(scope, this) else {
        rv.set(v8::Boolean::new(scope, false).into());
        return;
    };
    // The body is "used" if its stream has been disturbed. We check
    // via the stream's public locked + a private "used" marker we
    // stash on the wrapper. Simpler: a body is unusable if the stream
    // has been read or locked-then-released. For v1 we report `used`
    // when the body's stream returns `locked === true` OR the stream
    // is disturbed. Disturbed isn't directly observable from JS in a
    // single getter, so we additionally check the wrapper's
    // `__zsBodyUsed` private symbol set by consumers.
    let used = match &body.stream {
        Some(stream_global) => {
            let stream = v8::Local::new(scope, stream_global.clone());
            stream_disturbed_or_used(scope, this, stream)
        }
        None => false,
    };
    rv.set(v8::Boolean::new(scope, used).into());
}

/// Check if the body's stream has been used. Combines:
///   - The streams crate's disturbed flag (true if the stream has been
///     read from — survives reader.releaseLock()),
///   - The stream's `locked` getter (true while a reader holds the lock),
///   - A private symbol marker `__zsBodyUsed` we set on the wrapper
///     when a consumer starts (so post-consume, after lock release,
///     we still report `bodyUsed === true`).
///
/// Per Fetch §3.5 + WPT request-init-stream.any.js: a stream that's
/// had `getReader().read().releaseLock()` is disturbed even though
/// `locked === false`. The disturbed flag on RSState is the
/// authoritative answer.
pub fn stream_disturbed_or_used(
    scope: &mut v8::PinScope,
    wrapper: v8::Local<v8::Object>,
    stream: v8::Local<v8::Object>,
) -> bool {
    if check_used_marker(scope, wrapper) {
        return true;
    }
    // Reach into the streams crate's RSState for the spec-faithful
    // disturbed flag. Falls through to the legacy locked-getter check
    // for non-native streams (e.g. tests that pass a Headers object
    // accidentally — defensively).
    if let Some(disturbed) =
        crate::streams::readable::with_rs_state(scope, stream, |s| s.disturbed.get())
    {
        if disturbed {
            return true;
        }
    }
    let key = v8::String::new(scope, "locked").unwrap();
    if let Some(v) = stream.get(scope, key.into()) {
        if v.boolean_value(scope) {
            return true;
        }
    }
    false
}

/// Set the body-used marker on the wrapper. Called at the start of
/// every consumer method so even if the read fails (e.g. JSON parse
/// error), `bodyUsed` reads true afterwards per Fetch §3.5 step 1.
pub fn set_body_used_marker(scope: &mut v8::PinScope, wrapper: v8::Local<v8::Object>) {
    let key = body_used_symbol(scope);
    let true_v = v8::Boolean::new(scope, true);
    wrapper.set(scope, key.into(), true_v.into());
}

fn check_used_marker(scope: &mut v8::PinScope, wrapper: v8::Local<v8::Object>) -> bool {
    let key = body_used_symbol(scope);
    if let Some(v) = wrapper.get(scope, key.into()) {
        return v.boolean_value(scope);
    }
    false
}

fn body_used_symbol<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Symbol> {
    let name = v8::String::new(scope, "__zsBodyUsed").unwrap();
    v8::Symbol::for_key(scope, name)
}

// ---------------------------------------------------------------------------
// body getter (returns the stream or null)
// ---------------------------------------------------------------------------

fn install_body_getter<'s, T: Body + BodyMarker + 'static>(
    scope: &mut v8::PinScope<'s, '_>,
    proto: v8::Local<v8::Object>,
) {
    let key = v8::String::new(scope, "body").unwrap();
    let getter_tmpl = v8::FunctionTemplate::new(scope, body_getter::<T>);
    let getter_fn = getter_tmpl.get_function(scope).unwrap();
    let mut desc = v8::PropertyDescriptor::new_from_get_set(getter_fn.into(), v8::undefined(scope).into());
    desc.set_configurable(true);
    desc.set_enumerable(true);
    proto.define_property(scope, key.into(), &desc);
}

fn body_getter<T: Body + BodyMarker + 'static>(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    let Some(body) = T::body_state(scope, this) else {
        rv.set(v8::null(scope).into());
        return;
    };
    match &body.stream {
        Some(g) => {
            let stream_local = v8::Local::new(scope, g.clone());
            rv.set(stream_local.into());
        }
        None => {
            rv.set(v8::null(scope).into());
        }
    }
}

// ---------------------------------------------------------------------------
// Consumer common path
// ---------------------------------------------------------------------------

/// Pre-flight checks shared by every consumer method:
///
///   - Brand-check failure → already-rejected promise (TypeError).
///   - `body === null` → empty body.
///   - `bodyUsed === true` → already-rejected promise (TypeError).
///
/// Lifetime-elided: takes scope without binding `'s`. Returns either
/// `Ok` with the empty/has-body classification, or `Err` with the
/// pending Global promise. The caller localizes the Global at the
/// top-level callback's scope.
///
/// FIX C: when the body has a `BodySource::Bytes(rc)` (or other
/// rewindable bytes variant) AND the stream has not been disturbed
/// or locked, we return `PreFlight::Bytes(rc)` — the consumer skips
/// the JS-visible reader.read() pump entirely and hands the bytes to
/// the per-kind decoder synchronously. Saves ~5 V8↔Rust hops + 3
/// Promise allocations per body consume on the fast path. Spec
/// behaviour matches: we still set the body-used marker AND flip
/// the stream's `disturbed` flag, so observers (`bodyUsed` getter,
/// subsequent `getReader()`) see the same state as if the stream
/// had been read.
fn pre_flight<T: Body + BodyMarker + 'static>(
    scope: &mut v8::PinScope,
    this: v8::Local<v8::Object>,
) -> Result<PreFlight, v8::Global<v8::Promise>> {
    let body = match T::body_state(scope, this) {
        Some(b) => b,
        None => {
            return Err(rejected_promise_global(
                scope,
                ErrorKind::Type,
                &format!("Illegal invocation on non-{} object", T::CLASS_LABEL),
            ));
        }
    };

    // Empty-body short-circuit.
    let stream_global = match &body.stream {
        Some(g) => g.clone(),
        None => {
            // Body is null even if a source is somehow present —
            // matches the original behaviour of returning empty bytes.
            return Ok(PreFlight::EmptyBody);
        }
    };

    // Snapshot the source for the fast-path probe. Cheap (Rc clone
    // on Bytes/Blob/UrlSearchParams/FormData; nothing on Stream/None).
    let source_snapshot = body.source.clone();

    // bodyUsed?
    let stream_local = v8::Local::new(scope, stream_global.clone());
    if stream_disturbed_or_used(scope, this, stream_local) {
        return Err(rejected_promise_global(
            scope,
            ErrorKind::Type,
            &format!("{} body has already been consumed", T::CLASS_LABEL),
        ));
    }

    // Mark used per Fetch §3.5 step 1 (set body's stream to disturbed).
    set_body_used_marker(scope, this);

    // Fast path: rewindable byte source + fresh stream → return the
    // bytes directly. Also flip the stream's `disturbed` flag so
    // `bodyUsed` reads true on the spec-compliant path too.
    if let Some(rc) = source_bytes_for_fast_path(source_snapshot) {
        // Best-effort disturb flag flip on the native stream. For
        // JS-built streams (no RSState in internal field 0) the
        // disturb flag isn't ours to flip; the wrapper symbol still
        // makes `bodyUsed` return true.
        let _ = crate::streams::readable::with_rs_state(scope, stream_local, |s| {
            s.disturbed.set(true);
        });
        return Ok(PreFlight::Bytes(rc));
    }

    Ok(PreFlight::HasBody { stream_global })
}

/// Returns Some(rc) when the body source is a rewindable byte
/// sequence safe to drain directly. Stream → None (no fast path).
fn source_bytes_for_fast_path(
    source: Option<crate::fetch_body::body::BodySource>,
) -> Option<Rc<Vec<u8>>> {
    use crate::fetch_body::body::BodySource;
    match source {
        Some(BodySource::Bytes(rc))
        | Some(BodySource::Blob(rc, _))
        | Some(BodySource::UrlSearchParams(rc))
        | Some(BodySource::FormData(rc, _)) => Some(rc),
        Some(BodySource::Stream) | None => None,
    }
}

enum PreFlight {
    /// Body is null — return empty bytes.
    EmptyBody,
    /// FIX C fast path: body has a rewindable byte source AND the
    /// stream is fresh. The consumer can drain directly without going
    /// through reader.read().
    Bytes(Rc<Vec<u8>>),
    /// Body has a stream that must be read via the JS reader (because
    /// the user-supplied a ReadableStream, or the source was already
    /// consumed and re-tee'd). Consumer falls back to read_all_bytes.
    HasBody {
        stream_global: v8::Global<v8::Object>,
    },
}

#[derive(Clone, Copy)]
enum ErrorKind {
    Type,
    Range,
    Syntax,
}

fn rejected_promise_global(
    scope: &mut v8::PinScope,
    kind: ErrorKind,
    msg: &str,
) -> v8::Global<v8::Promise> {
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let m = v8::String::new(scope, msg).unwrap();
    let exc = match kind {
        ErrorKind::Type => v8::Exception::type_error(scope, m),
        ErrorKind::Range => v8::Exception::range_error(scope, m),
        ErrorKind::Syntax => v8::Exception::syntax_error(scope, m),
    };
    resolver.reject(scope, exc);
    v8::Global::new(scope, promise)
}

// ---------------------------------------------------------------------------
// text()
// ---------------------------------------------------------------------------

fn consumer_text<T: Body + BodyMarker + 'static>(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    let pre = match pre_flight::<T>(scope, this) {
        Ok(p) => p,
        Err(promise_global) => {
            let promise = v8::Local::new(scope, promise_global);
            rv.set(promise.into());
            return;
        }
    };
    match pre {
        PreFlight::EmptyBody => {
            let resolver = v8::PromiseResolver::new(scope).unwrap();
            let promise = resolver.get_promise(scope);
            let empty = v8::String::new(scope, "").unwrap();
            resolver.resolve(scope, empty.into());
            rv.set(promise.into());
        }
        PreFlight::Bytes(rc) => {
            // FIX C fast path — UTF-8 decode the source bytes inline.
            let resolver = v8::PromiseResolver::new(scope).unwrap();
            let promise = resolver.get_promise(scope);
            let s = String::from_utf8_lossy(&rc).into_owned();
            let v = v8::String::new(scope, &s).unwrap();
            resolver.resolve(scope, v.into());
            rv.set(promise.into());
        }
        PreFlight::HasBody { stream_global } => {
            let stream_local = v8::Local::new(scope, stream_global);
            let bytes_promise = match read_all_bytes(scope, stream_local) {
                Ok(p) => p,
                Err(e) => {
                    let resolver = v8::PromiseResolver::new(scope).unwrap();
                    let promise = resolver.get_promise(scope);
                    let m = v8::String::new(scope, &e.message).unwrap();
                    let exc = v8::Exception::type_error(scope, m);
                    resolver.reject(scope, exc);
                    rv.set(promise.into());
                    return;
                }
            };
            // Chain: bytes_promise.then(bytes -> decode UTF-8 -> string)
            let outer = map_promise_with(scope, bytes_promise, MapKind::Text);
            rv.set(outer.into());
        }
    }
}

// ---------------------------------------------------------------------------
// json()
// ---------------------------------------------------------------------------

fn consumer_json<T: Body + BodyMarker + 'static>(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    let pre = match pre_flight::<T>(scope, this) {
        Ok(p) => p,
        Err(promise_global) => {
            let promise = v8::Local::new(scope, promise_global);
            rv.set(promise.into());
            return;
        }
    };
    match pre {
        PreFlight::EmptyBody => {
            // Empty body → JSON.parse("") → SyntaxError.
            let resolver = v8::PromiseResolver::new(scope).unwrap();
            let promise = resolver.get_promise(scope);
            let exc = build_syntax_error(scope, "Unexpected end of JSON input");
            resolver.reject(scope, exc);
            rv.set(promise.into());
        }
        PreFlight::Bytes(rc) => {
            // FIX C fast path — JSON.parse the source bytes inline.
            let resolver = v8::PromiseResolver::new(scope).unwrap();
            let promise = resolver.get_promise(scope);
            settle_json_from_bytes(scope, resolver, &rc);
            rv.set(promise.into());
        }
        PreFlight::HasBody { stream_global } => {
            let stream_local = v8::Local::new(scope, stream_global);
            let bytes_promise = match read_all_bytes(scope, stream_local) {
                Ok(p) => p,
                Err(e) => {
                    let resolver = v8::PromiseResolver::new(scope).unwrap();
                    let promise = resolver.get_promise(scope);
                    let m = v8::String::new(scope, &e.message).unwrap();
                    let exc = v8::Exception::type_error(scope, m);
                    resolver.reject(scope, exc);
                    rv.set(promise.into());
                    return;
                }
            };
            let outer = map_promise_with(scope, bytes_promise, MapKind::Json);
            rv.set(outer.into());
        }
    }
}

/// Parse `bytes` as UTF-8 then JSON, settle `resolver` accordingly.
/// Used by the FIX C fast path for `consumer_json`. Mirrors the
/// MapKind::Json branch in `settle_outer`.
fn settle_json_from_bytes(
    scope: &mut v8::PinScope,
    resolver: v8::Local<v8::PromiseResolver>,
    bytes: &[u8],
) {
    let s = String::from_utf8_lossy(bytes).into_owned();
    let json_str = match v8::String::new(scope, &s) {
        Some(v) => v,
        None => {
            let exc = build_syntax_error(scope, "Invalid JSON input string");
            resolver.reject(scope, exc);
            return;
        }
    };
    let parsed: Option<v8::Global<v8::Value>> = {
        v8::tc_scope!(let tc, scope);
        match v8::json::parse(tc, json_str) {
            Some(v) => Some(v8::Global::new(tc, v)),
            None => {
                let _ = tc.exception();
                None
            }
        }
    };
    match parsed {
        Some(g) => {
            let v = v8::Local::new(scope, g);
            resolver.resolve(scope, v);
        }
        None => {
            let exc = build_syntax_error(scope, "Invalid JSON in body");
            resolver.reject(scope, exc);
        }
    }
}

// ---------------------------------------------------------------------------
// arrayBuffer()
// ---------------------------------------------------------------------------

fn consumer_array_buffer<T: Body + BodyMarker + 'static>(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    let pre = match pre_flight::<T>(scope, this) {
        Ok(p) => p,
        Err(promise_global) => {
            let promise = v8::Local::new(scope, promise_global);
            rv.set(promise.into());
            return;
        }
    };
    match pre {
        PreFlight::EmptyBody => {
            let resolver = v8::PromiseResolver::new(scope).unwrap();
            let promise = resolver.get_promise(scope);
            let store = v8::ArrayBuffer::new_backing_store_from_vec(Vec::new()).make_shared();
            let ab = v8::ArrayBuffer::with_backing_store(scope, &store);
            resolver.resolve(scope, ab.into());
            rv.set(promise.into());
        }
        PreFlight::Bytes(rc) => {
            // FIX C fast path. Try to take ownership of the Vec
            // (cheap if no aliases — common for response bodies);
            // fall back to clone if other Rc holders exist.
            let resolver = v8::PromiseResolver::new(scope).unwrap();
            let promise = resolver.get_promise(scope);
            let bytes = match Rc::try_unwrap(rc) {
                Ok(v) => v,
                Err(rc) => (*rc).clone(),
            };
            if bytes.len() > MAX_ARRAY_BUFFER_BYTES {
                let exc = build_range_error(scope, "Body too large for ArrayBuffer (>2GB)");
                resolver.reject(scope, exc);
                rv.set(promise.into());
                return;
            }
            let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes).make_shared();
            let ab = v8::ArrayBuffer::with_backing_store(scope, &store);
            resolver.resolve(scope, ab.into());
            rv.set(promise.into());
        }
        PreFlight::HasBody { stream_global } => {
            let stream_local = v8::Local::new(scope, stream_global);
            let bytes_promise = match read_all_bytes(scope, stream_local) {
                Ok(p) => p,
                Err(e) => {
                    let resolver = v8::PromiseResolver::new(scope).unwrap();
                    let promise = resolver.get_promise(scope);
                    let m = v8::String::new(scope, &e.message).unwrap();
                    let exc = v8::Exception::type_error(scope, m);
                    resolver.reject(scope, exc);
                    rv.set(promise.into());
                    return;
                }
            };
            let outer = map_promise_with(scope, bytes_promise, MapKind::ArrayBuffer);
            rv.set(outer.into());
        }
    }
}

// ---------------------------------------------------------------------------
// bytes()
// ---------------------------------------------------------------------------

fn consumer_bytes<T: Body + BodyMarker + 'static>(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    let pre = match pre_flight::<T>(scope, this) {
        Ok(p) => p,
        Err(promise_global) => {
            let promise = v8::Local::new(scope, promise_global);
            rv.set(promise.into());
            return;
        }
    };
    match pre {
        PreFlight::EmptyBody => {
            let resolver = v8::PromiseResolver::new(scope).unwrap();
            let promise = resolver.get_promise(scope);
            let store = v8::ArrayBuffer::new_backing_store_from_vec(Vec::new()).make_shared();
            let ab = v8::ArrayBuffer::with_backing_store(scope, &store);
            let view = v8::Uint8Array::new(scope, ab, 0, 0).unwrap();
            resolver.resolve(scope, view.into());
            rv.set(promise.into());
        }
        PreFlight::Bytes(rc) => {
            // FIX C fast path.
            let resolver = v8::PromiseResolver::new(scope).unwrap();
            let promise = resolver.get_promise(scope);
            let bytes = match Rc::try_unwrap(rc) {
                Ok(v) => v,
                Err(rc) => (*rc).clone(),
            };
            if bytes.len() > MAX_ARRAY_BUFFER_BYTES {
                let exc = build_range_error(scope, "Body too large for Uint8Array (>2GB)");
                resolver.reject(scope, exc);
                rv.set(promise.into());
                return;
            }
            let len = bytes.len();
            let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes).make_shared();
            let ab = v8::ArrayBuffer::with_backing_store(scope, &store);
            let view = v8::Uint8Array::new(scope, ab, 0, len).unwrap();
            resolver.resolve(scope, view.into());
            rv.set(promise.into());
        }
        PreFlight::HasBody { stream_global } => {
            let stream_local = v8::Local::new(scope, stream_global);
            let bytes_promise = match read_all_bytes(scope, stream_local) {
                Ok(p) => p,
                Err(e) => {
                    let resolver = v8::PromiseResolver::new(scope).unwrap();
                    let promise = resolver.get_promise(scope);
                    let m = v8::String::new(scope, &e.message).unwrap();
                    let exc = v8::Exception::type_error(scope, m);
                    resolver.reject(scope, exc);
                    rv.set(promise.into());
                    return;
                }
            };
            let outer = map_promise_with(scope, bytes_promise, MapKind::Bytes);
            rv.set(outer.into());
        }
    }
}

// ---------------------------------------------------------------------------
// blob() — deferred in v1
// ---------------------------------------------------------------------------

fn consumer_blob<T: Body + BodyMarker + 'static>(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    // Per Fetch §3.5 step 1, every body consumer disturbs the body
    // even if the rest of the algorithm fails. We run pre_flight here
    // so that `request.bodyUsed` flips to true after the (rejecting)
    // call — matching WPT request-disturbed.any.js expectations and
    // workerd's behaviour. The pre_flight will reject early if the
    // body is null OR already disturbed; if the body is intact it
    // marks the wrapper used and we then synchronously reject with
    // TypeError because we ship no native Blob class in v1.
    let this = args.this();
    let pre = match pre_flight::<T>(scope, this) {
        Ok(p) => p,
        Err(promise_global) => {
            let promise = v8::Local::new(scope, promise_global);
            rv.set(promise.into());
            return;
        }
    };
    // Body is now marked used (or empty). Pre-flight may have
    // returned a Bytes fast-path payload — drop it; blob() rejects
    // with TypeError because we ship no native Blob class in v1.
    let _ = pre;
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let m = v8::String::new(scope, "blob() not yet implemented (Blob class deferred)").unwrap();
    let exc = v8::Exception::type_error(scope, m);
    resolver.reject(scope, exc);
    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// formData() — urlencoded only in v1
// ---------------------------------------------------------------------------

fn consumer_form_data<T: Body + BodyMarker + 'static>(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();

    // Look at Content-Type to choose the parser. v1 only handles
    // urlencoded; multipart raises a TypeError.
    let ct = T::content_type(scope, this).unwrap_or_default();
    let lower = ct.to_ascii_lowercase();
    if lower.contains("multipart/form-data") {
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let promise = resolver.get_promise(scope);
        let m = v8::String::new(
            scope,
            "formData() multipart/form-data parsing is deferred",
        )
        .unwrap();
        let exc = v8::Exception::type_error(scope, m);
        resolver.reject(scope, exc);
        rv.set(promise.into());
        return;
    }
    if !lower.contains("application/x-www-form-urlencoded") {
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let promise = resolver.get_promise(scope);
        let m = v8::String::new(
            scope,
            "formData() requires Content-Type to be application/x-www-form-urlencoded \
             (multipart/form-data deferred)",
        )
        .unwrap();
        let exc = v8::Exception::type_error(scope, m);
        resolver.reject(scope, exc);
        rv.set(promise.into());
        return;
    }

    let pre = match pre_flight::<T>(scope, this) {
        Ok(p) => p,
        Err(promise_global) => {
            let promise = v8::Local::new(scope, promise_global);
            rv.set(promise.into());
            return;
        }
    };
    match pre {
        PreFlight::EmptyBody => {
            // Empty urlencoded → empty FormData.
            let resolver = v8::PromiseResolver::new(scope).unwrap();
            let promise = resolver.get_promise(scope);
            let fd = build_empty_form_data(scope);
            resolver.resolve(scope, fd.into());
            rv.set(promise.into());
        }
        PreFlight::Bytes(rc) => {
            // FIX C fast path — parse urlencoded directly.
            let resolver = v8::PromiseResolver::new(scope).unwrap();
            let promise = resolver.get_promise(scope);
            let fd = build_empty_form_data(scope);
            let s = String::from_utf8_lossy(&rc).into_owned();
            for pair in s.split('&') {
                if pair.is_empty() {
                    continue;
                }
                let (name, value) = match pair.find('=') {
                    Some(i) => (&pair[..i], &pair[i + 1..]),
                    None => (pair, ""),
                };
                let name = url_decode_form(name);
                let value = url_decode_form(value);
                form_data_append(scope, fd, &name, &value);
            }
            resolver.resolve(scope, fd.into());
            rv.set(promise.into());
        }
        PreFlight::HasBody { stream_global } => {
            let stream_local = v8::Local::new(scope, stream_global);
            let bytes_promise = match read_all_bytes(scope, stream_local) {
                Ok(p) => p,
                Err(e) => {
                    let resolver = v8::PromiseResolver::new(scope).unwrap();
                    let promise = resolver.get_promise(scope);
                    let m = v8::String::new(scope, &e.message).unwrap();
                    let exc = v8::Exception::type_error(scope, m);
                    resolver.reject(scope, exc);
                    rv.set(promise.into());
                    return;
                }
            };
            let outer = map_promise_with(scope, bytes_promise, MapKind::UrlencodedFormData);
            rv.set(outer.into());
        }
    }
}

fn build_empty_form_data<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Object> {
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, "FormData").unwrap();
    let class_v = global.get(scope, key.into()).expect("FormData missing");
    let class_fn: v8::Local<v8::Function> = class_v.try_into().unwrap();
    class_fn.new_instance(scope, &[]).unwrap()
}

// ---------------------------------------------------------------------------
// Promise mapping — bytes promise -> outer typed promise
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum MapKind {
    Text,
    Json,
    ArrayBuffer,
    Bytes,
    UrlencodedFormData,
}

/// Chain a bytes promise into a typed outer promise. The bytes promise
/// resolves to an ArrayBuffer; we extract bytes and run the per-kind
/// transform.
fn map_promise_with<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    inner: v8::Local<'s, v8::Promise>,
    kind: MapKind,
) -> v8::Local<'s, v8::Promise> {
    let outer_resolver = v8::PromiseResolver::new(scope).unwrap();
    let outer_promise = outer_resolver.get_promise(scope);
    let outer_resolver_global = v8::Global::new(scope, outer_resolver);

    let state = MapState {
        outer_resolver: Rc::new(RefCell::new(Some(outer_resolver_global))),
        kind,
    };

    let on_fulfilled = build_map_fulfilled(scope, state.clone());
    let on_rejected = build_map_rejected(scope, state.clone());

    let then_key = v8::String::new(scope, "then").unwrap();
    let Some(then_v) = inner.get(scope, then_key.into()) else {
        return outer_promise;
    };
    let Ok(then_fn) = v8::Local::<v8::Function>::try_from(then_v) else {
        return outer_promise;
    };
    let args = [on_fulfilled.into(), on_rejected.into()];
    let _ = then_fn.call(scope, inner.into(), &args);
    outer_promise
}

struct MapState {
    outer_resolver: Rc<RefCell<Option<v8::Global<v8::PromiseResolver>>>>,
    kind: MapKind,
}

impl Clone for MapState {
    fn clone(&self) -> Self {
        MapState {
            outer_resolver: self.outer_resolver.clone(),
            kind: self.kind,
        }
    }
}

fn build_map_fulfilled<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: MapState,
) -> v8::Local<'s, v8::Function> {
    let boxed = Box::new(state);
    let raw = Box::into_raw(boxed) as *mut std::ffi::c_void;
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw);

    let tmpl = v8::FunctionTemplate::builder(map_fulfilled_callback)
        .data(ext.into())
        .build(scope);
    let func = tmpl.get_function(scope).unwrap();

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        func,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut MapState));
        }),
    );
    std::mem::forget(weak);

    func
}

fn build_map_rejected<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: MapState,
) -> v8::Local<'s, v8::Function> {
    let boxed = Box::new(state);
    let raw = Box::into_raw(boxed) as *mut std::ffi::c_void;
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw);

    let tmpl = v8::FunctionTemplate::builder(map_rejected_callback)
        .data(ext.into())
        .build(scope);
    let func = tmpl.get_function(scope).unwrap();

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        func,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut MapState));
        }),
    );
    std::mem::forget(weak);

    func
}

fn map_fulfilled_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let data = args.data();
    let Ok(ext) = v8::Local::<v8::External>::try_from(data) else {
        return;
    };
    let raw = ext.value() as *const MapState;
    if raw.is_null() {
        return;
    }
    // SAFETY: the External's pointer is stable until the finalizer
    // fires (after this callback completes V8 may eventually GC the
    // function, then the Box drops). We borrow read-only.
    let state: &MapState = unsafe { &*raw };

    let inner_value = args.get(0);
    // The inner promise resolves to an ArrayBuffer (from
    // body_stream::settle_with_bytes).
    let bytes = match v8::Local::<v8::ArrayBuffer>::try_from(inner_value) {
        Ok(ab) => ab_to_vec(ab),
        Err(_) => Vec::new(),
    };

    settle_outer(scope, state, bytes);
}

fn map_rejected_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let data = args.data();
    let Ok(ext) = v8::Local::<v8::External>::try_from(data) else {
        return;
    };
    let raw = ext.value() as *const MapState;
    if raw.is_null() {
        return;
    }
    let state: &MapState = unsafe { &*raw };

    let reason = args.get(0);
    if let Some(resolver_global) = state.outer_resolver.borrow_mut().take() {
        let resolver = v8::Local::new(scope, resolver_global);
        resolver.reject(scope, reason);
    }
}

fn settle_outer(scope: &mut v8::PinScope, state: &MapState, bytes: Vec<u8>) {
    let Some(resolver_global) = state.outer_resolver.borrow_mut().take() else {
        return;
    };
    let resolver = v8::Local::new(scope, resolver_global);

    match state.kind {
        MapKind::Text => {
            let s = String::from_utf8_lossy(&bytes).into_owned();
            let v = v8::String::new(scope, &s).unwrap();
            resolver.resolve(scope, v.into());
        }
        MapKind::Json => {
            let s = String::from_utf8_lossy(&bytes).into_owned();
            let json_str = match v8::String::new(scope, &s) {
                Some(v) => v,
                None => {
                    let exc = build_syntax_error(scope, "Invalid JSON input string");
                    resolver.reject(scope, exc);
                    return;
                }
            };
            // v8::json::parse returns None and leaves the exception on
            // the isolate. Use a tc_scope to convert that exception
            // into a SyntaxError-shaped rejection per MAJOR-25.
            let parsed: Option<v8::Global<v8::Value>> = {
                v8::tc_scope!(let tc, scope);
                match v8::json::parse(tc, json_str) {
                    Some(v) => Some(v8::Global::new(tc, v)),
                    None => {
                        // Suppress the V8 exception and re-throw as
                        // SyntaxError below. Reading exception() clears
                        // the pending state inside this tc_scope.
                        let _ = tc.exception();
                        None
                    }
                }
            };
            match parsed {
                Some(g) => {
                    let v = v8::Local::new(scope, g);
                    resolver.resolve(scope, v);
                }
                None => {
                    let exc = build_syntax_error(scope, "Invalid JSON in body");
                    resolver.reject(scope, exc);
                }
            }
        }
        MapKind::ArrayBuffer => {
            if bytes.len() > MAX_ARRAY_BUFFER_BYTES {
                let exc = build_range_error(
                    scope,
                    "Body too large for ArrayBuffer (>2GB)",
                );
                resolver.reject(scope, exc);
                return;
            }
            let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes).make_shared();
            let ab = v8::ArrayBuffer::with_backing_store(scope, &store);
            resolver.resolve(scope, ab.into());
        }
        MapKind::Bytes => {
            if bytes.len() > MAX_ARRAY_BUFFER_BYTES {
                let exc = build_range_error(
                    scope,
                    "Body too large for Uint8Array (>2GB)",
                );
                resolver.reject(scope, exc);
                return;
            }
            let len = bytes.len();
            let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes).make_shared();
            let ab = v8::ArrayBuffer::with_backing_store(scope, &store);
            let view = v8::Uint8Array::new(scope, ab, 0, len).unwrap();
            resolver.resolve(scope, view.into());
        }
        MapKind::UrlencodedFormData => {
            let fd = build_empty_form_data(scope);
            // Parse `a=b&c=d` style.
            let s = String::from_utf8_lossy(&bytes).into_owned();
            for pair in s.split('&') {
                if pair.is_empty() {
                    continue;
                }
                let (name, value) = match pair.find('=') {
                    Some(i) => (&pair[..i], &pair[i + 1..]),
                    None => (pair, ""),
                };
                let name = url_decode_form(name);
                let value = url_decode_form(value);
                form_data_append(scope, fd, &name, &value);
            }
            resolver.resolve(scope, fd.into());
        }
    }
}

fn ab_to_vec(ab: v8::Local<v8::ArrayBuffer>) -> Vec<u8> {
    let len = ab.byte_length();
    if len == 0 {
        return Vec::new();
    }
    let mut out = vec![0u8; len];
    let store = ab.get_backing_store();
    unsafe {
        let src = store.data().expect("backing store").as_ptr() as *const u8;
        std::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), len);
    }
    out
}

fn form_data_append(
    scope: &mut v8::PinScope,
    fd: v8::Local<v8::Object>,
    name: &str,
    value: &str,
) {
    let key = v8::String::new(scope, "append").unwrap();
    let Some(fn_v) = fd.get(scope, key.into()) else {
        return;
    };
    let Ok(fn_l) = v8::Local::<v8::Function>::try_from(fn_v) else {
        return;
    };
    let n = v8::String::new(scope, name).unwrap();
    let v = v8::String::new(scope, value).unwrap();
    let args = [n.into(), v.into()];
    let _ = fn_l.call(scope, fd.into(), &args);
}

fn url_decode_form(s: &str) -> String {
    let bytes: Vec<u8> = s.replace('+', " ").into_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) =
                (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2]))
            {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn build_syntax_error<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    msg: &str,
) -> v8::Local<'s, v8::Value> {
    let m = v8::String::new(scope, msg).unwrap();
    v8::Exception::syntax_error(scope, m)
}

fn build_range_error<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    msg: &str,
) -> v8::Local<'s, v8::Value> {
    let m = v8::String::new(scope, msg).unwrap();
    v8::Exception::range_error(scope, m)
}
