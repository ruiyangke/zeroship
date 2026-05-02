//! Native `Request` class per WHATWG Fetch §5.4
//! (https://fetch.spec.whatwg.org/#request-class).
//!
//! Replaces the JS Request constructor that lives in `embed/fetch.js`.
//! Per design fetch-native v2 §IV the constructor walks the spec's
//! 43 steps and produces a Request whose body is a native BodyImpl
//! and whose headers come from the native Headers class (already
//! installed unconditionally in setup_globals).
//!
//! ## Storage layout (D-2 audit)
//!
//! Box<RequestState> in V8 internal field 0:
//!   - body: BodyImpl (the two-headed body)
//!   - method: String (uppercase for standard methods; preserved for
//!     non-standard like PATCH per Fetch §4.3 step 13)
//!   - url: String (parsed via ada-url; the parser-cleaned form)
//!   - headers: Global<Object> — the Headers instance
//!     (`[SameObject]` per Fetch §5.4 — same Headers across reads of
//!     `request.headers`)
//!   - signal: Option<Global<Object>> — AbortSignal chain (D-9)
//!   - destination, referrer, referrer_policy, mode, credentials,
//!     cache, redirect, integrity, keepalive, is_reload_navigation,
//!     is_history_navigation, duplex, priority — the spec attributes
//!     stored as their default values from §4.3.
//!
//! ## Body trait + consumer methods
//!
//! `impl Body for Request` provides `body_state` / `content_type`.
//! After the macro emits the constructor + getters, we install the
//! six body methods (text/json/arrayBuffer/bytes/blob/formData) on
//! the prototype via `install_body_methods::<Request>(...)`. Per v2
//! Process-8 the Body mixin is a Rust trait, not a V8 base class.

use std::cell::RefCell;

use crate::fetch_body::body::{Body, BodyImpl};
use crate::fetch_body::consumers::{install_body_methods, BodyMarker};
use crate::fetch_body::extract::extract_body;

/// Synthetic base URL used when `new Request(input)` receives a
/// relative URL or an empty string. Server-side runtimes (workerd,
/// Deno workers) follow the same convention since there is no
/// document.baseURI / Window.location to source the spec's "API base
/// URL" from. Matches workerd's default.
const DEFAULT_BASE_URL: &str = "http://localhost/";

// ---------------------------------------------------------------------------
// RequestState — boxed state stored in V8 internal field 0
// ---------------------------------------------------------------------------

/// The boxed Rust state for a Request wrapper. Mutable fields live in
/// `RefCell` so getter callbacks can hand out read-only views without
/// cloning, while constructor / future setter paths can mutate.
#[allow(missing_debug_implementations)]
pub struct RequestState {
    pub body: RefCell<BodyImpl>,
    pub method: RefCell<String>,
    pub url: RefCell<String>,
    /// Headers Global so re-reads of `request.headers` return the SAME
    /// JS object per Fetch §5.4 `[SameObject]`.
    pub headers: RefCell<Option<v8::Global<v8::Object>>>,
    /// AbortSignal Global. Always present per spec — `request.signal`
    /// returns a fresh signal even when the user didn't pass one. We
    /// lazily mint on first access if none was provided.
    pub signal: RefCell<Option<v8::Global<v8::Object>>>,
    /// Other Request attributes — strings rather than enums to keep
    /// the v1 surface small (no enum validation for v1; matches the
    /// polyfill's permissive behaviour).
    pub destination: RefCell<String>,
    pub referrer: RefCell<String>,
    pub referrer_policy: RefCell<String>,
    pub mode: RefCell<String>,
    pub credentials: RefCell<String>,
    pub cache: RefCell<String>,
    pub redirect: RefCell<String>,
    pub integrity: RefCell<String>,
    pub keepalive: RefCell<bool>,
    pub is_reload_navigation: RefCell<bool>,
    pub is_history_navigation: RefCell<bool>,
    pub duplex: RefCell<String>,
    pub priority: RefCell<String>,
}

impl Default for RequestState {
    fn default() -> Self {
        RequestState {
            body: RefCell::new(BodyImpl::null()),
            method: RefCell::new("GET".to_string()),
            url: RefCell::new(String::new()),
            headers: RefCell::new(None),
            signal: RefCell::new(None),
            destination: RefCell::new(String::new()),
            referrer: RefCell::new("about:client".to_string()),
            referrer_policy: RefCell::new(String::new()),
            mode: RefCell::new("cors".to_string()),
            credentials: RefCell::new("same-origin".to_string()),
            cache: RefCell::new("default".to_string()),
            redirect: RefCell::new("follow".to_string()),
            integrity: RefCell::new(String::new()),
            keepalive: RefCell::new(false),
            is_reload_navigation: RefCell::new(false),
            is_history_navigation: RefCell::new(false),
            duplex: RefCell::new("half".to_string()),
            priority: RefCell::new("auto".to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Body trait impl
// ---------------------------------------------------------------------------

/// Newtype the `#[v8_class]` macro recognises. The boxed state lives
/// at internal field 0; we project read-only references to its
/// `BodyImpl` for the consumer methods.
pub struct Request;

impl BodyMarker for Request {
    const CLASS_LABEL: &'static str = "Request";
}

impl Body for Request {
    fn body_state<'a>(
        scope: &mut v8::PinScope,
        this: v8::Local<v8::Object>,
    ) -> Option<&'a BodyImpl> {
        let raw = state_ptr(scope, this)?;
        // SAFETY: pointer stable for the lifetime of the wrapper. We
        // hand out a `&BodyImpl` whose lifetime is constrained by the
        // caller (typically the duration of a single V8 callback).
        let state: &RequestState = unsafe { &*raw };
        // Borrow the RefCell read-only and project to the inner. The
        // returned reference outlives the borrow — sound only because
        // BodyImpl fields live in their own allocations (Globals,
        // Option, u64). The caller must NOT call any mutator on the
        // RefCell during use; consumers don't.
        let cell = state.body.borrow();
        let ptr: *const BodyImpl = &*cell;
        drop(cell);
        Some(unsafe { &*ptr })
    }

    fn content_type(
        scope: &mut v8::PinScope,
        this: v8::Local<v8::Object>,
    ) -> Option<String> {
        let raw = state_ptr(scope, this)?;
        let state: &RequestState = unsafe { &*raw };
        let h_global = state.headers.borrow().as_ref()?.clone();
        read_content_type(scope, h_global)
    }
}

fn read_content_type(
    scope: &mut v8::PinScope,
    headers_global: v8::Global<v8::Object>,
) -> Option<String> {
    let headers_obj = v8::Local::new(scope, headers_global);
    let key = v8::String::new(scope, "get").unwrap();
    let fn_v = headers_obj.get(scope, key.into())?;
    let fn_l: v8::Local<v8::Function> = fn_v.try_into().ok()?;
    let arg = v8::String::new(scope, "Content-Type").unwrap();
    let result = fn_l.call(scope, headers_obj.into(), &[arg.into()])?;
    if result.is_null_or_undefined() {
        return None;
    }
    Some(result.to_rust_string_lossy(scope))
}

fn state_ptr(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>) -> Option<*mut RequestState> {
    let ext = obj
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())?;
    let ptr = ext.value() as *mut RequestState;
    if ptr.is_null() {
        return None;
    }
    Some(ptr)
}

// ---------------------------------------------------------------------------
// Class wiring — fully hand-rolled
// ---------------------------------------------------------------------------
//
// We don't use the `#[v8_class]` macro for Request because:
//
//   1. The macro's constructor codegen requires `Self: Default` and
//      stores `Box<Self>` in internal field 0 — but we store
//      `Box<RequestState>` (Self is the unit-ish marker type we use
//      for the Body trait dispatch).
//   2. The `headers`/`signal` getters need [SameObject] semantics,
//      which means returning a stored `v8::Global<v8::Object>` — the
//      macro doesn't know how to project a `Global` from internal
//      state.
//   3. Body consumers are installed via the shared trait dispatch
//      (`install_body_methods::<Request>(proto)`), bypassing the
//      macro's per-method wiring.
//
// Hand-rolling is straightforward (FunctionTemplate + per-callback
// wiring) and matches the patterns in `headers.rs` / `dom/abort_signal.rs`.

// ---------------------------------------------------------------------------
// Hand-rolled constructor + install
// ---------------------------------------------------------------------------

/// Install Request on globalThis with hand-rolled getters
/// (method/url/headers/signal/...) and the shared Body methods
/// (text/json/...).
pub fn install_global(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
    let class_tmpl = v8::FunctionTemplate::new(scope, request_constructor_callback);
    let class_name = v8::String::new(scope, "Request").unwrap();
    class_tmpl.set_class_name(class_name);
    class_tmpl
        .instance_template(scope)
        .set_internal_field_count(1);

    let class_fn = class_tmpl.get_function(scope).unwrap();

    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let our_proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    let our_proto: v8::Local<v8::Object> = our_proto_v.try_into().unwrap();

    install_request_getters(scope, our_proto);
    install_method(scope, our_proto, "clone", request_clone_callback);
    install_body_methods::<Request>(scope, our_proto);

    // Symbol.toStringTag — read-only, non-enumerable, configurable.
    let tag_sym = v8::Symbol::get_to_string_tag(scope);
    let tag_value = v8::String::new(scope, "Request").unwrap();
    let mut tag_desc = v8::PropertyDescriptor::new_from_value(tag_value.into());
    tag_desc.set_configurable(true);
    tag_desc.set_enumerable(false);
    our_proto.define_property(scope, tag_sym.into(), &tag_desc);

    let key = v8::String::new(scope, "Request").unwrap();
    global.set(scope, key.into(), class_fn.into());
}

fn install_method(
    scope: &mut v8::PinScope,
    proto: v8::Local<v8::Object>,
    name: &str,
    cb: impl v8::MapFnTo<v8::FunctionCallback>,
) {
    let key = v8::String::new(scope, name).unwrap();
    let tmpl = v8::FunctionTemplate::new(scope, cb);
    let func = tmpl.get_function(scope).unwrap();
    proto.set(scope, key.into(), func.into());
}

fn install_getter(
    scope: &mut v8::PinScope,
    proto: v8::Local<v8::Object>,
    name: &str,
    cb: impl v8::MapFnTo<v8::FunctionCallback>,
) {
    let key = v8::String::new(scope, name).unwrap();
    let tmpl = v8::FunctionTemplate::new(scope, cb);
    let getter_fn = tmpl.get_function(scope).unwrap();
    let mut desc = v8::PropertyDescriptor::new_from_get_set(
        getter_fn.into(),
        v8::undefined(scope).into(),
    );
    desc.set_configurable(true);
    desc.set_enumerable(true);
    proto.define_property(scope, key.into(), &desc);
}

fn install_request_getters(scope: &mut v8::PinScope, proto: v8::Local<v8::Object>) {
    install_getter(scope, proto, "method", method_getter);
    install_getter(scope, proto, "url", url_getter);
    install_getter(scope, proto, "headers", headers_getter);
    install_getter(scope, proto, "signal", signal_getter);
    install_getter(scope, proto, "destination", destination_getter);
    install_getter(scope, proto, "referrer", referrer_getter);
    install_getter(scope, proto, "referrerPolicy", referrer_policy_getter);
    install_getter(scope, proto, "mode", mode_getter);
    install_getter(scope, proto, "credentials", credentials_getter);
    install_getter(scope, proto, "cache", cache_getter);
    install_getter(scope, proto, "redirect", redirect_getter);
    install_getter(scope, proto, "integrity", integrity_getter);
    install_getter(scope, proto, "keepalive", keepalive_getter);
    install_getter(
        scope,
        proto,
        "isReloadNavigation",
        is_reload_navigation_getter,
    );
    install_getter(
        scope,
        proto,
        "isHistoryNavigation",
        is_history_navigation_getter,
    );
    install_getter(scope, proto, "duplex", duplex_getter);
    install_getter(scope, proto, "priority", priority_getter);
}

// ---------------------------------------------------------------------------
// Constructor: new Request(input, init?)
// ---------------------------------------------------------------------------

/// Fetch §5.4 step 1–43. Implemented as a hand-rolled callback so we
/// can:
///   1. Accept `input` as either a USVString URL or a Request to copy.
///   2. Apply `init` overrides in spec order.
///   3. Validate methods (D-18) and reject CONNECT/TRACE/TRACK.
///   4. Extract the body (if any) and stash on the state.
///   5. Default-mint an AbortSignal that follows `init.signal`.
fn request_constructor_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let this_obj = args.this();

    let input_v = args.get(0);
    let init_v = args.get(1);

    if input_v.is_undefined() {
        let m = v8::String::new(scope, "Request constructor requires an input argument").unwrap();
        let exc = v8::Exception::type_error(scope, m);
        scope.throw_exception(exc);
        return;
    }

    let mut state = RequestState::default();

    // Step 6: parse `input` — string or Request.
    let input_is_request =
        input_v.is_object() && is_request_instance(scope, input_v.try_into().unwrap_or(this_obj));

    let initial_url: String = if input_is_request {
        let req_obj: v8::Local<v8::Object> = input_v.try_into().unwrap();
        let raw = match state_ptr(scope, req_obj) {
            Some(p) => p,
            None => {
                let m = v8::String::new(scope, "Request input is not a Request").unwrap();
                let exc = v8::Exception::type_error(scope, m);
                scope.throw_exception(exc);
                return;
            }
        };
        let other: &RequestState = unsafe { &*raw };
        // Copy over scalar fields from the input Request.
        *state.method.borrow_mut() = other.method.borrow().clone();
        *state.referrer.borrow_mut() = other.referrer.borrow().clone();
        *state.referrer_policy.borrow_mut() = other.referrer_policy.borrow().clone();
        *state.mode.borrow_mut() = other.mode.borrow().clone();
        *state.credentials.borrow_mut() = other.credentials.borrow().clone();
        *state.cache.borrow_mut() = other.cache.borrow().clone();
        *state.redirect.borrow_mut() = other.redirect.borrow().clone();
        *state.integrity.borrow_mut() = other.integrity.borrow().clone();
        *state.keepalive.borrow_mut() = *other.keepalive.borrow();
        *state.priority.borrow_mut() = other.priority.borrow().clone();
        // Body / Headers will be (potentially) overridden by init.
        // The disturbed-input-Request check (Fetch §5.4 step 36 "If
        // input is a Request and inputBody is non-null and inputBody
        // is a body whose stream is disturbed, throw a TypeError")
        // moves AFTER we know whether init.body provides an override.
        // If init.body is set, we use that and don't inherit the
        // input's body — so a disturbed input is fine.
        other.url.borrow().clone()
    } else {
        // String input. Per Fetch §5.4 step 6: parse input against entry
        // settings object's API base URL. In a server-side runtime we
        // don't have a document or a worker location; we follow the
        // workerd convention of using `http://localhost/` as the
        // synthetic API base URL so that:
        //   - Empty string and relative URLs resolve (per spec they
        //     resolve against the base URL, not fail).
        //   - Absolute URLs short-circuit and use their own scheme.
        // ada-url tries absolute-parse first; if that fails it falls
        // back to base-relative parsing. We prefer absolute parse
        // explicitly to keep the resulting href closer to user input
        // when possible.
        let url_str = input_v.to_rust_string_lossy(scope);
        let parsed = ada_url::Url::parse(&url_str, None)
            .or_else(|_| ada_url::Url::parse(&url_str, Some(DEFAULT_BASE_URL)));
        match parsed {
            Ok(u) => u.href().to_string(),
            Err(_) => {
                let m = v8::String::new(
                    scope,
                    &format!("Failed to parse URL: {url_str}"),
                )
                .unwrap();
                let exc = v8::Exception::type_error(scope, m);
                scope.throw_exception(exc);
                return;
            }
        }
    };

    *state.url.borrow_mut() = initial_url;

    // Step 12+: apply `init` overrides.
    let init_obj: Option<v8::Local<v8::Object>> = if init_v.is_undefined() {
        None
    } else {
        match v8::Local::<v8::Object>::try_from(init_v) {
            Ok(o) => Some(o),
            Err(_) => None,
        }
    };

    // Method.
    if let Some(init) = init_obj {
        if let Some(m_v) = get_init(scope, init, "method") {
            let raw_method = m_v.to_rust_string_lossy(scope);
            let normalized = match normalize_method(&raw_method) {
                Ok(m) => m,
                Err(e) => {
                    let m = v8::String::new(scope, &e).unwrap();
                    let exc = v8::Exception::type_error(scope, m);
                    scope.throw_exception(exc);
                    return;
                }
            };
            *state.method.borrow_mut() = normalized;
        }
    }

    // Referrer / referrerPolicy / mode / credentials / cache /
    // redirect / integrity / keepalive / duplex / priority — string
    // copies, no validation in v1.
    if let Some(init) = init_obj {
        copy_string_init(scope, init, "referrer", &state.referrer);
        copy_string_init(scope, init, "referrerPolicy", &state.referrer_policy);
        copy_string_init(scope, init, "mode", &state.mode);
        copy_string_init(scope, init, "credentials", &state.credentials);
        copy_string_init(scope, init, "cache", &state.cache);
        copy_string_init(scope, init, "redirect", &state.redirect);
        copy_string_init(scope, init, "integrity", &state.integrity);
        copy_string_init(scope, init, "duplex", &state.duplex);
        copy_string_init(scope, init, "priority", &state.priority);
        if let Some(k) = get_init(scope, init, "keepalive") {
            *state.keepalive.borrow_mut() = k.boolean_value(scope);
        }
        // Per Fetch (Chrome/Deno/etc.): `duplex: "full"` is not yet
        // supported — implementations throw TypeError. We match that
        // behaviour. WPT request-init-stream.any.js explicitly
        // requires this for any body shape (null, string, Uint8Array,
        // ReadableStream) when duplex is "full".
        if state.duplex.borrow().as_str() == "full" {
            let m = v8::String::new(
                scope,
                "Request init.duplex = 'full' is not supported",
            )
            .unwrap();
            let exc = v8::Exception::type_error(scope, m);
            scope.throw_exception(exc);
            return;
        }
    }

    // Body extraction. Step 35–36.
    let body_v: Option<v8::Local<v8::Value>> = if let Some(init) = init_obj {
        get_init(scope, init, "body")
    } else {
        None
    };

    // If init.body is missing AND input was a Request, inherit the
    // input's body. Per Fetch §5.4 step 36 + step 42 ("clone a body"):
    // the new request's body is a CLONE of the input's body — a fresh
    // body whose stream is independent of the input's.
    //
    // We delay both the actual cloning AND the input-disturb marker
    // until AFTER all input validation succeeds (per WPT
    // request-disturbed.any.js "Request construction failure should
    // not set bodyUsed"). For now record only that we should perform
    // an inherit clone; carry forward the disturbed-check error.
    enum InheritMode {
        None,
        BytesSource(std::rc::Rc<Vec<u8>>),
        StreamSource,
    }
    let inherit_mode: InheritMode = if body_v.is_none() && input_is_request {
        let req_obj: v8::Local<v8::Object> = input_v.try_into().unwrap();
        let raw = state_ptr(scope, req_obj).unwrap();
        let other: &RequestState = unsafe { &*raw };
        let other_stream_g = other.body.borrow().stream.borrow().clone();
        let other_source = other.body.borrow().source.clone();

        if let Some(stream_g) = other_stream_g.as_ref() {
            let stream = v8::Local::new(scope, stream_g.clone());
            if crate::fetch_body::consumers::stream_disturbed_or_used(scope, req_obj, stream) {
                let m = v8::String::new(
                    scope,
                    "Cannot construct Request from a disturbed Request",
                )
                .unwrap();
                let exc = v8::Exception::type_error(scope, m);
                scope.throw_exception(exc);
                return;
            }
        }
        match other_source {
            Some(crate::fetch_body::body::BodySource::Bytes(rc))
            | Some(crate::fetch_body::body::BodySource::Blob(rc, _))
            | Some(crate::fetch_body::body::BodySource::UrlSearchParams(rc))
            | Some(crate::fetch_body::body::BodySource::FormData(rc, _)) => {
                InheritMode::BytesSource(rc)
            }
            Some(crate::fetch_body::body::BodySource::Stream) => InheritMode::StreamSource,
            None => InheritMode::None,
        }
    } else {
        InheritMode::None
    };

    // For the GET/HEAD/forbidden-method check below to fire BEFORE we
    // disturb input, the inherited body value here is ONLY the
    // disturbing-not-yet-applied marker. The actual stream construction
    // happens after validation succeeds.
    //
    // We synthesise a non-null sentinel (a fresh empty stream is
    // overkill, so we use null but track the mode separately). The
    // GET/HEAD validation needs to know SOMETHING is there → use a
    // null + mode-driven inherit applied later.
    let has_inherited_body = !matches!(inherit_mode, InheritMode::None);

    let body_input: Option<v8::Local<v8::Value>> = body_v;

    // GET/HEAD body-presence check (Fetch §5.4 step 35.5). Fires for
    // BOTH explicit init.body AND inherited body. This must run BEFORE
    // any body extraction / disturb-marker — per WPT
    // request-disturbed.any.js "Request construction failure should
    // not set bodyUsed".
    let has_explicit_body = body_input.is_some_and(|b| !b.is_null_or_undefined());
    if has_explicit_body || has_inherited_body {
        let method = state.method.borrow().clone();
        if method == "GET" || method == "HEAD" {
            let m = v8::String::new(
                scope,
                "Request with GET/HEAD method cannot have body",
            )
            .unwrap();
            let exc = v8::Exception::type_error(scope, m);
            scope.throw_exception(exc);
            return;
        }
    }

    // Process explicit init.body if any.
    if let Some(b) = body_input {
        if !b.is_null_or_undefined() {
            // Per Fetch §5.4 step 36: when body is a ReadableStream,
            // init["duplex"] must exist (since the body is half-duplex
            // by default — full-duplex is opt-in). The spec's exact
            // wording: "If body is a ReadableStream and init["duplex"]
            // does not exist, throw a TypeError."
            //
            // We match Chrome / Deno here: only validate when body is
            // a ReadableStream. URLSearchParams / Blob / etc. don't
            // need duplex.
            if let Some(init) = init_obj {
                let body_is_stream = if let Ok(obj) = v8::Local::<v8::Object>::try_from(b) {
                    is_readable_stream_global_instance(scope, obj)
                } else {
                    false
                };
                if body_is_stream {
                    let duplex_v = get_init(scope, init, "duplex");
                    if duplex_v.is_none() {
                        let m = v8::String::new(
                            scope,
                            "Request with ReadableStream body requires init.duplex = 'half'",
                        )
                        .unwrap();
                        let exc = v8::Exception::type_error(scope, m);
                        scope.throw_exception(exc);
                        return;
                    }
                }
            }
            let keepalive = *state.keepalive.borrow();
            match extract_body(scope, b, keepalive) {
                Ok(extracted) => {
                    *state.body.borrow_mut() = extracted.body;
                    if let Some(ct) = extracted.content_type {
                        // Stash for later: we'll set on Headers after
                        // the headers init step, but only if the user
                        // didn't already set one.
                        state
                            .destination
                            .borrow_mut()
                            .clear(); // unrelated; using a separate var below
                        ensure_content_type(scope, &mut state, ct);
                    }
                }
                Err(e) => {
                    let m = v8::String::new(scope, &e.message).unwrap();
                    let exc = match e.kind {
                        crate::state::OpErrorKind::TypeError => {
                            v8::Exception::type_error(scope, m)
                        }
                        crate::state::OpErrorKind::RangeError => {
                            v8::Exception::range_error(scope, m)
                        }
                        _ => v8::Exception::error(scope, m),
                    };
                    scope.throw_exception(exc);
                    return;
                }
            }
        }
    }

    // Apply inherited body from input Request (if init.body wasn't
    // provided). At this point all validation has succeeded, so
    // disturbing the input is safe. Per Fetch §5.4 step 42 ("If
    // initBody is null and inputBody is non-null, set finalBody to the
    // result of cloning inputBody.") — clone via body-source rebuild
    // (preserves input's stream identity for byte sources) or tee
    // (for true Stream sources).
    if !has_explicit_body {
        if let InheritMode::BytesSource(rc) = &inherit_mode {
            // FIX B: defer stream construction. The body getter
            // builds a ReadableStream lazily from the source.
            let length = Some(rc.len() as u64);
            *state.body.borrow_mut() = crate::fetch_body::body::BodyImpl {
                stream: std::cell::RefCell::new(None),
                source: Some(crate::fetch_body::body::BodySource::Bytes(rc.clone())),
                length,
            };
        } else if let InheritMode::StreamSource = &inherit_mode {
            // Tee the input's stream; replace input's stream with
            // branch[0] (it remains in input's body slot but is now
            // tee'd-locked); use branch[1] as the new request's body.
            let req_obj: v8::Local<v8::Object> = input_v.try_into().unwrap();
            let raw = state_ptr(scope, req_obj).unwrap();
            let other: &RequestState = unsafe { &*raw };
            let other_stream_g = other.body.borrow().stream.borrow().clone();
            if let Some(stream_g) = other_stream_g {
                let stream_local = v8::Local::new(scope, stream_g);
                if let Some((branch_a, branch_b)) = tee_stream(scope, stream_local) {
                    *other.body.borrow().stream.borrow_mut() = Some(v8::Global::new(scope, branch_a));
                    *state.body.borrow_mut() = crate::fetch_body::body::BodyImpl {
                        stream: std::cell::RefCell::new(Some(v8::Global::new(scope, branch_b))),
                        source: Some(crate::fetch_body::body::BodySource::Stream),
                        length: None,
                    };
                }
            }
        }
    }

    // Per Fetch §5.4 step 42 + WPT request-disturbed.any.js: if input
    // is a Request with a non-null body, the input is marked body-used
    // regardless of whether init.body overrode the body. The test
    // "Input request used for creating new request became disturbed
    // even if body is not used" confirms this: even when init.body is
    // provided, the input request becomes disturbed.
    //
    // Fire only on construction success (we only reach here past all
    // validation throws — per WPT "Request construction failure should
    // not set bodyUsed").
    if input_is_request {
        let req_obj: v8::Local<v8::Object> = input_v.try_into().unwrap();
        let raw = state_ptr(scope, req_obj).unwrap();
        let other: &RequestState = unsafe { &*raw };
        // Body is non-null when the stream is materialized OR a
        // rewindable source is present (FIX B lazy-stream path).
        let body_present = {
            let body = other.body.borrow();
            body.stream.borrow().is_some() || body.source.is_some()
        };
        if body_present {
            crate::fetch_body::consumers::set_body_used_marker(scope, req_obj);
        }
    }

    // Headers — build / inherit. Per Fetch §5.4 step 32:
    //   1. Let headers be a copy of this's headers.
    //   2. If init["headers"] exists, then [...] fill from init.
    let headers_obj = build_request_headers(scope, init_obj, input_is_request, input_v);
    let headers_obj = match headers_obj {
        Ok(h) => h,
        Err(msg) => {
            let m = v8::String::new(scope, &msg).unwrap();
            let exc = v8::Exception::type_error(scope, m);
            scope.throw_exception(exc);
            return;
        }
    };

    // Apply pending Content-Type (set on the body but only if the user
    // didn't already provide one on init.headers). The destination was
    // hijacked above as a flag — clean up by relying on a separate
    // pending_content_type local. Refactor: we inlined the apply
    // already in `ensure_content_type` which sets a private slot on
    // the state. Let's apply it now to the headers if present.
    apply_pending_content_type(scope, &mut state, headers_obj);

    *state.headers.borrow_mut() = Some(v8::Global::new(scope, headers_obj));

    // Signal: chain `init.signal` if provided. Always mint a fresh
    // signal so `request.signal` is non-null per Fetch §5.4.
    let signal_obj = build_request_signal(scope, init_obj);
    *state.signal.borrow_mut() = Some(v8::Global::new(scope, signal_obj));

    // Box up + install into internal field 0.
    let boxed = Box::new(state);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    this_obj.set_internal_field(0, ext.into());

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        this_obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut RequestState));
        }),
    );
    std::mem::forget(weak);
}

fn is_request_instance(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>) -> bool {
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, "Request").unwrap();
    let class_v = match global.get(scope, key.into()) {
        Some(v) => v,
        None => return false,
    };
    let class_obj: v8::Local<v8::Object> = match class_v.try_into() {
        Ok(o) => o,
        Err(_) => return false,
    };
    obj.instance_of(scope, class_obj).unwrap_or(false)
}

/// True iff `obj instanceof globalThis.ReadableStream`. Used for the
/// duplex-validation step (Fetch §5.4 step 36) — needs the same
/// discriminator as fetch_body::extract::is_readable_stream_instance.
fn is_readable_stream_global_instance(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
) -> bool {
    let global = scope.get_current_context().global(scope);
    let key = match v8::String::new(scope, "ReadableStream") {
        Some(k) => k,
        None => return false,
    };
    let Some(class_v) = global.get(scope, key.into()) else {
        return false;
    };
    let Ok(class_obj) = v8::Local::<v8::Object>::try_from(class_v) else {
        return false;
    };
    obj.instance_of(scope, class_obj).unwrap_or(false)
}

fn get_init<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    init: v8::Local<v8::Object>,
    name: &str,
) -> Option<v8::Local<'s, v8::Value>> {
    let key = v8::String::new(scope, name)?;
    let v = init.get(scope, key.into())?;
    if v.is_undefined() {
        return None;
    }
    Some(v)
}

fn copy_string_init(
    scope: &mut v8::PinScope,
    init: v8::Local<v8::Object>,
    name: &str,
    target: &RefCell<String>,
) {
    let key = match v8::String::new(scope, name) {
        Some(k) => k,
        None => return,
    };
    let Some(v) = init.get(scope, key.into()) else { return };
    if v.is_undefined() {
        return;
    }
    *target.borrow_mut() = v.to_rust_string_lossy(scope);
}

/// Per Fetch §4.3 "method" + §5.4 step 25:
///   1. If method is one of `CONNECT`, `TRACE`, `TRACK` (case-
///      insensitive), throw TypeError.
///   2. If method is one of the standard methods (DELETE, GET, HEAD,
///      OPTIONS, POST, PUT) case-insensitively, return the upper-case
///      form.
///   3. Otherwise, return method as-is (case-preserved per spec).
fn normalize_method(method: &str) -> Result<String, String> {
    let upper = method.to_ascii_uppercase();
    match upper.as_str() {
        "CONNECT" | "TRACE" | "TRACK" => {
            Err(format!("'{method}' HTTP method is forbidden"))
        }
        "DELETE" | "GET" | "HEAD" | "OPTIONS" | "POST" | "PUT" => Ok(upper),
        _ => {
            // Validate as a token (RFC 9110 §5.6.2).
            if method.is_empty() || !method.bytes().all(is_method_token) {
                return Err(format!("'{method}' is not a valid HTTP method"));
            }
            Ok(method.to_string())
        }
    }
}

fn is_method_token(b: u8) -> bool {
    matches!(
        b,
        b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+'
            | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z'
    )
}

// ---------------------------------------------------------------------------
// Body / Content-Type plumbing
// ---------------------------------------------------------------------------

thread_local! {
    /// Pending Content-Type to apply to headers AFTER they are built.
    /// Indexed by the address of the RequestState being constructed —
    /// per-thread, used only during the constructor's lifetime.
    static PENDING_CT: RefCell<std::collections::HashMap<usize, String>> =
        RefCell::new(std::collections::HashMap::new());
}

fn ensure_content_type(scope: &mut v8::PinScope, state: &mut RequestState, ct: String) {
    let _ = scope;
    let key = state as *const RequestState as usize;
    PENDING_CT.with(|m| {
        m.borrow_mut().insert(key, ct);
    });
}

fn apply_pending_content_type(
    scope: &mut v8::PinScope,
    state: &mut RequestState,
    headers_obj: v8::Local<v8::Object>,
) {
    let key = state as *const RequestState as usize;
    let pending = PENDING_CT.with(|m| m.borrow_mut().remove(&key));
    let Some(ct) = pending else { return };
    // Only set Content-Type if not already present.
    let has_key = v8::String::new(scope, "has").unwrap();
    let has_v = match headers_obj.get(scope, has_key.into()) {
        Some(v) => v,
        None => return,
    };
    let has_fn: v8::Local<v8::Function> = match has_v.try_into() {
        Ok(f) => f,
        Err(_) => return,
    };
    let arg = v8::String::new(scope, "Content-Type").unwrap();
    let exists = match has_fn.call(scope, headers_obj.into(), &[arg.into()]) {
        Some(v) => v.boolean_value(scope),
        None => false,
    };
    if exists {
        return;
    }
    // headers.set("Content-Type", ct)
    let set_key = v8::String::new(scope, "set").unwrap();
    let set_v = match headers_obj.get(scope, set_key.into()) {
        Some(v) => v,
        None => return,
    };
    let set_fn: v8::Local<v8::Function> = match set_v.try_into() {
        Ok(f) => f,
        Err(_) => return,
    };
    let n = v8::String::new(scope, "Content-Type").unwrap();
    let v = v8::String::new(scope, &ct).unwrap();
    let _ = set_fn.call(scope, headers_obj.into(), &[n.into(), v.into()]);
}

// ---------------------------------------------------------------------------
// Headers + Signal builders
// ---------------------------------------------------------------------------

fn build_request_headers<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    init_obj: Option<v8::Local<v8::Object>>,
    input_is_request: bool,
    input_v: v8::Local<v8::Value>,
) -> Result<v8::Local<'s, v8::Object>, String> {
    let global = scope.get_current_context().global(scope);
    let headers_class_key = v8::String::new(scope, "Headers").unwrap();
    let class_v = global
        .get(scope, headers_class_key.into())
        .ok_or_else(|| "Headers class missing".to_string())?;
    let class_fn: v8::Local<v8::Function> = class_v
        .try_into()
        .map_err(|_| "Headers is not a function".to_string())?;

    // Determine the init for the new Headers:
    //   - If init.headers is present, use that.
    //   - Else if input is a Request, copy headers from there.
    //   - Else empty.
    let headers_init: v8::Local<v8::Value> = if let Some(init) = init_obj {
        if let Some(v) = get_init(scope, init, "headers") {
            v
        } else if input_is_request {
            // Copy from input Request's headers. We invoke the Headers
            // constructor with the existing Headers object — it walks
            // its iterator.
            let req_obj: v8::Local<v8::Object> = input_v.try_into().unwrap();
            let raw = state_ptr(scope, req_obj).ok_or_else(|| "Request input invalid".to_string())?;
            let other: &RequestState = unsafe { &*raw };
            match other.headers.borrow().as_ref() {
                Some(g) => v8::Local::new(scope, g.clone()).into(),
                None => v8::undefined(scope).into(),
            }
        } else {
            v8::undefined(scope).into()
        }
    } else if input_is_request {
        let req_obj: v8::Local<v8::Object> = input_v.try_into().unwrap();
        let raw = state_ptr(scope, req_obj).ok_or_else(|| "Request input invalid".to_string())?;
        let other: &RequestState = unsafe { &*raw };
        match other.headers.borrow().as_ref() {
            Some(g) => v8::Local::new(scope, g.clone()).into(),
            None => v8::undefined(scope).into(),
        }
    } else {
        v8::undefined(scope).into()
    };

    let args = [headers_init];
    let h = class_fn
        .new_instance(scope, &args)
        .ok_or_else(|| "Headers constructor failed".to_string())?;
    Ok(h)
}

fn build_request_signal<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    init_obj: Option<v8::Local<v8::Object>>,
) -> v8::Local<'s, v8::Object> {
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, "AbortSignal").unwrap();
    let class_v = global.get(scope, key.into()).expect("AbortSignal missing");
    let class_obj: v8::Local<v8::Object> = class_v.try_into().unwrap();

    // If init.signal is provided, run AbortSignal.any([init.signal])
    // so the request's signal aborts when init.signal does. If no
    // init.signal, just `new AbortController().signal`.
    if let Some(init) = init_obj {
        if let Some(sig_v) = get_init(scope, init, "signal") {
            if !sig_v.is_null_or_undefined() {
                // AbortSignal.any([sig_v]) — returns a fresh signal.
                let any_key = v8::String::new(scope, "any").unwrap();
                if let Some(any_fn_v) = class_obj.get(scope, any_key.into()) {
                    if let Ok(any_fn) = v8::Local::<v8::Function>::try_from(any_fn_v) {
                        let arr = v8::Array::new(scope, 1);
                        arr.set_index(scope, 0, sig_v);
                        let args = [arr.into()];
                        if let Some(result) = any_fn.call(scope, class_obj.into(), &args) {
                            if let Ok(o) = v8::Local::<v8::Object>::try_from(result) {
                                return o;
                            }
                        }
                    }
                }
            }
        }
    }

    // Default: fresh AbortController().signal.
    let ac_key = v8::String::new(scope, "AbortController").unwrap();
    let ac_v = global.get(scope, ac_key.into()).expect("AbortController missing");
    let ac_fn: v8::Local<v8::Function> = ac_v.try_into().unwrap();
    let ac = ac_fn
        .new_instance(scope, &[])
        .expect("new AbortController failed");
    let sig_key = v8::String::new(scope, "signal").unwrap();
    let sig_v = ac.get(scope, sig_key.into()).unwrap();
    sig_v.try_into().unwrap()
}

// ---------------------------------------------------------------------------
// Getter callbacks
// ---------------------------------------------------------------------------

fn brand_check(scope: &mut v8::PinScope, this: v8::Local<v8::Object>) -> Option<*mut RequestState> {
    state_ptr(scope, this).or_else(|| {
        let m = v8::String::new(scope, "Illegal invocation").unwrap();
        let exc = v8::Exception::type_error(scope, m);
        scope.throw_exception(exc);
        None
    })
}

macro_rules! string_getter {
    ($name:ident, $field:ident) => {
        fn $name(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            mut rv: v8::ReturnValue,
        ) {
            let Some(raw) = brand_check(scope, args.this()) else { return };
            let state: &RequestState = unsafe { &*raw };
            let s = state.$field.borrow().clone();
            let v = v8::String::new(scope, &s).unwrap();
            rv.set(v.into());
        }
    };
}

string_getter!(method_getter, method);
string_getter!(url_getter, url);
string_getter!(destination_getter, destination);
string_getter!(referrer_getter, referrer);
string_getter!(referrer_policy_getter, referrer_policy);
string_getter!(mode_getter, mode);
string_getter!(credentials_getter, credentials);
string_getter!(cache_getter, cache);
string_getter!(redirect_getter, redirect);
string_getter!(integrity_getter, integrity);
string_getter!(duplex_getter, duplex);
string_getter!(priority_getter, priority);

macro_rules! bool_getter {
    ($name:ident, $field:ident) => {
        fn $name(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            mut rv: v8::ReturnValue,
        ) {
            let Some(raw) = brand_check(scope, args.this()) else { return };
            let state: &RequestState = unsafe { &*raw };
            let b = *state.$field.borrow();
            let v = v8::Boolean::new(scope, b);
            rv.set(v.into());
        }
    };
}

bool_getter!(keepalive_getter, keepalive);
bool_getter!(is_reload_navigation_getter, is_reload_navigation);
bool_getter!(is_history_navigation_getter, is_history_navigation);

fn headers_getter(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(raw) = brand_check(scope, args.this()) else { return };
    let state: &RequestState = unsafe { &*raw };
    match state.headers.borrow().as_ref() {
        Some(g) => {
            let v = v8::Local::new(scope, g.clone());
            rv.set(v.into());
        }
        None => {
            rv.set(v8::null(scope).into());
        }
    }
}

fn signal_getter(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(raw) = brand_check(scope, args.this()) else { return };
    let state: &RequestState = unsafe { &*raw };
    match state.signal.borrow().as_ref() {
        Some(g) => {
            let v = v8::Local::new(scope, g.clone());
            rv.set(v.into());
        }
        None => {
            rv.set(v8::null(scope).into());
        }
    }
}

// ---------------------------------------------------------------------------
// clone()
// ---------------------------------------------------------------------------

fn request_clone_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    let Some(raw) = brand_check(scope, this) else {
        return;
    };
    let state: &RequestState = unsafe { &*raw };

    // Disturbed body → TypeError. Use both the locked-stream check
    // AND the wrapper's body-used marker (which fires when a consumer
    // started but the stream auto-released its lock).
    let stream_global_opt = state.body.borrow().stream.borrow().clone();
    if let Some(stream_g) = &stream_global_opt {
        let stream = v8::Local::new(scope, stream_g.clone());
        if crate::fetch_body::consumers::stream_disturbed_or_used(scope, this, stream) {
            let m = v8::String::new(scope, "Cannot clone a disturbed Request").unwrap();
            let exc = v8::Exception::type_error(scope, m);
            scope.throw_exception(exc);
            return;
        }
    }

    // Build the clone WITHOUT going through `new Request(this, ...)`.
    // The constructor's "transfer body" step (per Fetch §5.4 step 36)
    // would disturb the original — we don't want that for clone(),
    // since the spec's `clone()` algorithm preserves the original's
    // body usability. So we build a fresh Request instance, copy
    // scalar fields from `this`, and tee or rebuild the body.
    let global = scope.get_current_context().global(scope);
    let req_class_key = v8::String::new(scope, "Request").unwrap();
    let req_class_v = global.get(scope, req_class_key.into()).unwrap();
    let req_class_fn: v8::Local<v8::Function> = req_class_v.try_into().unwrap();

    let body_is_stream = matches!(
        state.body.borrow().source,
        Some(crate::fetch_body::body::BodySource::Stream)
    ) && state.body.borrow().stream.borrow().is_some();

    // Tee the stream so original + clone share both halves and remain
    // independently consumable.
    let (left_branch, right_branch) = if body_is_stream {
        let stream_g = state.body.borrow().stream.borrow().clone().unwrap();
        let stream = v8::Local::new(scope, stream_g);
        match tee_stream(scope, stream) {
            Some(pair) => (Some(pair.0), Some(pair.1)),
            None => {
                let m = v8::String::new(scope, "Failed to tee Request body").unwrap();
                let exc = v8::Exception::type_error(scope, m);
                scope.throw_exception(exc);
                return;
            }
        }
    } else {
        (None, None)
    };

    // Build init that passes the URL via the constructor's URL parser
    // and the cloned body / headers. We include `duplex: "half"`
    // unconditionally — the constructor's duplex check fires for any
    // ReadableStream body, and we may pass a tee'd stream below.
    let init = v8::Object::new(scope);
    {
        let key = v8::String::new(scope, "method").unwrap();
        let v = v8::String::new(scope, &state.method.borrow()).unwrap();
        init.set(scope, key.into(), v.into());
    }
    {
        let key = v8::String::new(scope, "duplex").unwrap();
        let v = v8::String::new(scope, "half").unwrap();
        init.set(scope, key.into(), v.into());
    }
    if let Some(h_g) = state.headers.borrow().clone() {
        let h_local = v8::Local::new(scope, h_g);
        let key = v8::String::new(scope, "headers").unwrap();
        init.set(scope, key.into(), h_local.into());
    }
    if let Some(rb) = right_branch {
        let key = v8::String::new(scope, "body").unwrap();
        init.set(scope, key.into(), rb.into());
        if let Some(lb) = left_branch {
            *state.body.borrow().stream.borrow_mut() = Some(v8::Global::new(scope, lb));
        }
    } else if let Some(src) = state.body.borrow().source.clone() {
        match src {
            crate::fetch_body::body::BodySource::Bytes(rc)
            | crate::fetch_body::body::BodySource::Blob(rc, _)
            | crate::fetch_body::body::BodySource::UrlSearchParams(rc)
            | crate::fetch_body::body::BodySource::FormData(rc, _) => {
                let new_stream = crate::fetch_body::extract::build_byte_stream(scope, rc);
                let stream_local = v8::Local::new(scope, new_stream);
                let key = v8::String::new(scope, "body").unwrap();
                init.set(scope, key.into(), stream_local.into());
            }
            crate::fetch_body::body::BodySource::Stream => {}
        }
    }

    // Pass URL as a string input (NOT `this` — that would trigger the
    // constructor's input-Request copy path which disturbs the input).
    let url_str = v8::String::new(scope, &state.url.borrow()).unwrap();
    let args2 = [url_str.into(), init.into()];
    let result = req_class_fn.new_instance(scope, &args2);
    match result {
        Some(o) => rv.set(o.into()),
        None => {
            // Exception is already on the scope.
        }
    }
}

fn tee_stream<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<'s, v8::Object>,
) -> Option<(v8::Local<'s, v8::Object>, v8::Local<'s, v8::Object>)> {
    let key = v8::String::new(scope, "tee")?;
    let fn_v = stream.get(scope, key.into())?;
    let fn_l: v8::Local<v8::Function> = fn_v.try_into().ok()?;
    let result = fn_l.call(scope, stream.into(), &[])?;
    let arr: v8::Local<v8::Array> = result.try_into().ok()?;
    let a = arr.get_index(scope, 0)?;
    let b = arr.get_index(scope, 1)?;
    Some((a.try_into().ok()?, b.try_into().ok()?))
}
