//! `ReadableStreamBYOBRequest` — spec §3.8.
//!
//! IDL (§3.8):
//! ```webidl
//! [Exposed=*]
//! interface ReadableStreamBYOBRequest {
//!   readonly attribute ArrayBufferView? view;
//!   undefined respond([EnforceRange] unsigned long long bytesWritten);
//!   undefined respondWithNewView(ArrayBufferView view);
//! };
//! ```
//!
//! Internal slots (per design §II.7):
//! - `[[controller]]` — V8 priv sym `[[controllerObj]]`
//! - `[[view]]`       — V8 priv sym `[[viewObj]]`
//!
//! Per critic #19: there's NO RustState struct beyond a marker —
//! all slot data lives in V8 priv syms on the wrapper. This eliminates
//! the nested-RefCell deadlock risk InvalidateBYOBRequest could otherwise
//! create when the controller is mid-borrow.

use crate::streams::slots::{self, VIEW};

const CONTROLLER_OBJ_SLOT: &str = "[[controllerObj]]";
const TAG_SLOT: &str = "[[byobRequest.tag]]";

// ---------------------------------------------------------------------------
// State (empty marker — see module doc)
// ---------------------------------------------------------------------------

/// `Box<BYOBRequestState>` lives in the wrapper's V8 internal field 0.
/// Empty per spec — both slots are V8 priv syms. The Box is here so the
/// wrapper has the same internal-field shape as other stream classes
/// (one External pointing at a Box).
#[allow(missing_debug_implementations)]
pub struct BYOBRequestState {
    _marker: (),
}

// ---------------------------------------------------------------------------
// V8 wrapper helpers
// ---------------------------------------------------------------------------

pub fn is_byob_request(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>) -> bool {
    if obj
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
        .map(|e| e.value().is_null())
        .unwrap_or(true)
    {
        return false;
    }
    !slots::slot_is_empty(scope, obj, TAG_SLOT)
}

// ---------------------------------------------------------------------------
// Class template
// ---------------------------------------------------------------------------

fn request_class_template<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::FunctionTemplate> {
    let ctor_tmpl = v8::FunctionTemplate::new(scope, illegal_constructor_callback);
    let class_name = v8::String::new(scope, "ReadableStreamBYOBRequest").unwrap();
    ctor_tmpl.set_class_name(class_name);
    ctor_tmpl
        .instance_template(scope)
        .set_internal_field_count(1);

    let proto = ctor_tmpl.prototype_template(scope);

    {
        let key = v8::String::new(scope, "view").unwrap();
        let getter_tmpl = v8::FunctionTemplate::new(scope, view_getter_callback);
        proto.set_accessor_property(
            key.into(),
            Some(getter_tmpl.into()),
            None,
            v8::PropertyAttribute::NONE,
        );
    }
    install_proto_method(scope, proto, "respond", respond_method_callback);
    install_proto_method(
        scope,
        proto,
        "respondWithNewView",
        respond_with_new_view_callback,
    );

    let tag_sym = v8::Symbol::get_to_string_tag(scope);
    let tag_value = v8::String::new(scope, "ReadableStreamBYOBRequest").unwrap();
    proto.set_with_attr(
        tag_sym.into(),
        tag_value.into(),
        v8::PropertyAttribute::READ_ONLY | v8::PropertyAttribute::DONT_ENUM,
    );

    ctor_tmpl
}

fn install_proto_method(
    scope: &mut v8::PinScope,
    proto: v8::Local<v8::ObjectTemplate>,
    name: &str,
    cb: impl v8::MapFnTo<v8::FunctionCallback>,
) {
    let key = v8::String::new(scope, name).unwrap();
    let tmpl = v8::FunctionTemplate::new(scope, cb);
    proto.set(key.into(), tmpl.into());
}

fn illegal_constructor_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let msg = v8::String::new(scope, "ReadableStreamBYOBRequest: illegal constructor").unwrap();
    let exc = v8::Exception::type_error(scope, msg);
    scope.throw_exception(exc);
}

// ---------------------------------------------------------------------------
// build — internal constructor used by ByteController.GetBYOBRequest
// ---------------------------------------------------------------------------

/// Build a fresh BYOBRequest wrapper bound to `controller` + `view`.
pub fn build<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller: v8::Local<v8::Object>,
    view: v8::Local<v8::ArrayBufferView>,
) -> v8::Local<'s, v8::Object> {
    let tmpl = request_class_template(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let req = inst_tmpl.new_instance(scope).unwrap();
    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    req.set_prototype(scope, proto_v);

    let state = BYOBRequestState { _marker: () };
    let boxed = Box::new(state);
    let raw_ptr = Box::into_raw(boxed);
    let raw_addr = raw_ptr as usize;
    let ext = v8::External::new(scope, raw_ptr as *mut std::ffi::c_void);
    req.set_internal_field(0, ext.into());
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        req,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut BYOBRequestState));
        }),
    );
    std::mem::forget(weak);

    let tag = v8::Boolean::new(scope, true);
    slots::write_slot(scope, req, TAG_SLOT, tag.into());
    slots::write_slot(scope, req, CONTROLLER_OBJ_SLOT, controller.into());
    let view_v: v8::Local<v8::Value> = view.into();
    slots::write_slot(scope, req, VIEW, view_v);

    req
}

/// `InvalidateBYOBRequest(request)` — clear `[[controller]]` and `[[view]]`.
pub fn invalidate(scope: &mut v8::PinScope, request: v8::Local<v8::Object>) {
    slots::delete_slot(scope, request, CONTROLLER_OBJ_SLOT);
    slots::delete_slot(scope, request, VIEW);
}

// ---------------------------------------------------------------------------
// IDL methods
// ---------------------------------------------------------------------------

fn view_getter_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    if !is_byob_request(scope, this) {
        let msg = v8::String::new(scope, "view: receiver invalid").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    rv.set(slots::read_slot(scope, this, VIEW));
}

fn respond_method_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    _rv: v8::ReturnValue<'s>,
) {
    let this = args.this();
    if !is_byob_request(scope, this) {
        let msg = v8::String::new(scope, "respond: receiver invalid").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    let controller_v = slots::read_slot(scope, this, CONTROLLER_OBJ_SLOT);
    let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) else {
        let msg = v8::String::new(scope, "respond: request was invalidated").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    };
    // Detached check on the request's view buffer (D-16).
    let view_v = slots::read_slot(scope, this, VIEW);
    if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(view_v) {
        if let Some(buffer) = view.buffer(scope) {
            if buffer.was_detached() {
                let msg = v8::String::new(scope, "respond: request's view buffer is detached").unwrap();
                let exc = v8::Exception::type_error(scope, msg);
                scope.throw_exception(exc);
                return;
            }
        }
    }
    // Convert bytesWritten — [EnforceRange] unsigned long long.
    let bw_v = args.get(0);
    let bytes_written = match parse_enforce_range_u64(scope, bw_v) {
        Ok(n) => n,
        Err(exc) => {
            scope.throw_exception(exc);
            return;
        }
    };
    if let Err(exc_g) =
        crate::streams::readable_byte_controller::readable_byte_stream_controller_respond(
            scope,
            controller,
            bytes_written,
        )
    {
        let exc = v8::Local::new(scope, &exc_g);
        scope.throw_exception(exc);
    }
}

fn respond_with_new_view_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    _rv: v8::ReturnValue<'s>,
) {
    let this = args.this();
    if !is_byob_request(scope, this) {
        let msg = v8::String::new(scope, "respondWithNewView: receiver invalid").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    let controller_v = slots::read_slot(scope, this, CONTROLLER_OBJ_SLOT);
    let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) else {
        let msg = v8::String::new(scope, "respondWithNewView: request was invalidated").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    };
    let view_v = args.get(0);
    let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(view_v) else {
        let msg = v8::String::new(scope, "respondWithNewView: argument must be an ArrayBufferView").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    };
    // D-16: detached check.
    if let Some(buf) = view.buffer(scope) {
        if buf.was_detached() {
            let msg = v8::String::new(scope, "respondWithNewView: view's buffer is detached").unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    }
    if let Err(exc_g) = crate::streams::readable_byte_controller::readable_byte_stream_controller_respond_with_new_view(
        scope,
        controller,
        view,
    ) {
        let exc = v8::Local::new(scope, &exc_g);
        scope.throw_exception(exc);
    }
}

fn parse_enforce_range_u64<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    v: v8::Local<v8::Value>,
) -> Result<u64, v8::Local<'s, v8::Value>> {
    let n = v.number_value(scope).ok_or_else(|| {
        let msg = v8::String::new(scope, "bytesWritten must be a number").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        exc
    })?;
    if !n.is_finite() {
        let msg = v8::String::new(scope, "bytesWritten is not finite").unwrap();
        let exc = v8::Exception::range_error(scope, msg);
        return Err(exc);
    }
    if n < 0.0 || n > (1u64 << 53) as f64 {
        let msg = v8::String::new(scope, "bytesWritten out of [EnforceRange]").unwrap();
        let exc = v8::Exception::range_error(scope, msg);
        return Err(exc);
    }
    Ok(n as u64)
}

// ---------------------------------------------------------------------------
// Public install
// ---------------------------------------------------------------------------

pub fn install(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
    let tmpl = request_class_template(scope);
    let class_fn = tmpl.get_function(scope).unwrap();
    let key = v8::String::new(scope, "ReadableStreamBYOBRequest").unwrap();
    global.set(scope, key.into(), class_fn.into());
}
