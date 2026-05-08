//! Native `Response` class per WHATWG Fetch §5.5
//! (https://fetch.spec.whatwg.org/#response-class).
//!
//! Replaces the JS Response constructor that lives in `embed/fetch.js`.
//! Per design fetch-native v2 §V the constructor walks the spec's 17
//! steps and produces a Response whose body is a native BodyImpl.
//!
//! ## Storage layout
//!
//! Box<ResponseState> in V8 internal field 0:
//!   - body: BodyImpl
//!   - status: u16 (default 200)
//!   - status_text: String (default "")
//!   - type: String — "default" / "error" / "basic" / "cors" / "opaque"
//!     / "opaqueredirect"; v1 only emits "default" and "error".
//!   - url: String — the LAST URL in request's URL list, fragment-
//!     stripped. v1 ships empty by default.
//!   - redirected: bool
//!   - ok: bool — derived (status in 200..300)
//!   - headers: Global<Object>
//!   - web_socket: Option<Global<Object>> — workerd extension preserved
//!     for the gateway upgrade path.
//!
//! ## Static methods
//!
//! - `Response.error()` returns a network-error response.
//! - `Response.redirect(url, status?)` validates status and returns
//!   a redirect response.
//! - `Response.json(data, init?)` serializes via JSON.stringify and
//!   sets Content-Type "application/json".
//!
//! The class is emitted via `#[v8_class] #[v8_state_marker(Response)]
//! impl ResponseState`. The unit `Response` marker drives JS-class
//! identity (install slot, brand check, callback names,
//! `set_class_name`); the `ResponseState` struct carries the boxed
//! state stored in V8 internal field 0. The constructor returns
//! `Result<ResponseState, OpError>` and the eight getters / `clone`
//! method dispatch through `&self` against the state.
//!
//! `install_global` remains hand-rolled because it must:
//!   - install body consumer methods (`text` / `json` / `arrayBuffer`
//!     / `bytes` / `blob` / `formData`) via the shared trait dispatch
//!     (`install_body_methods::<Response>`) — those bypass the macro,
//!   - stash the FunctionTemplate + prototype in a per-isolate
//!     `ResponseTemplateSlot` for the kernel-side fast-path Response
//!     builder (`build_kernel_response`).
//!
//! The three static methods (`error` / `redirect` / `json`) live on
//! the impl block annotated with `#[v8_static_method]` — the macro
//! installs them on the constructor FunctionTemplate (WebIDL §3.7.4)
//! once `gen_static_callback` was fixed to dispatch through `state_ty`
//! under `#[v8_state_marker]` (the call expression now resolves
//! against `ResponseState`, where the bodies live).

use std::cell::{Cell, RefCell};

use zeroship_runtime_macros::{
    v8_class, v8_constructor, v8_getter, v8_method, v8_name, v8_state_marker, v8_static_method,
    WebIdlDict,
};

use super::enums::ResponseType;
use crate::fetch_body::body::{Body, BodyImpl, BodySource};
use crate::fetch_body::consumers::{install_body_methods, BodyMarker};
use crate::fetch_body::extract::extract_body;
use crate::state::OpError;

// ---------------------------------------------------------------------------
// Null-body status set per Fetch §5.5 step 7
// ---------------------------------------------------------------------------

/// Per Fetch's https://fetch.spec.whatwg.org/#null-body-status:
///   { 101, 103, 204, 205, 304 }.
fn is_null_body_status(status: u16) -> bool {
    matches!(status, 101 | 103 | 204 | 205 | 304)
}

fn is_redirect_status(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

// ---------------------------------------------------------------------------
// ResponseInit — `[Dictionary]` per Fetch §5.5
// ---------------------------------------------------------------------------

/// `ResponseInit` per Fetch §5.5. Members:
///   - `status`: u16 (default 200; spec range 200..=599; we extend to
///     allow 101 for the workerd WebSocket-upgrade carve-out).
///   - `statusText`: ByteString-like (default "").
///
/// `headers` (HeadersInit) and `webSocket` (workerd extension) are
/// read raw from the init object outside the dict — same v8::Value
/// passthrough convention as `RequestInit`.
///
/// Status is read as `f64` so we can preserve the spec range check
/// (`isNaN` / out-of-range → RangeError) — converting via `u16` directly
/// would silently truncate. statusText keeps `Option<String>` so the
/// missing-key path skips the per-byte reason-phrase validation.
#[derive(Default, Debug, WebIdlDict)]
pub(crate) struct ResponseInit {
    pub status: Option<f64>,
    #[webidl_name = "statusText"]
    pub status_text: Option<String>,
}

// ---------------------------------------------------------------------------
// ResponseState
// ---------------------------------------------------------------------------

#[allow(missing_debug_implementations)]
pub struct ResponseState {
    pub body: RefCell<BodyImpl>,
    pub status: Cell<u16>,
    pub status_text: RefCell<String>,
    /// Typed `ResponseType` — see `enums.rs`. Default is `Default`
    /// (matches the spec for constructor-built responses); the
    /// `Response.error()` static patches to `Error` after construction.
    pub response_type: Cell<ResponseType>,
    pub url: RefCell<String>,
    pub redirected: Cell<bool>,
    pub headers: RefCell<Option<v8::Global<v8::Object>>>,
    pub web_socket: RefCell<Option<v8::Global<v8::Object>>>,
}

impl Default for ResponseState {
    fn default() -> Self {
        ResponseState {
            body: RefCell::new(BodyImpl::null()),
            status: Cell::new(200),
            status_text: RefCell::new(String::new()),
            response_type: Cell::new(ResponseType::Default),
            url: RefCell::new(String::new()),
            redirected: Cell::new(false),
            headers: RefCell::new(None),
            web_socket: RefCell::new(None),
        }
    }
}

// ---------------------------------------------------------------------------
// Body trait impl + JS-class identity marker
// ---------------------------------------------------------------------------

/// Unit marker recognised by `#[v8_state_marker(Response)]` and the
/// `Body` / `BodyMarker` trait impls. The boxed state at V8 internal
/// field 0 is `Box<ResponseState>`; the marker drives JS-class identity
/// (install slot, brand check, callback names). See design §7.3.
pub struct Response;

impl BodyMarker for Response {
    const CLASS_LABEL: &'static str = "Response";
}

impl Body for Response {
    fn body_state<'a>(
        scope: &mut v8::PinScope,
        this: v8::Local<v8::Object>,
    ) -> Option<&'a BodyImpl> {
        let raw = state_ptr(scope, this)?;
        let state: &ResponseState = unsafe { &*raw };
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
        let state: &ResponseState = unsafe { &*raw };
        let g = state.headers.borrow().as_ref()?.clone();
        let headers_obj = v8::Local::new(scope, g);
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
}

/// Recover the boxed `ResponseState` raw pointer from V8 internal
/// field 0. Returns `None` when the receiver isn't a native Response
/// (the `is_native_response` / `try_native_response_*` consumers below
/// rely on this lax check — the design §7.3.2 settles that the
/// state-pointer accessor stays hand-rolled, NOT macro-emitted).
pub(crate) fn state_ptr(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
) -> Option<*mut ResponseState> {
    let ext = obj
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())?;
    let ptr = ext.value() as *mut ResponseState;
    if ptr.is_null() {
        return None;
    }
    Some(ptr)
}

// ---------------------------------------------------------------------------
// Public surface used by `crate::http::inspect_response`.
//
// The kernel inspects a Response object after the user's handler resolves.
// `try_native_response_body` returns a structured view of the body so the
// kernel can read the body without poking at internal fields: `Some(view)`
// when `obj` is a native Response (state pointer in internal field 0);
// `None` when it's a plain handler return (`{ status, url }`-shaped duck
// type) so the kernel falls through to its plain-value handler.
// ---------------------------------------------------------------------------

/// Body view emitted by `try_native_response_body`. The kernel handles
/// each variant differently:
///
///   - `Empty`: respond with an empty body (no Content-Length set by us).
///   - `Bytes(Vec<u8>)`: rewindable buffered body — read once, ship it.
///   - `Stream`: a user-visible ReadableStream — the kernel calls
///     `streams::response_forwarder::begin_forward` to lock + pump the
///     stream into a Rust-side forwarder.
pub enum NativeResponseBody {
    /// Body is conceptually `null` (no Content-Length, empty body).
    Empty,
    /// Buffered bytes — extracted from `BodySource::Bytes/Blob/
    /// UrlSearchParams/FormData`. Cheap clone via Rc.
    Bytes(std::rc::Rc<Vec<u8>>),
    /// Body source is a user-supplied ReadableStream. Kernel must call
    /// `streams::response_forwarder::begin_forward` to start pumping
    /// into a Rust-side forwarder.
    Stream,
}

/// True iff `obj` is an instance of the native Response class (i.e.
/// has a non-null Box<ResponseState> in internal field 0). Returns
/// `false` for plain objects and primitives.
pub fn is_native_response(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>) -> bool {
    state_ptr(scope, obj).is_some()
}

/// Inspect a native Response's body for the kernel's wire path. Returns
/// `None` if `obj` is not a native Response — caller treats that as a
/// plain handler return and falls through to its duck-typed handler.
///
/// Stream classification: a body is `Stream` iff `BodySource::Stream`,
/// i.e. the user passed a ReadableStream to `new Response(...)`. All
/// other rewindable sources collapse to `Bytes`.
pub fn try_native_response_body(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
) -> Option<NativeResponseBody> {
    let raw = state_ptr(scope, obj)?;
    let state: &ResponseState = unsafe { &*raw };
    let body = state.body.borrow();

    if body.is_null() {
        return Some(NativeResponseBody::Empty);
    }

    match body.source.clone() {
        Some(BodySource::Bytes(rc))
        | Some(BodySource::Blob(rc, _))
        | Some(BodySource::UrlSearchParams(rc))
        | Some(BodySource::FormData(rc, _)) => Some(NativeResponseBody::Bytes(rc)),
        Some(BodySource::Stream) => Some(NativeResponseBody::Stream),
        None => {
            // BodyImpl with stream but no source — shouldn't happen in
            // practice (extract_body always sets one or the other), but
            // treat as Stream for safety: the kernel will run the
            // response forwarder and pump whatever's there.
            Some(NativeResponseBody::Stream)
        }
    }
}

/// Read the native Response's `webSocket` extension as a Global. Returns
/// `None` if `obj` is not a native Response or the slot is unset. Used by
/// `inspect_response` to surface WebSocket upgrade responses.
pub fn try_native_response_websocket(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
) -> Option<v8::Global<v8::Object>> {
    let raw = state_ptr(scope, obj)?;
    let state: &ResponseState = unsafe { &*raw };
    state.web_socket.borrow().clone()
}

// ---------------------------------------------------------------------------
// install_global — hand-rolled wrapper around the macro-emitted
// `Response::install`. Adds:
//   - the body consumer methods (text / json / arrayBuffer / bytes /
//     blob / formData) via `install_body_methods::<Response>`,
//   - the per-isolate `ResponseTemplateSlot` cache used by the kernel
//     fast-path Response builder.
// ---------------------------------------------------------------------------

/// Per-isolate cache of the Response FunctionTemplate + prototype.
///
/// Set from `install_global`; consumed by `build_kernel_response` so the
/// fetch-success path can allocate a Response wrapper without resolving
/// `globalThis.Response` and without invoking the spec constructor (which
/// rebuilds Headers from an init array, runs `extract_body`, etc).
pub struct ResponseTemplateSlot {
    pub class_tmpl: v8::Global<v8::FunctionTemplate>,
    pub prototype: v8::Global<v8::Object>,
}

pub fn install_global(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
    // Macro-emitted install: builds the FunctionTemplate, populates
    // prototype with `clone` + the eight getters, sets Symbol.toStringTag
    // = "Response", caches the template in `__InstallSlot_Response`.
    let class_tmpl = Response::install(scope);

    let class_fn = class_tmpl.get_function(scope).unwrap();

    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let our_proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    let our_proto: v8::Local<v8::Object> = our_proto_v.try_into().unwrap();

    // Body consumer methods are NOT routed through the macro because
    // they share a generic `T: Body + BodyMarker` dispatch that lives in
    // `fetch_body::consumers`. Same shape as Request.
    install_body_methods::<Response>(scope, our_proto);

    let key = v8::String::new(scope, "Response").unwrap();
    global.set(scope, key.into(), class_fn.into());

    // Stash the template + prototype for the kernel-side fast-path
    // Response builder. See `ResponseTemplateSlot`.
    let class_tmpl_g = v8::Global::new(scope, class_tmpl);
    let proto_g = v8::Global::new(scope, our_proto);
    scope.set_slot(ResponseTemplateSlot {
        class_tmpl: class_tmpl_g,
        prototype: proto_g,
    });
}

/// Build a Response wrapper directly from a (status, headers, body) tuple
/// — the success path of the algorithm chain. Skips:
///   - the `globalThis.Response` lookup,
///   - the JS Response constructor's WebIDL init walk,
///   - the per-init-pair JS Array materialization (one 2-elem JS Array per
///     header) that `build_response_object` previously used,
///   - the inner `new Headers(seq)` invocation (which iterates that
///     array and per-pair-validates each header),
///   - `extract_body` (the response bytes already exist as `Vec<u8>` —
///     just install them as `BodySource::Bytes`).
///
/// Body is installed as `BodySource::Bytes(Rc<...>)` with `stream: None` so
/// the body getter materializes the JS ReadableStream lazily on first
/// access (cheap fast path for benchmarks; correct for streaming code that
/// reads `.body`).
pub fn build_kernel_response<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    status: u16,
    status_text: String,
    url: String,
    redirected: bool,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> Option<v8::Local<'s, v8::Object>> {
    // 1. Allocate the Response wrapper via the cached instance template.
    let (resp_tmpl_g, resp_proto_g) = {
        let slot = scope.get_slot::<ResponseTemplateSlot>()?;
        (slot.class_tmpl.clone(), slot.prototype.clone())
    };
    let resp_tmpl = v8::Local::new(scope, resp_tmpl_g);
    let inst_tmpl = resp_tmpl.instance_template(scope);
    let this_obj = inst_tmpl.new_instance(scope)?;
    let resp_proto = v8::Local::new(scope, resp_proto_g);
    this_obj.set_prototype(scope, resp_proto.into());

    // 2. Build the Headers wrapper directly, consuming the (name, value)
    // list — the bytes become the internal storage zero-copy. The wire
    // layer already validated the names + values when parsing the
    // response.
    let headers_obj = crate::headers::build_kernel_headers_owned(scope, headers)?;
    let headers_g = v8::Global::new(scope, headers_obj);

    // 3. Build the body. Null-body status responses (101, 103, 204, 205,
    // 304) keep BodyImpl::null even if `body` is non-empty (defensive —
    // the algorithm chain shouldn't supply a body for these but if it
    // does, we ignore it to match the constructor's behavior).
    let body_impl = if is_null_body_status(status) || body.is_empty() {
        BodyImpl::null()
    } else {
        let len = body.len() as u64;
        let body_rc = std::rc::Rc::new(body);
        BodyImpl {
            stream: RefCell::new(None),
            source: Some(BodySource::Bytes(body_rc)),
            length: Some(len),
        }
    };

    // 4. Build the ResponseState directly.
    let state = ResponseState {
        body: RefCell::new(body_impl),
        status: Cell::new(status),
        status_text: RefCell::new(status_text),
        url: RefCell::new(url),
        redirected: Cell::new(redirected),
        headers: RefCell::new(Some(headers_g)),
        ..ResponseState::default()
    };

    // 5. Box, install in internal field 0, register finalizer.
    let boxed = Box::new(state);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    this_obj.set_internal_field(0, ext.into());

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        this_obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut ResponseState));
        }),
    );
    std::mem::forget(weak);

    Some(this_obj)
}

/// Fast path for `Response.json(data, init?)` when init has no
/// `headers` member. Builds the Response wrapper directly via the
/// per-isolate `ResponseTemplateSlot` — same shape as
/// `build_kernel_response` but optimised for the static-method path:
///
///   - the body is a v8::String we already have (just convert to
///     UTF-8 bytes once),
///   - the headers are a fixed 1-entry list `[("Content-Type",
///     "application/json")]`, minted via `build_kernel_headers_owned`
///     (no per-pair `is_header_name` / value validation),
///   - status / statusText were already validated by the caller.
///
/// Saves vs the JS Response constructor:
///   - the `extract_body` walk (BufferSource / Blob / FormData /
///     URLSearchParams / stream branches all skipped — we know the
///     body is a string),
///   - the `new Headers(init_v)` invocation (HeadersInit WebIDL union
///     dispatch + per-pair validate),
///   - the `set_default_content_type` has-then-set call pair,
///   - the ResponseInit dict parse (we already parsed status /
///     statusText inline above).
fn build_response_json_fast<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    json_str: v8::Local<v8::String>,
    status: u16,
    status_text: String,
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    // 1. Allocate the Response wrapper via the cached instance template.
    let (resp_tmpl_g, resp_proto_g) = {
        let slot = scope
            .get_slot::<ResponseTemplateSlot>()
            .ok_or_else(|| OpError::error("Response template slot missing"))?;
        (slot.class_tmpl.clone(), slot.prototype.clone())
    };
    let resp_tmpl = v8::Local::new(scope, resp_tmpl_g);
    let inst_tmpl = resp_tmpl.instance_template(scope);
    let this_obj = inst_tmpl
        .new_instance(scope)
        .ok_or_else(|| OpError::error("Response instance allocation failed"))?;
    let resp_proto = v8::Local::new(scope, resp_proto_g);
    this_obj.set_prototype(scope, resp_proto.into());

    // 2. Build a 1-entry Headers wrapper containing Content-Type:
    // application/json. The bytes are owned (no clone), and the upstream
    // is "we just minted this string literal" so per-pair validation is
    // skipped — same skip as build_kernel_response uses for response
    // headers from the wire.
    let header_pairs = vec![("Content-Type".to_string(), "application/json".to_string())];
    let headers_obj = crate::headers::build_kernel_headers_owned(scope, header_pairs)
        .ok_or_else(|| OpError::error("Headers allocation failed"))?;
    let headers_g = v8::Global::new(scope, headers_obj);

    // 3. Convert the JSON v8::String to UTF-8 bytes for BodySource::Bytes.
    // We need an owned Vec<u8> so the Body can outlive any V8 GC of the
    // input string. `to_rust_string_lossy` does an isolate-side UTF-8
    // copy.
    let body_bytes = json_str.to_rust_string_lossy(scope).into_bytes();
    let body_len = body_bytes.len() as u64;
    let body_rc = std::rc::Rc::new(body_bytes);
    let body_impl = BodyImpl {
        stream: RefCell::new(None),
        source: Some(BodySource::Bytes(body_rc)),
        length: Some(body_len),
    };

    // 4. Build the ResponseState directly. Type / url / redirected /
    // web_socket all stay at their spec defaults.
    let state = ResponseState {
        body: RefCell::new(body_impl),
        status: Cell::new(status),
        status_text: RefCell::new(status_text),
        headers: RefCell::new(Some(headers_g)),
        ..ResponseState::default()
    };

    // 5. Box, install in internal field 0, register finalizer. Same
    // shape as build_kernel_response and the macro-emitted constructor's
    // gen_box_and_install_finalizer.
    let boxed = Box::new(state);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    this_obj.set_internal_field(0, ext.into());

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        this_obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut ResponseState));
        }),
    );
    std::mem::forget(weak);

    Ok(this_obj)
}

// ---------------------------------------------------------------------------
// Macro-emitted class
// ---------------------------------------------------------------------------

#[v8_class]
#[v8_state_marker(Response)]
impl ResponseState {
    /// `new Response(body?, init?)` — Fetch §5.5 17-step constructor.
    ///
    /// Status: spec range 200..=599 PLUS workerd-style 101 carve-out for
    /// the WebSocket upgrade path (preserved verbatim per design §7.3.3).
    /// statusText: validated as HTTP/1.1 reason-phrase (RFC 7230 §3.2.6
    /// — HTAB / SP / VCHAR / obs-text).
    /// `webSocket` extension preserved on `state.web_socket` so the
    /// gateway can surface `Response.webSocket` for the upgrade dance
    ///.
    #[v8_constructor]
    fn new<'s>(
        scope: &mut v8::PinScope<'s, '_>,
        body: v8::Local<v8::Value>,
        init: v8::Local<v8::Value>,
    ) -> Result<ResponseState, OpError> {
        let state = ResponseState::default();

        // Parse the typed dict once. The dict is the single point where
        // any future WebIDL-enum members (or the f64 status) reject
        // bogus values; today `ResponseInit` carries only scalar
        // status / statusText so the dict is mostly an organisational
        // helper.
        let init_dict = ResponseInit::from_v8(scope, init)?;
        // Whether init is an actual JS Object (vs null/undefined). We
        // need this for the raw `headers` / `webSocket` passthroughs
        // below (the dict can't distinguish missing from null for raw
        // v8::Value members — see `RequestInit` rationale).
        let init_obj: Option<v8::Local<v8::Object>> = if init.is_null_or_undefined() {
            None
        } else {
            v8::Local::<v8::Object>::try_from(init).ok()
        };

        // Step 1: status (default 200). Spec allows 200..=599; we additionally
        // allow 101 as a workerd-style extension for the WebSocket upgrade
        // path — the gateway returns `new Response(null, { status: 101,
        // webSocket: client })` from the user's `fetch` handler. The polyfill
        // had the same carve-out (`embed/fetch.js:246-249`).
        if let Some(n) = init_dict.status {
            let in_range = n == 101.0 || (200.0..=599.0).contains(&n);
            if n.is_nan() || !in_range {
                return Err(OpError::range_error("Invalid status code"));
            }
            state.status.set(n as u16);
        }

        // Step 2: statusText. Validate per HTTP/1.1 reason-phrase ABNF
        // (HTAB / SP / VCHAR / obs-text). Reject CR/LF/non-ASCII control.
        if let Some(s) = init_dict.status_text.clone() {
            if !is_valid_reason_phrase(&s) {
                return Err(OpError::type_error("Invalid statusText"));
            }
            *state.status_text.borrow_mut() = s;
        }

        // Build headers: from init.headers if present, else empty.
        let init_headers_v: Option<v8::Local<v8::Value>> =
            init_obj.and_then(|o| read_init_member(scope, o, "headers"));
        let headers_obj =
            build_response_headers(scope, init_headers_v).map_err(OpError::type_error)?;

        // webSocket extension — preserve as-is for the gateway path.
        if let Some(o) = init_obj {
            if let Some(ws_v) = read_init_member(scope, o, "webSocket") {
                if !ws_v.is_null_or_undefined() {
                    if let Ok(o) = v8::Local::<v8::Object>::try_from(ws_v) {
                        *state.web_socket.borrow_mut() = Some(v8::Global::new(scope, o));
                    }
                }
            }
        }

        // Step 7: null-body status check.
        let status_now = state.status.get();
        let body_is_null = body.is_null_or_undefined();
        if !body_is_null && is_null_body_status(status_now) {
            return Err(OpError::type_error(
                "Response with null body status cannot have a body",
            ));
        }

        // Body extraction. JsValue passthrough (custom user-thrown values
        // from `extract_body`) is preserved automatically by the
        // macro-emitted constructor wrapper — `OpErrorKind::JsValue`
        // re-throws verbatim. Same for TypeError/RangeError/Error mapping.
        if !body_is_null {
            let extracted = extract_body(scope, body, false)?;
            *state.body.borrow_mut() = extracted.body;
            if let Some(ct) = extracted.content_type {
                set_default_content_type(scope, headers_obj, &ct);
            }
        }

        *state.headers.borrow_mut() = Some(v8::Global::new(scope, headers_obj));
        Ok(state)
    }

    /// `type` — WebIDL `[SameObject]` not applicable (string).
    #[v8_getter]
    #[v8_name = "type"]
    fn type_(&self) -> String {
        self.response_type.get().as_str().to_string()
    }

    #[v8_getter]
    fn url(&self) -> String {
        self.url.borrow().clone()
    }

    #[v8_getter]
    fn redirected(&self) -> bool {
        self.redirected.get()
    }

    #[v8_getter]
    fn status(&self) -> u32 {
        self.status.get() as u32
    }

    #[v8_getter]
    fn ok(&self) -> bool {
        let s = self.status.get();
        (200..300).contains(&s)
    }

    #[v8_getter]
    #[v8_name = "statusText"]
    fn status_text(&self) -> String {
        self.status_text.borrow().clone()
    }

    /// `headers` — Fetch §5.5 `[SameObject]`. The hand-roll preserved
    /// identity by storing the Headers wrapper as a single `Global` and
    /// re-Localising it on every read (Globals lock to the same JS
    /// object across reborrows). The macro path does the same — the
    /// stored Global is set once at construction.
    #[v8_getter]
    fn headers<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        match self.headers.borrow().as_ref() {
            Some(g) => v8::Local::new(scope, g.clone()).into(),
            None => v8::null(scope).into(),
        }
    }

    /// `webSocket` — workerd extension; null when absent.
    #[v8_getter]
    #[v8_name = "webSocket"]
    fn web_socket<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        match self.web_socket.borrow().as_ref() {
            Some(g) => v8::Local::new(scope, g.clone()).into(),
            None => v8::null(scope).into(),
        }
    }

    /// `clone()` — Fetch §5.5. Build a fresh Response with copies of
    /// status / statusText / headers / type / url / redirected, and
    /// either tee the stream-bodied body or rebuild from the
    /// rewindable source. We can't go through `new Response(this)`
    /// since that constructor doesn't accept Response as input — so we
    /// build via `globalThis.Response(body, init)` and patch the
    /// type/url/redirected fields directly.
    #[v8_method]
    fn clone<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> Result<v8::Local<'s, v8::Object>, OpError> {
        if let Some(stream_g) = self.body.borrow().stream.borrow().clone() {
            let stream = v8::Local::new(scope, stream_g);
            let key = v8::String::new(scope, "locked").unwrap();
            if let Some(v) = stream.get(scope, key.into()) {
                if v.boolean_value(scope) {
                    return Err(OpError::type_error("Cannot clone a disturbed Response"));
                }
            }
        }

        // For Response, we can't go through `new Response(this)` since
        // the Response constructor doesn't accept Response as input.
        // Build the clone field-by-field via `globalThis.Response`.
        let global = scope.get_current_context().global(scope);
        let class_key = v8::String::new(scope, "Response").unwrap();
        let class_v = global.get(scope, class_key.into()).unwrap();
        let class_fn: v8::Local<v8::Function> = class_v.try_into().unwrap();

        // Body: tee if stream-bodied, re-build from source otherwise.
        let body_is_stream = matches!(
            self.body.borrow().source,
            Some(crate::fetch_body::body::BodySource::Stream)
        ) && self.body.borrow().stream.borrow().is_some();

        let body_arg: v8::Local<v8::Value> = if body_is_stream {
            let stream_g = self.body.borrow().stream.borrow().clone().unwrap();
            let stream = v8::Local::new(scope, stream_g);
            match tee_stream(scope, stream) {
                Some((left, right)) => {
                    *self.body.borrow().stream.borrow_mut() = Some(v8::Global::new(scope, left));
                    right.into()
                }
                None => {
                    return Err(OpError::type_error("Failed to tee Response body"));
                }
            }
        } else if let Some(src) = self.body.borrow().source.clone() {
            match src {
                crate::fetch_body::body::BodySource::Bytes(rc)
                | crate::fetch_body::body::BodySource::Blob(rc, _)
                | crate::fetch_body::body::BodySource::UrlSearchParams(rc)
                | crate::fetch_body::body::BodySource::FormData(rc, _) => {
                    let new_stream = crate::fetch_body::extract::build_byte_stream(scope, rc);
                    let stream_local = v8::Local::new(scope, new_stream);
                    stream_local.into()
                }
                crate::fetch_body::body::BodySource::Stream => v8::null(scope).into(),
            }
        } else {
            v8::null(scope).into()
        };

        // Build init: { status, statusText, headers }.
        let init = v8::Object::new(scope);
        {
            let key = v8::String::new(scope, "status").unwrap();
            let v = v8::Integer::new_from_unsigned(scope, self.status.get() as u32);
            init.set(scope, key.into(), v.into());
        }
        {
            let key = v8::String::new(scope, "statusText").unwrap();
            let v = v8::String::new(scope, &self.status_text.borrow()).unwrap();
            init.set(scope, key.into(), v.into());
        }
        if let Some(h_g) = self.headers.borrow().clone() {
            let key = v8::String::new(scope, "headers").unwrap();
            let v = v8::Local::new(scope, h_g);
            init.set(scope, key.into(), v.into());
        }

        let args2 = [body_arg, init.into()];
        let clone_obj = class_fn
            .new_instance(scope, &args2)
            .ok_or_else(|| OpError::error("Response constructor failed"))?;

        // Copy over `type`, `url`, `redirected`.
        if let Some(clone_raw) = state_ptr(scope, clone_obj) {
            let clone_state: &mut ResponseState = unsafe { &mut *clone_raw };
            clone_state.response_type.set(self.response_type.get());
            *clone_state.url.borrow_mut() = self.url.borrow().clone();
            clone_state.redirected.set(self.redirected.get());
        }

        Ok(clone_obj)
    }

    // ---------------------------------------------------------------
    // Static methods (WebIDL §3.7.4) — `Response.error()`,
    // `Response.redirect(url, status?)`, `Response.json(data, init?)`.
    // Migrated to `#[v8_static_method]` once the macro learned to
    // dispatch through `state_ty` under `#[v8_state_marker]` (the
    // `gen_static_callback` fix that introduced this commit).
    // ---------------------------------------------------------------

    /// `Response.error()` — Fetch §6.2.4. Returns a network-error
    /// response: type "error", status 0, empty headers (sealed
    /// immutable per step 4), null body.
    #[v8_static_method]
    fn error<'s>(
        scope: &mut v8::PinScope<'s, '_>,
    ) -> Result<v8::Local<'s, v8::Object>, OpError> {
        // We can't call our constructor with status=0 (range check
        // rejects). Build via a fresh instance (default 200/null body),
        // then patch state to "error"/0.
        let global = scope.get_current_context().global(scope);
        let class_key = v8::String::new(scope, "Response").unwrap();
        let class_v = global
            .get(scope, class_key.into())
            .ok_or_else(|| OpError::error("Response constructor missing"))?;
        let class_fn: v8::Local<v8::Function> = class_v
            .try_into()
            .map_err(|_| OpError::error("Response is not a function"))?;

        let null_v = v8::null(scope);
        let init = v8::Object::new(scope);
        let obj = class_fn
            .new_instance(scope, &[null_v.into(), init.into()])
            .ok_or_else(|| OpError::error("Response constructor failed"))?;

        let Some(raw) = state_ptr(scope, obj) else {
            return Ok(obj);
        };
        let state: &mut ResponseState = unsafe { &mut *raw };
        state.response_type.set(ResponseType::Error);
        state.status.set(0);
        *state.status_text.borrow_mut() = String::new();
        *state.body.borrow_mut() = BodyImpl::null();

        // Per Fetch §6.2.4 step 4: "Set response's headers' guard to
        // immutable." Seal the headers we minted above. WPT
        // response-static-error.any.js verifies this.
        if let Some(headers_g) = state.headers.borrow().clone() {
            let headers_local = v8::Local::new(scope, headers_g);
            crate::headers::seal_immutable(scope, headers_local);
        }
        Ok(obj)
    }

    /// `Response.redirect(url, status?)` — Fetch §6.2.4. Validates the
    /// URL and status, returns a redirect response with `Location`
    /// header set to the parsed URL.
    #[v8_static_method]
    fn redirect<'s>(
        scope: &mut v8::PinScope<'s, '_>,
        url: v8::Local<v8::Value>,
        status: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Object>, OpError> {
        let url_str = url.to_rust_string_lossy(scope);
        if ada_url::Url::parse(&url_str, None).is_err() {
            return Err(OpError::type_error("Invalid URL for Response.redirect"));
        }

        let status_code: u16 = if status.is_undefined() {
            302
        } else {
            let n = status.number_value(scope).unwrap_or(0.0);
            if n.is_nan() || n < 0.0 || n > 65535.0 {
                return Err(OpError::range_error("Invalid status code for redirect"));
            }
            n as u16
        };

        if !is_redirect_status(status_code) {
            return Err(OpError::range_error("Invalid status code for redirect"));
        }

        let global = scope.get_current_context().global(scope);
        let class_key = v8::String::new(scope, "Response").unwrap();
        let class_v = global
            .get(scope, class_key.into())
            .ok_or_else(|| OpError::error("Response constructor missing"))?;
        let class_fn: v8::Local<v8::Function> = class_v
            .try_into()
            .map_err(|_| OpError::error("Response is not a function"))?;

        let init = v8::Object::new(scope);
        let st_key = v8::String::new(scope, "status").unwrap();
        let st_val = v8::Integer::new_from_unsigned(scope, status_code as u32);
        init.set(scope, st_key.into(), st_val.into());

        let null_v = v8::null(scope);
        let obj = class_fn
            .new_instance(scope, &[null_v.into(), init.into()])
            .ok_or_else(|| OpError::error("Response constructor failed"))?;

        // Set Location header.
        if let Some(raw) = state_ptr(scope, obj) {
            let state: &ResponseState = unsafe { &*raw };
            if let Some(h_g) = state.headers.borrow().clone() {
                let h = v8::Local::new(scope, h_g);
                let set_key = v8::String::new(scope, "set").unwrap();
                if let Some(set_v) = h.get(scope, set_key.into()) {
                    if let Ok(set_fn) = v8::Local::<v8::Function>::try_from(set_v) {
                        let n = v8::String::new(scope, "Location").unwrap();
                        let v = v8::String::new(scope, &url_str).unwrap();
                        let _ = set_fn.call(scope, h.into(), &[n.into(), v.into()]);
                    }
                }
            }
        }

        Ok(obj)
    }

    /// `Response.json(data, init?)` — Fetch §5.5. Serializes `data`
    /// via JSON.stringify, builds a Response with the JSON body, and
    /// sets Content-Type to "application/json" unless init.headers
    /// already supplied one.
    ///
    /// Native fast path:
    ///   - `v8::json::stringify(scope, data)` directly (no globalThis.JSON
    ///     property walk + 2 V8 .get + JSON.stringify.call).
    ///   - When `init` has no `headers` member (the common case for AI
    ///     agents emitting `Response.json(obj)` / `Response.json(obj, {
    ///     status })`), we build the Response wrapper directly via the
    ///     kernel-side template slot — skipping the spec constructor's
    ///     ResponseInit dict walk, `extract_body` (we know the body is a
    ///     JSON string), `new Headers(init_v)` (we mint a 1-entry header
    ///     list with `build_kernel_headers_owned`), and `set_default_
    ///     content_type`'s has/set call pair.
    ///   - When `init.headers` is supplied we fall back to the JS Response
    ///     constructor path (`new Response(json_str, init)`) followed by
    ///     the existing default-Content-Type set, since the user's
    ///     headers can be a Headers instance, a record, or a sequence —
    ///     all of which the JS Headers constructor already handles.
    #[v8_static_method]
    fn json<'s>(
        scope: &mut v8::PinScope<'s, '_>,
        data: v8::Local<v8::Value>,
        init: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Object>, OpError> {
        // Step 1: serialize data to a JSON string. Per Fetch §5.5
        // Response.json + WHATWG Infra "serialize a JavaScript value to
        // JSON bytes":
        //   - JSON.stringify(value) — if it throws (circular ref, BigInt,
        //     throwing toJSON / getter), propagate verbatim.
        //   - If the result is undefined (Symbol / undefined / function
        //     value), throw a TypeError.
        //   - Otherwise, UTF-8 encode the resulting string.
        //
        // We use rusty_v8's `v8::json::stringify` which is a direct C++
        // shim into V8's JSON::Stringify — no globalThis.JSON property
        // walk, no JSON.stringify.call. The TryCatch captures any pending
        // exception and re-throws via OpError::js_value so callers see
        // the original Error subclass / `e.code` / stack verbatim
        // (matching WPT response-static-json's CustomError test).
        enum StringifyOutcome {
            Ok(v8::Global<v8::String>),
            Threw(v8::Global<v8::Value>),
        }
        let outcome: StringifyOutcome = {
            v8::tc_scope!(let tc, scope);
            match v8::json::stringify(tc, data) {
                Some(s) => StringifyOutcome::Ok(v8::Global::new(tc, s)),
                None => {
                    let exc = tc.exception().unwrap_or_else(|| {
                        let m = v8::String::new(tc, "JSON.stringify threw").unwrap();
                        v8::Exception::error(tc, m)
                    });
                    StringifyOutcome::Threw(v8::Global::new(tc, exc))
                }
            }
        };
        let json_str: v8::Local<v8::String> = match outcome {
            StringifyOutcome::Ok(g) => v8::Local::new(scope, &g),
            StringifyOutcome::Threw(g) => {
                let exc = v8::Local::new(scope, &g);
                return Err(OpError::js_value(scope, exc, "JSON.stringify threw"));
            }
        };
        // V8's JSON::Stringify returns an empty (null) Local for inputs
        // that JSON.stringify(value) maps to JS `undefined` — i.e.
        // top-level Symbol / undefined / function values. The Some-case
        // check below covers the C++ shim's "returned non-null" path;
        // the .is_undefined() check is defensive in case V8 ever returns
        // an actual JS `undefined` string-typed Value.
        if json_str.is_undefined() {
            return Err(OpError::type_error(
                "Response.json: data is not JSON-serializable",
            ));
        }

        // Step 2: parse status / statusText / detect headers presence on
        // init. We walk `init` once to read all three; default status is
        // 200 / statusText is "".
        let init_obj: Option<v8::Local<v8::Object>> = if init.is_null_or_undefined() {
            None
        } else {
            v8::Local::<v8::Object>::try_from(init).ok()
        };

        let mut status: u16 = 200;
        let mut status_text = String::new();
        let mut init_has_headers = false;

        if let Some(o) = init_obj {
            // status
            if let Some(v) = read_init_member(scope, o, "status") {
                let n = v.number_value(scope).unwrap_or(f64::NAN);
                let in_range = n == 101.0 || (200.0..=599.0).contains(&n);
                if n.is_nan() || !in_range {
                    return Err(OpError::range_error("Invalid status code"));
                }
                status = n as u16;
            }
            // statusText
            if let Some(v) = read_init_member(scope, o, "statusText") {
                let s = v.to_rust_string_lossy(scope);
                if !is_valid_reason_phrase(&s) {
                    return Err(OpError::type_error("Invalid statusText"));
                }
                status_text = s;
            }
            // detect headers presence (don't extract — handled below).
            // We use the same read_init_member that treats undefined as
            // missing, so `{ headers: undefined }` correctly flows through
            // the no-headers fast path.
            init_has_headers = read_init_member(scope, o, "headers").is_some();
        }

        // Step 3: null-body status check. WPT response-static-json
        // explicitly tests `Response.json("hello world", { status: 204 })`
        // and expects TypeError (the JSON-encoded body is non-empty, so
        // any null-body status is invalid).
        if is_null_body_status(status) {
            return Err(OpError::type_error(
                "Response with null body status cannot have a body",
            ));
        }

        // Step 4: build the Response. Two paths:
        //   - No init.headers → kernel fast path (build_kernel_response-
        //     style direct wrapper alloc with a 1-entry Content-Type
        //     header list).
        //   - init.headers → fall back to the JS Response constructor so
        //     the existing Headers WebIDL-union dispatch handles
        //     instance/record/sequence inputs. We then apply the default
        //     Content-Type via the existing set_default_content_type
        //     helper.
        if !init_has_headers {
            return build_response_json_fast(scope, json_str, status, status_text);
        }

        // Slow path: user supplied init.headers. Reuse the JS Response
        // constructor so the spec WebIDL-union dispatch on HeadersInit
        // covers Headers / record / sequence-of-pairs verbatim.
        let global = scope.get_current_context().global(scope);
        let class_key = v8::String::new(scope, "Response").unwrap();
        let class_v = global
            .get(scope, class_key.into())
            .ok_or_else(|| OpError::error("Response constructor missing"))?;
        let class_fn: v8::Local<v8::Function> = class_v
            .try_into()
            .map_err(|_| OpError::error("Response is not a function"))?;

        let obj = class_fn
            .new_instance(scope, &[json_str.into(), init])
            .ok_or_else(|| OpError::error("Response constructor failed"))?;

        // Set Content-Type to "application/json" unless the user already
        // supplied one in init.headers. set_default_content_type is a
        // has-then-set-if-absent helper.
        if let Some(raw) = state_ptr(scope, obj) {
            let state: &ResponseState = unsafe { &*raw };
            if let Some(h_g) = state.headers.borrow().clone() {
                let h = v8::Local::new(scope, h_g);
                set_default_content_type(scope, h, "application/json");
            }
        }
        Ok(obj)
    }
}

// ---------------------------------------------------------------------------
// Hand-rolled helpers used by the constructor + static methods
// ---------------------------------------------------------------------------

/// Read a single property from the init object. Returns `None` for
/// missing keys / undefined values; `Some(v)` for explicit null. Used
/// by raw v8::Value passthroughs (`headers` / `webSocket`) where
/// distinguishing missing-vs-null matters for the spec algorithm.
fn read_init_member<'s>(
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

fn is_valid_reason_phrase(s: &str) -> bool {
    // RFC 7230: reason-phrase = *( HTAB / SP / VCHAR / obs-text ).
    //   HTAB     = 0x09
    //   SP       = 0x20
    //   VCHAR    = 0x21..=0x7E
    //   obs-text = 0x80..=0xFF (per RFC 7230 §3.2.6)
    // Per Fetch spec ByteString conversion of statusText, we accept
    // each ByteString byte if it satisfies the above. Reject CR/LF/NUL.
    // WPT response-init-001 explicitly tests `String.fromCharCode(0x80)`.
    s.bytes().all(|b| match b {
        0x09 | 0x20 => true,
        0x21..=0x7E => true,
        0x80..=0xFF => true,
        _ => false,
    })
}

fn build_response_headers<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    init_headers: Option<v8::Local<v8::Value>>,
) -> Result<v8::Local<'s, v8::Object>, String> {
    let global = scope.get_current_context().global(scope);
    let headers_class_key = v8::String::new(scope, "Headers").unwrap();
    let class_v = global
        .get(scope, headers_class_key.into())
        .ok_or_else(|| "Headers class missing".to_string())?;
    let class_fn: v8::Local<v8::Function> = class_v
        .try_into()
        .map_err(|_| "Headers is not a function".to_string())?;
    let init: v8::Local<v8::Value> = match init_headers {
        Some(v) if !v.is_undefined() => v,
        _ => v8::undefined(scope).into(),
    };
    let args = [init];
    class_fn
        .new_instance(scope, &args)
        .ok_or_else(|| "Headers constructor failed".to_string())
}

fn set_default_content_type(scope: &mut v8::PinScope, headers: v8::Local<v8::Object>, ct: &str) {
    let has_key = v8::String::new(scope, "has").unwrap();
    let has_v = match headers.get(scope, has_key.into()) {
        Some(v) => v,
        None => return,
    };
    let has_fn: v8::Local<v8::Function> = match has_v.try_into() {
        Ok(f) => f,
        Err(_) => return,
    };
    let arg = v8::String::new(scope, "Content-Type").unwrap();
    let exists = match has_fn.call(scope, headers.into(), &[arg.into()]) {
        Some(v) => v.boolean_value(scope),
        None => false,
    };
    if exists {
        return;
    }
    let set_key = v8::String::new(scope, "set").unwrap();
    let set_v = match headers.get(scope, set_key.into()) {
        Some(v) => v,
        None => return,
    };
    let set_fn: v8::Local<v8::Function> = match set_v.try_into() {
        Ok(f) => f,
        Err(_) => return,
    };
    let n = v8::String::new(scope, "Content-Type").unwrap();
    let v = v8::String::new(scope, ct).unwrap();
    let _ = set_fn.call(scope, headers.into(), &[n.into(), v.into()]);
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

