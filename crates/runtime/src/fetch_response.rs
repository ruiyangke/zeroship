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
//!     stripped, per D-30. v1 ships empty by default.
//!   - redirected: bool
//!   - ok: bool — derived (status in 200..300)
//!   - headers: Global<Object>
//!   - web_socket: Option<Global<Object>> — workerd extension preserved
//!     for the gateway upgrade path (D-13).
//!
//! ## Static methods
//!
//! - `Response.error()` returns a network-error response.
//! - `Response.redirect(url, status?)` validates status and returns
//!   a redirect response.
//! - `Response.json(data, init?)` serializes via JSON.stringify and
//!   sets Content-Type "application/json".

use std::cell::RefCell;

use crate::fetch_body::body::{Body, BodyImpl};
use crate::fetch_body::consumers::{install_body_methods, BodyMarker};
use crate::fetch_body::extract::extract_body;

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
// ResponseState
// ---------------------------------------------------------------------------

#[allow(missing_debug_implementations)]
pub struct ResponseState {
    pub body: RefCell<BodyImpl>,
    pub status: RefCell<u16>,
    pub status_text: RefCell<String>,
    pub response_type: RefCell<String>,
    pub url: RefCell<String>,
    pub redirected: RefCell<bool>,
    pub headers: RefCell<Option<v8::Global<v8::Object>>>,
    pub web_socket: RefCell<Option<v8::Global<v8::Object>>>,
}

impl Default for ResponseState {
    fn default() -> Self {
        ResponseState {
            body: RefCell::new(BodyImpl::null()),
            status: RefCell::new(200),
            status_text: RefCell::new(String::new()),
            response_type: RefCell::new("default".to_string()),
            url: RefCell::new(String::new()),
            redirected: RefCell::new(false),
            headers: RefCell::new(None),
            web_socket: RefCell::new(None),
        }
    }
}

// ---------------------------------------------------------------------------
// Body trait impl
// ---------------------------------------------------------------------------

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

fn state_ptr(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>) -> Option<*mut ResponseState> {
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
// install_global — hand-rolled (no macro), same pattern as Request.
// ---------------------------------------------------------------------------

pub fn install_global(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
    let class_tmpl = v8::FunctionTemplate::new(scope, response_constructor_callback);
    let class_name = v8::String::new(scope, "Response").unwrap();
    class_tmpl.set_class_name(class_name);
    class_tmpl
        .instance_template(scope)
        .set_internal_field_count(1);

    let class_fn = class_tmpl.get_function(scope).unwrap();

    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let our_proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    let our_proto: v8::Local<v8::Object> = our_proto_v.try_into().unwrap();

    install_response_getters(scope, our_proto);
    install_method(scope, our_proto, "clone", response_clone_callback);
    install_body_methods::<Response>(scope, our_proto);

    // Symbol.toStringTag — read-only, non-enumerable, configurable.
    let tag_sym = v8::Symbol::get_to_string_tag(scope);
    let tag_value = v8::String::new(scope, "Response").unwrap();
    let mut tag_desc = v8::PropertyDescriptor::new_from_value(tag_value.into());
    tag_desc.set_configurable(true);
    tag_desc.set_enumerable(false);
    our_proto.define_property(scope, tag_sym.into(), &tag_desc);

    // Static methods on the constructor function.
    install_static(scope, class_fn, "error", static_error_callback);
    install_static(scope, class_fn, "redirect", static_redirect_callback);
    install_static(scope, class_fn, "json", static_json_callback);

    let key = v8::String::new(scope, "Response").unwrap();
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

fn install_static(
    scope: &mut v8::PinScope,
    ctor: v8::Local<v8::Function>,
    name: &str,
    cb: impl v8::MapFnTo<v8::FunctionCallback>,
) {
    let key = v8::String::new(scope, name).unwrap();
    let tmpl = v8::FunctionTemplate::new(scope, cb);
    let func = tmpl.get_function(scope).unwrap();
    ctor.set(scope, key.into(), func.into());
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

fn install_response_getters(scope: &mut v8::PinScope, proto: v8::Local<v8::Object>) {
    install_getter(scope, proto, "type", type_getter);
    install_getter(scope, proto, "url", url_getter);
    install_getter(scope, proto, "redirected", redirected_getter);
    install_getter(scope, proto, "status", status_getter);
    install_getter(scope, proto, "ok", ok_getter);
    install_getter(scope, proto, "statusText", status_text_getter);
    install_getter(scope, proto, "headers", headers_getter);
    install_getter(scope, proto, "webSocket", web_socket_getter);
}

// ---------------------------------------------------------------------------
// Constructor: new Response(body?, init?)
// ---------------------------------------------------------------------------

fn response_constructor_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let this_obj = args.this();

    let body_v = args.get(0);
    let init_v = args.get(1);

    let mut state = ResponseState::default();

    let init_obj: Option<v8::Local<v8::Object>> = if init_v.is_undefined() {
        None
    } else {
        v8::Local::<v8::Object>::try_from(init_v).ok()
    };

    // Step 1: status (default 200). Range 200..=599.
    if let Some(init) = init_obj {
        let key = v8::String::new(scope, "status").unwrap();
        if let Some(s_v) = init.get(scope, key.into()) {
            if !s_v.is_undefined() {
                let n = s_v.number_value(scope).unwrap_or(0.0);
                if n.is_nan() || n < 200.0 || n > 599.0 {
                    let m = v8::String::new(scope, "Invalid status code").unwrap();
                    let exc = v8::Exception::range_error(scope, m);
                    scope.throw_exception(exc);
                    return;
                }
                *state.status.borrow_mut() = n as u16;
            }
        }
    }

    // Step 2: statusText. Validate per HTTP/1.1 reason-phrase ABNF
    // (HTAB / SP / VCHAR / obs-text). Reject CR/LF/non-ASCII control.
    if let Some(init) = init_obj {
        let key = v8::String::new(scope, "statusText").unwrap();
        if let Some(s_v) = init.get(scope, key.into()) {
            if !s_v.is_undefined() {
                let s = s_v.to_rust_string_lossy(scope);
                if !is_valid_reason_phrase(&s) {
                    let m = v8::String::new(scope, "Invalid statusText").unwrap();
                    let exc = v8::Exception::type_error(scope, m);
                    scope.throw_exception(exc);
                    return;
                }
                *state.status_text.borrow_mut() = s;
            }
        }
    }

    // Build headers: from init.headers if present, else empty.
    let headers_obj = build_response_headers(scope, init_obj);
    let headers_obj = match headers_obj {
        Ok(h) => h,
        Err(msg) => {
            let m = v8::String::new(scope, &msg).unwrap();
            let exc = v8::Exception::type_error(scope, m);
            scope.throw_exception(exc);
            return;
        }
    };

    // webSocket extension — preserve as-is for the gateway path.
    if let Some(init) = init_obj {
        let key = v8::String::new(scope, "webSocket").unwrap();
        if let Some(ws_v) = init.get(scope, key.into()) {
            if !ws_v.is_null_or_undefined() {
                if let Ok(o) = v8::Local::<v8::Object>::try_from(ws_v) {
                    *state.web_socket.borrow_mut() = Some(v8::Global::new(scope, o));
                }
            }
        }
    }

    // Step 7: null-body status check.
    let status_now = *state.status.borrow();
    let body_is_null = body_v.is_null_or_undefined();
    if !body_is_null && is_null_body_status(status_now) {
        let m = v8::String::new(
            scope,
            "Response with null body status cannot have a body",
        )
        .unwrap();
        let exc = v8::Exception::type_error(scope, m);
        scope.throw_exception(exc);
        return;
    }

    // Body extraction.
    if !body_is_null {
        match extract_body(scope, body_v, false) {
            Ok(extracted) => {
                *state.body.borrow_mut() = extracted.body;
                if let Some(ct) = extracted.content_type {
                    set_default_content_type(scope, headers_obj, &ct);
                }
            }
            Err(e) => {
                let m = v8::String::new(scope, &e.message).unwrap();
                let exc = match e.kind {
                    crate::state::OpErrorKind::TypeError => v8::Exception::type_error(scope, m),
                    crate::state::OpErrorKind::RangeError => v8::Exception::range_error(scope, m),
                    _ => v8::Exception::error(scope, m),
                };
                scope.throw_exception(exc);
                return;
            }
        }
    }

    *state.headers.borrow_mut() = Some(v8::Global::new(scope, headers_obj));

    // Box up + install.
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
    init_obj: Option<v8::Local<v8::Object>>,
) -> Result<v8::Local<'s, v8::Object>, String> {
    let global = scope.get_current_context().global(scope);
    let headers_class_key = v8::String::new(scope, "Headers").unwrap();
    let class_v = global
        .get(scope, headers_class_key.into())
        .ok_or_else(|| "Headers class missing".to_string())?;
    let class_fn: v8::Local<v8::Function> = class_v
        .try_into()
        .map_err(|_| "Headers is not a function".to_string())?;
    let init: v8::Local<v8::Value> = if let Some(init_o) = init_obj {
        let key = v8::String::new(scope, "headers").unwrap();
        match init_o.get(scope, key.into()) {
            Some(v) if !v.is_undefined() => v,
            _ => v8::undefined(scope).into(),
        }
    } else {
        v8::undefined(scope).into()
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

// ---------------------------------------------------------------------------
// Getter callbacks
// ---------------------------------------------------------------------------

fn brand_check(scope: &mut v8::PinScope, this: v8::Local<v8::Object>) -> Option<*mut ResponseState> {
    state_ptr(scope, this).or_else(|| {
        let m = v8::String::new(scope, "Illegal invocation").unwrap();
        let exc = v8::Exception::type_error(scope, m);
        scope.throw_exception(exc);
        None
    })
}

fn type_getter(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(raw) = brand_check(scope, args.this()) else { return };
    let state: &ResponseState = unsafe { &*raw };
    let s = state.response_type.borrow().clone();
    let v = v8::String::new(scope, &s).unwrap();
    rv.set(v.into());
}

fn url_getter(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(raw) = brand_check(scope, args.this()) else { return };
    let state: &ResponseState = unsafe { &*raw };
    let s = state.url.borrow().clone();
    let v = v8::String::new(scope, &s).unwrap();
    rv.set(v.into());
}

fn redirected_getter(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(raw) = brand_check(scope, args.this()) else { return };
    let state: &ResponseState = unsafe { &*raw };
    rv.set(v8::Boolean::new(scope, *state.redirected.borrow()).into());
}

fn status_getter(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(raw) = brand_check(scope, args.this()) else { return };
    let state: &ResponseState = unsafe { &*raw };
    rv.set(v8::Integer::new_from_unsigned(scope, *state.status.borrow() as u32).into());
}

fn ok_getter(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(raw) = brand_check(scope, args.this()) else { return };
    let state: &ResponseState = unsafe { &*raw };
    let s = *state.status.borrow();
    rv.set(v8::Boolean::new(scope, s >= 200 && s < 300).into());
}

fn status_text_getter(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(raw) = brand_check(scope, args.this()) else { return };
    let state: &ResponseState = unsafe { &*raw };
    let s = state.status_text.borrow().clone();
    let v = v8::String::new(scope, &s).unwrap();
    rv.set(v.into());
}

fn headers_getter(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(raw) = brand_check(scope, args.this()) else { return };
    let state: &ResponseState = unsafe { &*raw };
    match state.headers.borrow().as_ref() {
        Some(g) => {
            let v = v8::Local::new(scope, g.clone());
            rv.set(v.into());
        }
        None => rv.set(v8::null(scope).into()),
    }
}

fn web_socket_getter(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(raw) = brand_check(scope, args.this()) else { return };
    let state: &ResponseState = unsafe { &*raw };
    match state.web_socket.borrow().as_ref() {
        Some(g) => {
            let v = v8::Local::new(scope, g.clone());
            rv.set(v.into());
        }
        None => rv.set(v8::null(scope).into()),
    }
}

// ---------------------------------------------------------------------------
// clone()
// ---------------------------------------------------------------------------

fn response_clone_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    let Some(raw) = brand_check(scope, this) else { return };
    let state: &ResponseState = unsafe { &*raw };

    if let Some(stream_g) = state.body.borrow().stream.clone() {
        let stream = v8::Local::new(scope, stream_g);
        let key = v8::String::new(scope, "locked").unwrap();
        if let Some(v) = stream.get(scope, key.into()) {
            if v.boolean_value(scope) {
                let m = v8::String::new(scope, "Cannot clone a disturbed Response").unwrap();
                let exc = v8::Exception::type_error(scope, m);
                scope.throw_exception(exc);
                return;
            }
        }
    }

    // For Response, we can't go through `new Response(this)` since
    // Response constructor doesn't accept Response as input. Build the
    // clone field-by-field.
    let global = scope.get_current_context().global(scope);
    let class_key = v8::String::new(scope, "Response").unwrap();
    let class_v = global.get(scope, class_key.into()).unwrap();
    let class_fn: v8::Local<v8::Function> = class_v.try_into().unwrap();

    // Body: tee if stream-bodied, re-build from source otherwise.
    let body_is_stream = state.body.borrow().stream.is_some()
        && matches!(
            state.body.borrow().source,
            Some(crate::fetch_body::body::BodySource::Stream)
        );

    let body_arg: v8::Local<v8::Value> = if body_is_stream {
        let stream_g = state.body.borrow().stream.clone().unwrap();
        let stream = v8::Local::new(scope, stream_g);
        match tee_stream(scope, stream) {
            Some((left, right)) => {
                state.body.borrow_mut().stream = Some(v8::Global::new(scope, left));
                right.into()
            }
            None => {
                let m = v8::String::new(scope, "Failed to tee Response body").unwrap();
                let exc = v8::Exception::type_error(scope, m);
                scope.throw_exception(exc);
                return;
            }
        }
    } else if let Some(src) = state.body.borrow().source.clone() {
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
        let v = v8::Integer::new_from_unsigned(scope, *state.status.borrow() as u32);
        init.set(scope, key.into(), v.into());
    }
    {
        let key = v8::String::new(scope, "statusText").unwrap();
        let v = v8::String::new(scope, &state.status_text.borrow()).unwrap();
        init.set(scope, key.into(), v.into());
    }
    if let Some(h_g) = state.headers.borrow().clone() {
        let key = v8::String::new(scope, "headers").unwrap();
        let v = v8::Local::new(scope, h_g);
        init.set(scope, key.into(), v.into());
    }

    let args2 = [body_arg, init.into()];
    let result = class_fn.new_instance(scope, &args2);
    let Some(clone_obj) = result else { return };

    // Copy over `type`, `url`, `redirected`.
    let Some(clone_raw) = state_ptr(scope, clone_obj) else {
        rv.set(clone_obj.into());
        return;
    };
    let clone_state: &mut ResponseState = unsafe { &mut *clone_raw };
    *clone_state.response_type.borrow_mut() = state.response_type.borrow().clone();
    *clone_state.url.borrow_mut() = state.url.borrow().clone();
    *clone_state.redirected.borrow_mut() = *state.redirected.borrow();

    rv.set(clone_obj.into());
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

// ---------------------------------------------------------------------------
// Static methods
// ---------------------------------------------------------------------------

fn static_error_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    // Build a Response via our constructor with empty init then patch
    // type/status to "error"/0.
    let global = scope.get_current_context().global(scope);
    let class_key = v8::String::new(scope, "Response").unwrap();
    let class_v = global.get(scope, class_key.into()).unwrap();
    let class_fn: v8::Local<v8::Function> = class_v.try_into().unwrap();

    // We can't call our constructor with status=0 (range check rejects).
    // Build via a fresh instance bypassing the constructor: invoke
    // `class_fn` with a dummy 200/null body, then patch state.
    let null_v = v8::null(scope);
    let init = v8::Object::new(scope);
    let result = class_fn.new_instance(scope, &[null_v.into(), init.into()]);
    let Some(obj) = result else { return };

    let Some(raw) = state_ptr(scope, obj) else {
        rv.set(obj.into());
        return;
    };
    let state: &mut ResponseState = unsafe { &mut *raw };
    *state.response_type.borrow_mut() = "error".to_string();
    *state.status.borrow_mut() = 0;
    *state.status_text.borrow_mut() = String::new();
    *state.body.borrow_mut() = BodyImpl::null();

    // Per spec: error response's headers list is empty + immutable.
    // The Headers we minted above already exist — empty by default.
    // (v1 has no immutable guard; deferred.)
    rv.set(obj.into());
}

fn static_redirect_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let url_v = args.get(0);
    let status_v = args.get(1);

    let url_str = url_v.to_rust_string_lossy(scope);
    // Parse URL.
    if ada_url::Url::parse(&url_str, None).is_err() {
        let m = v8::String::new(scope, "Invalid URL for Response.redirect").unwrap();
        let exc = v8::Exception::type_error(scope, m);
        scope.throw_exception(exc);
        return;
    }

    let status: u16 = if status_v.is_undefined() {
        302
    } else {
        let n = status_v.number_value(scope).unwrap_or(0.0);
        if n.is_nan() || n < 0.0 || n > 65535.0 {
            let m = v8::String::new(scope, "Invalid status code for redirect").unwrap();
            let exc = v8::Exception::range_error(scope, m);
            scope.throw_exception(exc);
            return;
        }
        n as u16
    };

    if !is_redirect_status(status) {
        let m = v8::String::new(scope, "Invalid status code for redirect").unwrap();
        let exc = v8::Exception::range_error(scope, m);
        scope.throw_exception(exc);
        return;
    }

    let global = scope.get_current_context().global(scope);
    let class_key = v8::String::new(scope, "Response").unwrap();
    let class_v = global.get(scope, class_key.into()).unwrap();
    let class_fn: v8::Local<v8::Function> = class_v.try_into().unwrap();

    let init = v8::Object::new(scope);
    let st_key = v8::String::new(scope, "status").unwrap();
    let st_val = v8::Integer::new_from_unsigned(scope, status as u32);
    init.set(scope, st_key.into(), st_val.into());

    let null_v = v8::null(scope);
    let result = class_fn.new_instance(scope, &[null_v.into(), init.into()]);
    let Some(obj) = result else { return };

    // Set Location header.
    let Some(raw) = state_ptr(scope, obj) else {
        rv.set(obj.into());
        return;
    };
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

    rv.set(obj.into());
}

fn static_json_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let data_v = args.get(0);
    let init_v = args.get(1);

    // Per Fetch §5.5 Response.json step 1: "serialize a JavaScript
    // value to JSON bytes". Per the WHATWG Infra spec, this:
    //   1. Sets `string` to JSON.stringify(value).
    //   2. If `string` is undefined (i.e., `value` is a Symbol or
    //      undefined or contains non-encodables), throw TypeError.
    //   3. Otherwise, UTF-8 encode `string`.
    //
    // V8's JSON.stringify behaviour:
    //   - Symbol value, undefined value, function value → returns
    //     undefined (a JS undefined, NOT a throw).
    //   - Circular reference, BigInt → throws TypeError.
    //   - Object with throwing `toJSON` / getter → throws that error.
    //
    // v8::json::stringify mirrors this: it returns `Some(JsString)`
    // when JSON.stringify returned a string, `None` when JSON.stringify
    // threw. To match the spec we need a third case: when JSON.stringify
    // returned `undefined` (no exception), throw TypeError ourselves.
    //
    // We test by running `JSON.stringify(value)` and checking the result.
    let json_result_v = {
        let global = scope.get_current_context().global(scope);
        let json_key = v8::String::new(scope, "JSON").unwrap();
        let json_obj_v = global.get(scope, json_key.into()).unwrap();
        let Ok(json_obj) = v8::Local::<v8::Object>::try_from(json_obj_v) else {
            return;
        };
        let stringify_key = v8::String::new(scope, "stringify").unwrap();
        let Some(stringify_v) = json_obj.get(scope, stringify_key.into()) else {
            return;
        };
        let Ok(stringify_fn) = v8::Local::<v8::Function>::try_from(stringify_v) else {
            return;
        };
        match stringify_fn.call(scope, json_obj.into(), &[data_v]) {
            Some(v) => v,
            None => {
                // JSON.stringify threw — exception is on the isolate,
                // propagate.
                return;
            }
        }
    };
    if json_result_v.is_undefined() {
        let m =
            v8::String::new(scope, "Response.json: data is not JSON-serializable").unwrap();
        let exc = v8::Exception::type_error(scope, m);
        scope.throw_exception(exc);
        return;
    }
    let Ok(json_str) = v8::Local::<v8::String>::try_from(json_result_v) else {
        // Defensive: should not happen.
        return;
    };

    let global = scope.get_current_context().global(scope);
    let class_key = v8::String::new(scope, "Response").unwrap();
    let class_v = global.get(scope, class_key.into()).unwrap();
    let class_fn: v8::Local<v8::Function> = class_v.try_into().unwrap();

    let result = class_fn.new_instance(scope, &[json_str.into(), init_v]);
    let Some(obj) = result else { return };

    // Per Fetch §5.5 Response.json: invoke "initialize a response"
    // with the body content-type set to "application/json". The
    // "initialize a response" algorithm sets Content-Type ONLY IF the
    // user's init.headers didn't supply one. The Response constructor
    // already runs `set_default_content_type` when extracting the
    // body — but it uses the body-derived MIME (which for our string
    // path is "text/plain;charset=UTF-8"). We replace that with
    // "application/json" UNLESS init.headers explicitly provided a
    // Content-Type.
    if let Some(raw) = state_ptr(scope, obj) {
        let state: &ResponseState = unsafe { &*raw };
        if let Some(h_g) = state.headers.borrow().clone() {
            let h = v8::Local::new(scope, h_g);
            let user_supplied_ct =
                init_supplied_content_type(scope, init_v).unwrap_or(false);
            if !user_supplied_ct {
                let set_key = v8::String::new(scope, "set").unwrap();
                if let Some(set_v) = h.get(scope, set_key.into()) {
                    if let Ok(set_fn) = v8::Local::<v8::Function>::try_from(set_v) {
                        let n = v8::String::new(scope, "Content-Type").unwrap();
                        let v = v8::String::new(scope, "application/json").unwrap();
                        let _ = set_fn.call(scope, h.into(), &[n.into(), v.into()]);
                    }
                }
            }
        }
    }

    rv.set(obj.into());
}

/// Inspect init?.headers to see whether the user supplied a
/// Content-Type. Used by Response.json so we don't clobber a
/// user-provided MIME with the default "application/json".
fn init_supplied_content_type(
    scope: &mut v8::PinScope,
    init_v: v8::Local<v8::Value>,
) -> Option<bool> {
    if init_v.is_null_or_undefined() {
        return Some(false);
    }
    let init_obj: v8::Local<v8::Object> = init_v.try_into().ok()?;
    let headers_key = v8::String::new(scope, "headers")?;
    let h_v = init_obj.get(scope, headers_key.into())?;
    if h_v.is_null_or_undefined() {
        return Some(false);
    }
    // h_v can be a Headers instance OR a record OR a sequence-of-pairs.
    // We need to check each shape for "Content-Type" (case-insensitively).
    if let Ok(h_obj) = v8::Local::<v8::Object>::try_from(h_v) {
        // Try `headers.has("Content-Type")` first (Headers instance).
        let has_key = v8::String::new(scope, "has")?;
        if let Some(has_v) = h_obj.get(scope, has_key.into()) {
            if let Ok(has_fn) = v8::Local::<v8::Function>::try_from(has_v) {
                let arg = v8::String::new(scope, "Content-Type")?;
                if let Some(r) = has_fn.call(scope, h_obj.into(), &[arg.into()]) {
                    if r.boolean_value(scope) {
                        return Some(true);
                    }
                }
            }
        }
        // Plain object record: walk own properties case-insensitively.
        if let Some(names) = h_obj.get_own_property_names(scope, Default::default()) {
            for i in 0..names.length() {
                let Some(k) = names.get_index(scope, i) else { continue };
                let key_str = k.to_rust_string_lossy(scope);
                if key_str.eq_ignore_ascii_case("content-type") {
                    return Some(true);
                }
            }
        }
    }
    Some(false)
}
