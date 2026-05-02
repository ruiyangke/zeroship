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
            return Ok(PreFlight::EmptyBody);
        }
    };

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

    Ok(PreFlight::HasBody { stream_global })
}

enum PreFlight {
    /// Body is null — return empty bytes.
    EmptyBody,
    /// Body has a stream; consumer should read it.
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
// blob() — returns a native Blob whose type comes from Content-Type
// ---------------------------------------------------------------------------

fn consumer_blob<T: Body + BodyMarker + 'static>(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    // Per Fetch §3.5 step 1, every body consumer disturbs the body
    // even if the rest of the algorithm fails. The pre_flight checks
    // body-null/already-used and marks `bodyUsed` true on success.
    let this = args.this();
    // Read the Content-Type before pre_flight (cheap and unaffected by
    // bodyUsed marker) — this becomes the resulting Blob's `type` per
    // Fetch §3.5 "blob" step 4.
    let content_type = T::content_type(scope, this).unwrap_or_default();

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
            // No bytes — resolve with an empty Blob whose type matches
            // the (possibly empty) Content-Type.
            let resolver = v8::PromiseResolver::new(scope).unwrap();
            let promise = resolver.get_promise(scope);
            let blob =
                crate::blob_native::blob::create_blob(scope, Vec::new(), &content_type);
            resolver.resolve(scope, blob);
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
            let outer = map_promise_with(
                scope,
                bytes_promise,
                MapKind::Blob {
                    content_type,
                },
            );
            rv.set(outer.into());
        }
    }
}

// ---------------------------------------------------------------------------
// formData() — urlencoded + multipart/form-data (RFC 7578)
// ---------------------------------------------------------------------------

fn consumer_form_data<T: Body + BodyMarker + 'static>(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();

    // Look at Content-Type to choose the parser. We support two MIME
    // types per Fetch §3.5 "formData":
    //   * application/x-www-form-urlencoded → key=value pairs
    //   * multipart/form-data; boundary=... → RFC 7578 parts
    let ct = T::content_type(scope, this).unwrap_or_default();
    let lower = ct.to_ascii_lowercase();
    let is_multipart = lower.starts_with("multipart/form-data")
        || lower.contains("; boundary=")
        || (lower.contains("multipart/form-data") && extract_boundary(&ct).is_some());

    let kind: MapKind = if is_multipart {
        match extract_boundary(&ct) {
            Some(b) => MapKind::MultipartFormData { boundary: b },
            None => {
                let resolver = v8::PromiseResolver::new(scope).unwrap();
                let promise = resolver.get_promise(scope);
                let m = v8::String::new(
                    scope,
                    "formData(): multipart/form-data Content-Type missing boundary parameter",
                )
                .unwrap();
                let exc = v8::Exception::type_error(scope, m);
                resolver.reject(scope, exc);
                rv.set(promise.into());
                return;
            }
        }
    } else if lower.contains("application/x-www-form-urlencoded") {
        MapKind::UrlencodedFormData
    } else {
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let promise = resolver.get_promise(scope);
        let m = v8::String::new(
            scope,
            "formData() requires Content-Type to be application/x-www-form-urlencoded \
             or multipart/form-data",
        )
        .unwrap();
        let exc = v8::Exception::type_error(scope, m);
        resolver.reject(scope, exc);
        rv.set(promise.into());
        return;
    };

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
            // Empty body — for urlencoded resolve with an empty
            // FormData; for multipart reject with TypeError because
            // an empty buffer has no parts (matches WPT
            // request-consume-empty.any.js "with correct multipart
            // type (error case)").
            let resolver = v8::PromiseResolver::new(scope).unwrap();
            let promise = resolver.get_promise(scope);
            match kind {
                MapKind::UrlencodedFormData => {
                    let fd = build_empty_form_data(scope);
                    resolver.resolve(scope, fd.into());
                }
                MapKind::MultipartFormData { .. } => {
                    let m = v8::String::new(
                        scope,
                        "formData() multipart parsing failed: body is empty",
                    )
                    .unwrap();
                    let exc = v8::Exception::type_error(scope, m);
                    resolver.reject(scope, exc);
                }
                _ => {
                    // Unreachable: kind is constructed above as
                    // urlencoded or multipart.
                    let fd = build_empty_form_data(scope);
                    resolver.resolve(scope, fd.into());
                }
            }
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
            let outer = map_promise_with(scope, bytes_promise, kind);
            rv.set(outer.into());
        }
    }
}

/// Parse the `boundary=` parameter out of a `multipart/form-data;
/// boundary=...` Content-Type. Per RFC 2046 §5.1.1, a boundary is up
/// to 70 chars from a restricted ASCII set, optionally double-quoted.
fn extract_boundary(content_type: &str) -> Option<String> {
    // Look for `boundary=` or `boundary =` (loose whitespace).
    // Case-insensitive parameter name per RFC 7231 §3.1.1.1.
    let bytes = content_type.as_bytes();
    let lower = content_type.to_ascii_lowercase();
    let key = "boundary";
    let mut idx = 0;
    while idx + key.len() <= lower.len() {
        if lower[idx..idx + key.len()].eq(key) {
            // Verify boundary at parameter position (after a `;` or
            // start-of-string, followed by optional whitespace then
            // `=`). Cheap approximation: look for `=` after this.
            let mut j = idx + key.len();
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'=' {
                j += 1;
                while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                    j += 1;
                }
                let (start, quoted) = if j < bytes.len() && bytes[j] == b'"' {
                    (j + 1, true)
                } else {
                    (j, false)
                };
                let mut end = start;
                if quoted {
                    while end < bytes.len() && bytes[end] != b'"' {
                        end += 1;
                    }
                } else {
                    while end < bytes.len() && bytes[end] != b';' && bytes[end] != b' ' && bytes[end] != b'\t' {
                        end += 1;
                    }
                }
                if end > start {
                    return Some(content_type[start..end].to_string());
                }
                return None;
            }
        }
        idx += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// Multipart parser (RFC 7578)
// ---------------------------------------------------------------------------

/// One parsed multipart entry: either a text field or a file field.
enum MultipartEntry {
    Text {
        name: String,
        value: String,
    },
    File {
        name: String,
        filename: String,
        content_type: String,
        bytes: Vec<u8>,
    },
}

/// Parse RFC 7578 multipart/form-data bytes. Returns one `MultipartEntry`
/// per part. The expected wire format is:
///
/// ```text
/// --<boundary>\r\n
/// Content-Disposition: form-data; name="..."[; filename="..."]\r\n
/// [Content-Type: ...\r\n]
/// \r\n
/// <bytes>\r\n
/// --<boundary>\r\n
/// ...
/// --<boundary>--\r\n
/// ```
///
/// We accept LF-only line endings as a robustness measure (mirrors
/// curl, browsers). The parser is small and focused on the test
/// fixtures the platform actually emits — it's not a full RFC 7578
/// validator.
fn parse_multipart(bytes: &[u8], boundary: &str) -> Result<Vec<MultipartEntry>, String> {
    // The opening delimiter is `--<boundary>` at the start of a line.
    // Between parts we have `\r\n--<boundary>\r\n` (or LF-only).
    // The terminator is `--<boundary>--`.
    let dash_boundary: Vec<u8> = {
        let mut v = Vec::with_capacity(2 + boundary.len());
        v.extend_from_slice(b"--");
        v.extend_from_slice(boundary.as_bytes());
        v
    };

    // Find the first boundary occurrence — anything before it is
    // preamble per RFC 2046 §5.1.1 and is ignored.
    let first = match find_subsequence(bytes, &dash_boundary) {
        Some(i) => i,
        None => return Err("multipart: opening boundary not found".to_string()),
    };

    let mut entries: Vec<MultipartEntry> = Vec::new();
    let mut cursor = first + dash_boundary.len();
    loop {
        // After a boundary marker we expect either:
        //   "--" (terminator) followed by optional CRLF/LF/EOF, or
        //   CRLF/LF (next part header) or end-of-buffer.
        if cursor + 2 <= bytes.len() && &bytes[cursor..cursor + 2] == b"--" {
            // Terminator. We're done — anything after is epilogue.
            break;
        }
        // Skip the optional whitespace after the boundary (per RFC 2046
        // some implementations include LWSP-char) and the CRLF/LF.
        cursor = skip_optional_whitespace(bytes, cursor);
        cursor = skip_line_ending(bytes, cursor);

        // Parse headers until empty line.
        let mut name: Option<String> = None;
        let mut filename: Option<String> = None;
        let mut content_type: String = String::new();
        loop {
            let line_end = match find_line_ending(bytes, cursor) {
                Some(p) => p,
                None => return Err("multipart: unterminated headers".to_string()),
            };
            if line_end == cursor {
                // Empty line — end of headers.
                cursor = skip_line_ending(bytes, cursor);
                break;
            }
            let header_line = std::str::from_utf8(&bytes[cursor..line_end])
                .map_err(|_| "multipart: non-UTF-8 header line".to_string())?;
            cursor = skip_line_ending(bytes, line_end);

            // Split at the first colon.
            let (h_name, h_value) = match header_line.find(':') {
                Some(i) => (header_line[..i].trim(), header_line[i + 1..].trim()),
                None => continue, // skip malformed header lines defensively
            };
            let h_name_l = h_name.to_ascii_lowercase();
            if h_name_l == "content-disposition" {
                // Parse `form-data; name="..."[; filename="..."]`.
                let (n, fname) = parse_content_disposition(h_value);
                name = n;
                filename = fname;
            } else if h_name_l == "content-type" {
                content_type = h_value.to_string();
            }
        }

        let name = match name {
            Some(n) => n,
            None => {
                return Err(
                    "multipart: part missing Content-Disposition name= parameter".to_string()
                )
            }
        };

        // Body of the part runs until the next CRLF/LF + "--<boundary>".
        // We search for `\r\n--<boundary>` (preferred) and fall back to
        // `\n--<boundary>` if the input uses LF-only line endings.
        let next_boundary = find_part_boundary(bytes, cursor, &dash_boundary)
            .ok_or_else(|| "multipart: terminating boundary not found".to_string())?;
        let body = bytes[cursor..next_boundary.body_end].to_vec();
        cursor = next_boundary.boundary_end;

        // Build the entry. Spec §3.5 of HTML form encoding: a part
        // with `filename=` is a File-valued entry; without filename
        // it's a String-valued entry.
        match filename {
            Some(fname) => {
                entries.push(MultipartEntry::File {
                    name,
                    filename: fname,
                    content_type,
                    bytes: body,
                });
            }
            None => {
                let value = match std::str::from_utf8(&body) {
                    Ok(s) => s.to_string(),
                    Err(_) => String::from_utf8_lossy(&body).into_owned(),
                };
                entries.push(MultipartEntry::Text { name, value });
            }
        }
    }

    Ok(entries)
}

/// Result of locating the next `--<boundary>` after a part body.
/// `body_end` points at the CR (or LF) before the boundary; `boundary_end`
/// is the byte AFTER `--<boundary>` so the caller can resume parsing.
struct PartBoundary {
    body_end: usize,
    boundary_end: usize,
}

fn find_part_boundary(bytes: &[u8], start: usize, dash_boundary: &[u8]) -> Option<PartBoundary> {
    // Search for `\r\n` + dash_boundary first; fallback to `\n` + dash_boundary.
    let mut i = start;
    while i + dash_boundary.len() < bytes.len() {
        // Try CRLF-prefixed boundary.
        if bytes[i] == b'\r'
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'\n'
            && i + 2 + dash_boundary.len() <= bytes.len()
            && &bytes[i + 2..i + 2 + dash_boundary.len()] == dash_boundary
        {
            return Some(PartBoundary {
                body_end: i,
                boundary_end: i + 2 + dash_boundary.len(),
            });
        }
        // Fallback: LF-prefixed boundary (used by some non-conforming impls).
        if bytes[i] == b'\n'
            && i + 1 + dash_boundary.len() <= bytes.len()
            && &bytes[i + 1..i + 1 + dash_boundary.len()] == dash_boundary
        {
            return Some(PartBoundary {
                body_end: i,
                boundary_end: i + 1 + dash_boundary.len(),
            });
        }
        i += 1;
    }
    None
}

/// Parse the value of a Content-Disposition header (without the name).
/// Returns `(name, filename)`. We expect the disposition type to be
/// `form-data` and ignore other parameters. Quoted-string values are
/// supported; bare tokens are too.
fn parse_content_disposition(value: &str) -> (Option<String>, Option<String>) {
    // value e.g. `form-data; name="a"; filename="b.txt"`
    let mut name = None;
    let mut filename = None;
    // Tokenize on `;` then split each on `=`.
    for raw_param in value.split(';').skip(1) {
        let param = raw_param.trim();
        let (k, v) = match param.find('=') {
            Some(i) => (param[..i].trim(), param[i + 1..].trim()),
            None => continue,
        };
        let k_l = k.to_ascii_lowercase();
        // Strip surrounding double quotes if present.
        let v_unquoted: String = if v.starts_with('"') && v.ends_with('"') && v.len() >= 2 {
            // Note: per RFC 7578 §4.2, the only escape is `\"`. We do
            // NOT decode those for simplicity; the senders we test
            // against don't emit them.
            v[1..v.len() - 1].to_string()
        } else {
            v.to_string()
        };
        if k_l == "name" {
            name = Some(percent_decode_form_field(&v_unquoted));
        } else if k_l == "filename" {
            filename = Some(percent_decode_form_field(&v_unquoted));
        }
    }
    (name, filename)
}

/// Reverse the limited percent-encoding done in
/// `extract::serialize_form_data_multipart` (which encodes `"`, `\r`,
/// `\n` as `%22`, `%0D`, `%0A`). All other bytes pass through.
fn percent_decode_form_field(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
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

/// Find the index where the line ending starts (`\r` of `\r\n`, or
/// `\n`). Returns None if no line ending is found.
fn find_line_ending(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    while i < bytes.len() {
        if bytes[i] == b'\r' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
            return Some(i);
        }
        if bytes[i] == b'\n' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Skip past a line ending starting at `start`. Returns the index of
/// the byte AFTER the ending. If `start` doesn't sit on a line ending
/// it returns `start` unchanged.
fn skip_line_ending(bytes: &[u8], start: usize) -> usize {
    if start < bytes.len() && bytes[start] == b'\r' && start + 1 < bytes.len() && bytes[start + 1] == b'\n' {
        return start + 2;
    }
    if start < bytes.len() && bytes[start] == b'\n' {
        return start + 1;
    }
    start
}

/// Skip ASCII whitespace ` ` and `\t` starting at `start`.
fn skip_optional_whitespace(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    i
}

/// Naïve subsequence search: returns the index of the first occurrence
/// of `needle` in `haystack`, or None.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    let last = haystack.len() - needle.len();
    let mut i = 0;
    while i <= last {
        if &haystack[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
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

#[derive(Clone)]
enum MapKind {
    Text,
    Json,
    ArrayBuffer,
    Bytes,
    UrlencodedFormData,
    /// `body.blob()` — content_type is the value of the body's
    /// Content-Type header, becomes the Blob's `type` (after Blob's
    /// own normalize step lowercases printable-ASCII).
    Blob {
        content_type: String,
    },
    /// `body.formData()` parsed as multipart/form-data with the given
    /// boundary. The parser walks the byte buffer and emits a FormData
    /// with String entries for text parts and File entries for parts
    /// with a `filename=` Content-Disposition param.
    MultipartFormData {
        boundary: String,
    },
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
            kind: self.kind.clone(),
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

    match &state.kind {
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
        MapKind::Blob { content_type } => {
            // Construct a native Blob whose `type` is the body's
            // Content-Type. The Blob constructor's normalize step
            // lowercases printable-ASCII content.
            let blob = crate::blob_native::blob::create_blob(scope, bytes, content_type);
            resolver.resolve(scope, blob);
        }
        MapKind::MultipartFormData { boundary } => {
            // Parse RFC 7578 multipart/form-data into a FormData. Text
            // parts (no filename) become String entries; parts with a
            // filename become File entries.
            match parse_multipart(&bytes, boundary) {
                Ok(parsed) => {
                    let fd = build_empty_form_data(scope);
                    for part in parsed {
                        match part {
                            MultipartEntry::Text { name, value } => {
                                form_data_append(scope, fd, &name, &value);
                            }
                            MultipartEntry::File {
                                name,
                                filename,
                                content_type,
                                bytes,
                            } => {
                                form_data_append_file(
                                    scope,
                                    fd,
                                    &name,
                                    bytes,
                                    filename,
                                    &content_type,
                                );
                            }
                        }
                    }
                    resolver.resolve(scope, fd.into());
                }
                Err(msg) => {
                    let m = v8::String::new(scope, &msg).unwrap();
                    let exc = v8::Exception::type_error(scope, m);
                    resolver.reject(scope, exc);
                }
            }
        }
    }
}

/// Append a File-typed entry to a FormData via its prototype `append`.
/// We construct the File via `blob_native::file::create_file` and
/// call `append(name, file)` with two args (the spec says the filename
/// is already part of the File).
fn form_data_append_file(
    scope: &mut v8::PinScope,
    fd: v8::Local<v8::Object>,
    name: &str,
    bytes: Vec<u8>,
    filename: String,
    content_type: &str,
) {
    let file = crate::blob_native::file::create_file(scope, bytes, filename, content_type);
    let key = v8::String::new(scope, "append").unwrap();
    let Some(fn_v) = fd.get(scope, key.into()) else {
        return;
    };
    let Ok(fn_l) = v8::Local::<v8::Function>::try_from(fn_v) else {
        return;
    };
    let n = v8::String::new(scope, name).unwrap();
    let args = [n.into(), file];
    let _ = fn_l.call(scope, fd.into(), &args);
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
