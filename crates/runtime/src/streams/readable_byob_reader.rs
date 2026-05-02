//! `ReadableStreamBYOBReader` — spec §3.5.
//!
//! IDL (§3.5):
//! ```webidl
//! [Exposed=*]
//! interface ReadableStreamBYOBReader {
//!   constructor(ReadableStream stream);
//!   Promise<ReadableStreamReadResult> read(ArrayBufferView view,
//!                                           optional ReadableStreamBYOBReaderReadOptions options = {});
//!   undefined releaseLock();
//! };
//! ReadableStreamBYOBReader includes ReadableStreamGenericReader;
//!
//! dictionary ReadableStreamBYOBReaderReadOptions {
//!   [EnforceRange] unsigned long long min = 1;
//! };
//! ```
//!
//! Storage (§XV per-class):
//! - `[[stream]]`              → V8 priv sym `[[stream]]`
//! - `[[closedPromise]]` + closedResolver → paired storage in BYOBReaderState
//! - `[[readIntoRequests]]`    → Rust VecDeque on BYOBReaderState

use std::cell::RefCell;
use std::collections::VecDeque;

use crate::streams::algorithms;
use crate::streams::pull_into::ViewConstructor;
use crate::streams::readable::{is_readable_stream, StreamState};
use crate::streams::slots::{self, CLOSED_PROMISE, READER, STORED_ERROR, STREAM};

// ---------------------------------------------------------------------------
// Reader state — Box<BYOBReaderState> in internal field 0
// ---------------------------------------------------------------------------

#[allow(missing_debug_implementations)]
pub struct BYOBReaderState {
    pub read_into_requests: RefCell<VecDeque<ReadIntoRequest>>,
    pub closed_resolver: RefCell<Option<v8::Global<v8::PromiseResolver>>>,
}

impl BYOBReaderState {
    fn new(closed_resolver: v8::Global<v8::PromiseResolver>) -> Self {
        Self {
            read_into_requests: RefCell::new(VecDeque::new()),
            closed_resolver: RefCell::new(Some(closed_resolver)),
        }
    }
}

/// Read-into request — spec §3.5.4. The three step variants
/// (`chunkSteps`/`closeSteps`/`errorSteps`) are encoded by the dispatch
/// site (this module) which calls one of three methods on the request.
#[allow(missing_debug_implementations)]
pub struct ReadIntoRequest {
    pub kind: ReadIntoRequestKind,
}

pub enum ReadIntoRequestKind {
    /// User-facing `reader.read(view)` — resolve a promise resolver with
    /// `{value, done}`.
    Js {
        resolver: v8::Global<v8::PromiseResolver>,
    },
    /// Internal Rust callback — analogous to ReadRequestKind::Native.
    /// Currently unused but reserved for byte-tee and future internal
    /// users (compression-stream, fetch body bridge).
    #[allow(dead_code)]
    Native(Box<dyn ReadIntoRequestNative + 'static>),
}

pub trait ReadIntoRequestNative: 'static {
    fn chunk_steps<'s>(
        self: Box<Self>,
        scope: &mut v8::PinScope<'s, '_>,
        chunk: v8::Local<'s, v8::Value>,
    );
    fn close_steps<'s>(
        self: Box<Self>,
        scope: &mut v8::PinScope<'s, '_>,
        chunk: v8::Local<'s, v8::Value>,
    );
    fn error_steps<'s>(
        self: Box<Self>,
        scope: &mut v8::PinScope<'s, '_>,
        reason: v8::Local<'s, v8::Value>,
    );
}

// ---------------------------------------------------------------------------
// V8 wrapper helpers
// ---------------------------------------------------------------------------

/// Discriminator for `is_byob_reader` — set as a priv-sym tag at
/// construction so we can distinguish from default readers (whose
/// internal field 0 is also non-null).
const BYOB_READER_TAG_SLOT: &str = "[[byobReader.tag]]";

pub fn is_byob_reader(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>) -> bool {
    if obj
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
        .map(|e| e.value().is_null())
        .unwrap_or(true)
    {
        return false;
    }
    !slots::slot_is_empty(scope, obj, BYOB_READER_TAG_SLOT)
}

pub fn with_state<R>(
    scope: &mut v8::PinScope,
    reader: v8::Local<v8::Object>,
    f: impl FnOnce(&BYOBReaderState) -> R,
) -> Option<R> {
    let raw = reader.get_internal_field(scope, 0)?;
    let ext = v8::Local::<v8::External>::try_from(raw).ok()?;
    let ptr = ext.value() as *const BYOBReaderState;
    if ptr.is_null() {
        return None;
    }
    // SAFETY: External points at a Box<BYOBReaderState> set during
    // construction; dropped only by the V8 weak finalizer.
    let inst = unsafe { &*ptr };
    Some(f(inst))
}

// ---------------------------------------------------------------------------
// Cross-class operations on the stream side (called by byte controller)
// ---------------------------------------------------------------------------

/// `ReadableStreamHasBYOBReader(stream)` — §3.9.1.14. Real
/// implementation that replaces the always-false stub. Used by the byte
/// controller's `ShouldCallPull`.
pub fn readable_stream_has_byob_reader(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) -> bool {
    let reader_v = slots::read_slot(scope, stream, READER);
    let Ok(reader) = v8::Local::<v8::Object>::try_from(reader_v) else {
        return false;
    };
    is_byob_reader(scope, reader)
}

/// Variant for debug_assert sites that take a controller obj. The
/// `scope.x.y` access is OK in this dispatch since the controller's
/// streamObj is a separate priv-sym; this helper exists so the byte
/// controller's debug_asserts compile cleanly.
pub fn readable_stream_has_byob_reader_check(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) -> bool {
    readable_stream_has_byob_reader(scope, stream)
}

/// `ReadableStreamGetNumReadIntoRequests(stream)` — §3.9.1.x.
pub fn readable_stream_get_num_read_into_requests(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) -> usize {
    let reader_v = slots::read_slot(scope, stream, READER);
    let Ok(reader) = v8::Local::<v8::Object>::try_from(reader_v) else {
        return 0;
    };
    if !is_byob_reader(scope, reader) {
        return 0;
    }
    with_state(scope, reader, |s| s.read_into_requests.borrow().len()).unwrap_or(0)
}

/// `ReadableStreamFulfillReadIntoRequest(stream, chunk, done)` — §3.9.1.x.
pub fn readable_stream_fulfill_read_into_request<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
    chunk: v8::Local<'s, v8::Value>,
    done: bool,
) {
    let reader_v = slots::read_slot(scope, stream, READER);
    let Ok(reader) = v8::Local::<v8::Object>::try_from(reader_v) else {
        return;
    };
    let req = with_state(scope, reader, |s| s.read_into_requests.borrow_mut().pop_front())
        .flatten();
    let Some(req) = req else { return };
    if done {
        // Spec closeSteps: pass `chunk` (the partial-fill view) to
        // closeSteps; resolve {value: chunk, done: true}.
        fulfill_read_into_close(scope, req, chunk);
    } else {
        fulfill_read_into_chunk(scope, req, chunk);
    }
}

/// Add a read-into request to the BYOB reader's queue.
pub fn add_read_into_request(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
    request: ReadIntoRequest,
) {
    let reader_v = slots::read_slot(scope, stream, READER);
    let Ok(reader) = v8::Local::<v8::Object>::try_from(reader_v) else {
        return;
    };
    with_state(scope, reader, |s| {
        s.read_into_requests.borrow_mut().push_back(request);
    });
}

/// Resolve a single read-into request as `{value: view, done: true}`.
/// Used when a BYOB read happens on a closed stream.
pub fn resolve_read_into_request_done<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    request: ReadIntoRequest,
    chunk: v8::Local<'s, v8::Value>,
) {
    fulfill_read_into_close(scope, request, chunk);
}

/// Resolve a single read-into request as `{value: view, done: false}`.
/// Used by `pull_into`'s queue-fast-path: the bytes came from the
/// queue, so the read is non-final regardless of subsequent close.
pub fn resolve_read_into_request_chunk<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    request: ReadIntoRequest,
    chunk: v8::Local<'s, v8::Value>,
) {
    fulfill_read_into_chunk(scope, request, chunk);
}

/// Reject a single pending read-into request with the given error.
pub fn error_read_into_request<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    request: ReadIntoRequest,
    error: v8::Local<'s, v8::Value>,
) {
    fulfill_read_into_error(scope, request, error);
}

// ---------------------------------------------------------------------------
// Class template
// ---------------------------------------------------------------------------

fn reader_class_template<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::FunctionTemplate> {
    let ctor_tmpl = v8::FunctionTemplate::new(scope, constructor_callback);
    let class_name = v8::String::new(scope, "ReadableStreamBYOBReader").unwrap();
    ctor_tmpl.set_class_name(class_name);
    ctor_tmpl
        .instance_template(scope)
        .set_internal_field_count(1);

    let proto = ctor_tmpl.prototype_template(scope);

    {
        let key = v8::String::new(scope, "closed").unwrap();
        let getter_tmpl = v8::FunctionTemplate::new(scope, closed_getter_callback);
        proto.set_accessor_property(
            key.into(),
            Some(getter_tmpl.into()),
            None,
            v8::PropertyAttribute::NONE,
        );
    }
    install_proto_method(scope, proto, "read", read_method_callback);
    install_proto_method(scope, proto, "releaseLock", release_lock_method_callback);
    install_proto_method(scope, proto, "cancel", cancel_method_callback);

    let tag_sym = v8::Symbol::get_to_string_tag(scope);
    let tag_value = v8::String::new(scope, "ReadableStreamBYOBReader").unwrap();
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

// ---------------------------------------------------------------------------
// Constructor — `new ReadableStreamBYOBReader(stream)`
// ---------------------------------------------------------------------------

fn constructor_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    if !args.is_construct_call() {
        let msg =
            v8::String::new(scope, "ReadableStreamBYOBReader: must be called with 'new'")
                .unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    let reader_obj = args.this();
    let stream_arg = args.get(0);
    let Ok(stream) = v8::Local::<v8::Object>::try_from(stream_arg) else {
        let msg = v8::String::new(
            scope,
            "ReadableStreamBYOBReader: argument must be a ReadableStream",
        )
        .unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    };
    if !is_readable_stream(scope, stream) {
        let msg = v8::String::new(
            scope,
            "ReadableStreamBYOBReader: argument must be a ReadableStream",
        )
        .unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    // Reject byte-only stream check: BYOBReader is only valid on byte
    // streams (their controller is a ReadableByteStreamController).
    let controller_v = slots::read_slot(scope, stream, slots::CONTROLLER);
    let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) else {
        let msg = v8::String::new(scope, "ReadableStreamBYOBReader: stream has no controller").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    };
    if !crate::streams::readable_byte_controller::is_byte_controller(scope, controller) {
        let msg = v8::String::new(
            scope,
            "ReadableStreamBYOBReader: cannot construct on a non-byte-stream",
        )
        .unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    if algorithms::is_readable_stream_locked(scope, stream) {
        let msg =
            v8::String::new(scope, "ReadableStreamBYOBReader: stream is already locked")
                .unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    set_up_byob_reader(scope, reader_obj, stream);
}

/// Public constructor used by `getReader({mode: "byob"})`.
pub fn acquire_readable_stream_byob_reader<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
) -> Result<v8::Local<'s, v8::Object>, String> {
    if algorithms::is_readable_stream_locked(scope, stream) {
        return Err("ReadableStream.getReader: stream is already locked".to_string());
    }
    let controller_v = slots::read_slot(scope, stream, slots::CONTROLLER);
    let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) else {
        return Err("getReader(byob): stream has no controller".to_string());
    };
    if !crate::streams::readable_byte_controller::is_byte_controller(scope, controller) {
        return Err(
            "getReader(byob): can only be called on a ReadableStream with type=\"bytes\""
                .to_string(),
        );
    }
    // Use the GLOBAL ReadableStreamBYOBReader class so `instanceof`
    // checks work. Fall back to a local template if global isn't set
    // (e.g., in tests that haven't installed the streams namespace).
    let global = scope.get_current_context().global(scope);
    let class_name = v8::String::new(scope, "ReadableStreamBYOBReader").unwrap();
    let class_v = global.get(scope, class_name.into()).unwrap_or_else(|| v8::undefined(scope).into());
    let class_fn = if let Ok(f) = v8::Local::<v8::Function>::try_from(class_v) {
        f
    } else {
        let tmpl = reader_class_template(scope);
        tmpl.get_function(scope).unwrap()
    };
    // Create instance via class_fn's instance template (we need internal
    // fields). Get the FunctionTemplate by calling the matching helper.
    let tmpl = reader_class_template(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let reader_obj = inst_tmpl
        .new_instance(scope)
        .ok_or_else(|| "alloc BYOB reader instance".to_string())?;
    // Set prototype to the global class's prototype so instanceof works.
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    reader_obj.set_prototype(scope, proto_v);

    set_up_byob_reader(scope, reader_obj, stream);
    Ok(reader_obj)
}

fn set_up_byob_reader(
    scope: &mut v8::PinScope,
    reader: v8::Local<v8::Object>,
    stream: v8::Local<v8::Object>,
) {
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let closed_promise = resolver.get_promise(scope);
    let resolver_g = v8::Global::new(scope, resolver);

    let state = BYOBReaderState::new(resolver_g);
    let boxed = Box::new(state);
    let raw_ptr = Box::into_raw(boxed);
    let raw_addr = raw_ptr as usize;
    let ext = v8::External::new(scope, raw_ptr as *mut std::ffi::c_void);
    reader.set_internal_field(0, ext.into());
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        reader,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut BYOBReaderState));
        }),
    );
    std::mem::forget(weak);

    // BYOB reader tag (used by is_byob_reader).
    let tag = v8::Boolean::new(scope, true);
    slots::write_slot(scope, reader, BYOB_READER_TAG_SLOT, tag.into());

    // ReaderGenericInitialize.
    slots::write_slot(scope, reader, STREAM, stream.into());
    slots::write_slot(scope, stream, READER, reader.into());
    slots::write_slot(scope, reader, CLOSED_PROMISE, closed_promise.into());

    let st = match crate::streams::readable::with_rs_state(scope, stream, |s| s.state.get()) {
        Some(s) => s,
        None => return,
    };
    match st {
        StreamState::Readable => {}
        StreamState::Closed => {
            resolve_closed_promise(scope, reader);
        }
        StreamState::Errored => {
            let stored = slots::read_slot(scope, stream, STORED_ERROR);
            reject_closed_promise(scope, reader, stored);
        }
    }
}

// ---------------------------------------------------------------------------
// Closed-promise helpers (mirror default-reader)
// ---------------------------------------------------------------------------

pub fn resolve_closed_promise(scope: &mut v8::PinScope, reader: v8::Local<v8::Object>) {
    let resolver_g =
        with_state(scope, reader, |s| s.closed_resolver.borrow_mut().take()).flatten();
    if let Some(resolver_g) = resolver_g {
        let resolver = v8::Local::new(scope, &resolver_g);
        let undef = v8::undefined(scope);
        resolver.resolve(scope, undef.into());
    }
}

pub fn reject_closed_promise<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    reader: v8::Local<v8::Object>,
    error: v8::Local<'s, v8::Value>,
) {
    let resolver_g =
        with_state(scope, reader, |s| s.closed_resolver.borrow_mut().take()).flatten();
    if let Some(resolver_g) = resolver_g {
        let resolver = v8::Local::new(scope, &resolver_g);
        resolver.reject(scope, error);
        let cp_v = slots::read_slot(scope, reader, CLOSED_PROMISE);
        if let Ok(cp) = v8::Local::<v8::Promise>::try_from(cp_v) {
            crate::streams::promise_resolve::set_promise_is_handled_to_true(scope, cp);
        }
    }
}

// ---------------------------------------------------------------------------
// IDL methods
// ---------------------------------------------------------------------------

fn closed_getter_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    if !is_byob_reader(scope, this) {
        let msg = v8::String::new(scope, "closed: receiver invalid").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    rv.set(slots::read_slot(scope, this, CLOSED_PROMISE));
}

fn read_method_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'s>,
) {
    let this = args.this();
    if !is_byob_reader(scope, this) {
        let msg = v8::String::new(scope, "read: receiver invalid").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    let view_v = args.get(0);
    let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(view_v) else {
        let msg = v8::String::new(scope, "read: argument must be an ArrayBufferView").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    };
    if view.byte_length() == 0 {
        let msg = v8::String::new(scope, "read: view byteLength is 0").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    // D-16: detached check on view.buffer.
    let buffer = match view.buffer(scope) {
        Some(b) => b,
        None => {
            let msg = v8::String::new(scope, "read: view has no buffer").unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            let resolver = v8::PromiseResolver::new(scope).unwrap();
            let p = resolver.get_promise(scope);
            resolver.reject(scope, exc);
            rv.set(p.into());
            return;
        }
    };
    if buffer.was_detached() {
        let msg = v8::String::new(scope, "read: view's buffer is detached").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    if buffer.byte_length() == 0 {
        let msg = v8::String::new(scope, "read: view's buffer byteLength is 0").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    let ctor = match ViewConstructor::from_view(view) {
        Some(c) => c,
        None => {
            let msg = v8::String::new(scope, "read: unrecognized view constructor").unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            let resolver = v8::PromiseResolver::new(scope).unwrap();
            let p = resolver.get_promise(scope);
            resolver.reject(scope, exc);
            rv.set(p.into());
            return;
        }
    };
    let elem_size = ctor.element_size();
    // Parse options (min, default 1).
    let opts = args.get(1);
    let min = match parse_min(scope, opts) {
        Ok(n) => n,
        Err(exc) => {
            let resolver = v8::PromiseResolver::new(scope).unwrap();
            let p = resolver.get_promise(scope);
            resolver.reject(scope, exc);
            rv.set(p.into());
            return;
        }
    };
    if min == 0 {
        let msg = v8::String::new(scope, "read: min must be greater than 0").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    let elem_count = (view.byte_length() as u64) / elem_size;
    if min > elem_count {
        let msg = v8::String::new(scope, "read: min exceeds view length").unwrap();
        let exc = v8::Exception::range_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    // Stream must be present.
    let stream_v = slots::read_slot(scope, this, STREAM);
    let Ok(stream) = v8::Local::<v8::Object>::try_from(stream_v) else {
        let msg = v8::String::new(scope, "read: reader has no stream").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    };
    // Set [[disturbed]] = true.
    crate::streams::readable::with_rs_state(scope, stream, |s| s.disturbed.set(true));

    // Allocate result Promise.
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let p = resolver.get_promise(scope);
    let resolver_g = v8::Global::new(scope, resolver);

    // If state == "errored", reject with stored error.
    let st = crate::streams::readable::with_rs_state(scope, stream, |s| s.state.get())
        .unwrap_or(StreamState::Errored);
    if st == StreamState::Errored {
        let stored = slots::read_slot(scope, stream, STORED_ERROR);
        let resolver_l = v8::Local::new(scope, &resolver_g);
        resolver_l.reject(scope, stored);
        rv.set(p.into());
        return;
    }

    // Pull-into bytes_written = min * elementSize.
    let minimum_fill = min * elem_size;
    let request = ReadIntoRequest {
        kind: ReadIntoRequestKind::Js {
            resolver: resolver_g,
        },
    };
    let controller_v = slots::read_slot(scope, stream, slots::CONTROLLER);
    if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
        crate::streams::readable_byte_controller::readable_byte_stream_controller_pull_into(
            scope,
            controller,
            view,
            minimum_fill,
            request,
        );
    }
    rv.set(p.into());
}

fn parse_min<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    opts: v8::Local<v8::Value>,
) -> Result<u64, v8::Local<'s, v8::Value>> {
    if opts.is_undefined() || opts.is_null() {
        return Ok(1);
    }
    let Ok(obj) = v8::Local::<v8::Object>::try_from(opts) else {
        let msg = v8::String::new(scope, "read: options must be an object").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        return Err(exc);
    };
    let key = v8::String::new(scope, "min").unwrap();
    let v = obj
        .get(scope, key.into())
        .unwrap_or_else(|| v8::undefined(scope).into());
    if v.is_undefined() {
        return Ok(1);
    }
    // [EnforceRange] unsigned long long: convert to integer; if NaN/<0/
    // > 2^53-1 → throw RangeError.
    let n = v
        .number_value(scope)
        .ok_or_else(|| {
            let msg = v8::String::new(scope, "read: min must be a number").unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            exc
        })?;
    // [EnforceRange] per WebIDL: if not a finite integer in the valid
    // range, throw a TypeError. (NaN/inf, < 0, > 2^53-1 → TypeError.)
    if !n.is_finite() || n < 0.0 || n > (1u64 << 53) as f64 || n.fract() != 0.0 {
        let msg = v8::String::new(scope, "read: min out of [EnforceRange]").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        return Err(exc);
    }
    Ok(n as u64)
}

fn release_lock_method_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let this = args.this();
    if !is_byob_reader(scope, this) {
        let msg = v8::String::new(scope, "releaseLock: receiver invalid").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    readable_stream_byob_reader_release(scope, this);
}

/// `ReadableStreamBYOBReaderRelease(reader)` — §3.5.x.
pub fn readable_stream_byob_reader_release(
    scope: &mut v8::PinScope,
    reader: v8::Local<v8::Object>,
) {
    let stream_v = slots::read_slot(scope, reader, STREAM);
    if stream_v.is_undefined() {
        return;
    }
    // Tell the byte controller to mark the front descriptor's
    // readerType = None.
    if let Ok(stream) = v8::Local::<v8::Object>::try_from(stream_v) {
        crate::streams::readable_byte_controller::release_steps(scope, stream);
    }
    readable_stream_byob_reader_generic_release(scope, reader);
    let msg = v8::String::new(
        scope,
        "Reader released; outstanding read() requests rejected",
    )
    .unwrap();
    let exc = v8::Exception::type_error(scope, msg);
    let exc_l = exc.into();
    readable_stream_byob_reader_error_read_into_requests(scope, reader, exc_l);
}

/// `ReadableStreamBYOBReaderErrorReadIntoRequests(reader, e)` — §3.5.x.
pub fn readable_stream_byob_reader_error_read_into_requests<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    reader: v8::Local<v8::Object>,
    error: v8::Local<'s, v8::Value>,
) {
    let drained: Vec<ReadIntoRequest> = with_state(scope, reader, |state| {
        state.read_into_requests.borrow_mut().drain(..).collect()
    })
    .unwrap_or_default();
    for req in drained {
        fulfill_read_into_error(scope, req, error);
    }
}

/// Generic release — mirrors default reader's logic.
fn readable_stream_byob_reader_generic_release(
    scope: &mut v8::PinScope,
    reader: v8::Local<v8::Object>,
) {
    let stream_v = slots::read_slot(scope, reader, STREAM);
    let Ok(stream) = v8::Local::<v8::Object>::try_from(stream_v) else {
        return;
    };
    let st = crate::streams::readable::with_rs_state(scope, stream, |s| s.state.get());
    let msg = v8::String::new(
        scope,
        "Reader was released and can no longer be used to monitor the stream's state",
    )
    .unwrap();
    let exc = v8::Exception::type_error(scope, msg);
    match st {
        Some(StreamState::Readable) => {
            reject_closed_promise(scope, reader, exc);
        }
        _ => {
            let resolver = v8::PromiseResolver::new(scope).unwrap();
            resolver.reject(scope, exc);
            let p = resolver.get_promise(scope);
            slots::write_slot(scope, reader, CLOSED_PROMISE, p.into());
            crate::streams::promise_resolve::set_promise_is_handled_to_true(scope, p);
        }
    }
    slots::delete_slot(scope, stream, READER);
    slots::delete_slot(scope, reader, STREAM);
}

fn cancel_method_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'s>,
) {
    let this = args.this();
    if !is_byob_reader(scope, this) {
        let msg = v8::String::new(scope, "cancel: receiver invalid").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    let stream_v = slots::read_slot(scope, this, STREAM);
    let Ok(stream) = v8::Local::<v8::Object>::try_from(stream_v) else {
        let msg = v8::String::new(scope, "cancel: reader has no associated stream").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    };
    let reason = args.get(0);
    let promise = algorithms::readable_stream_cancel(scope, stream, reason);
    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Read-into request fulfillment
// ---------------------------------------------------------------------------

fn fulfill_read_into_chunk<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    request: ReadIntoRequest,
    chunk: v8::Local<'s, v8::Value>,
) {
    match request.kind {
        ReadIntoRequestKind::Js { resolver } => {
            let resolver_l = v8::Local::new(scope, &resolver);
            let result = v8::Object::new(scope);
            let value_key = v8::String::new(scope, "value").unwrap();
            let done_key = v8::String::new(scope, "done").unwrap();
            result.set(scope, value_key.into(), chunk);
            result.set(scope, done_key.into(), v8::Boolean::new(scope, false).into());
            resolver_l.resolve(scope, result.into());
        }
        ReadIntoRequestKind::Native(req) => {
            req.chunk_steps(scope, chunk);
        }
    }
}

fn fulfill_read_into_close<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    request: ReadIntoRequest,
    chunk: v8::Local<'s, v8::Value>,
) {
    match request.kind {
        ReadIntoRequestKind::Js { resolver } => {
            let resolver_l = v8::Local::new(scope, &resolver);
            let result = v8::Object::new(scope);
            let value_key = v8::String::new(scope, "value").unwrap();
            let done_key = v8::String::new(scope, "done").unwrap();
            result.set(scope, value_key.into(), chunk);
            result.set(scope, done_key.into(), v8::Boolean::new(scope, true).into());
            resolver_l.resolve(scope, result.into());
        }
        ReadIntoRequestKind::Native(req) => {
            req.close_steps(scope, chunk);
        }
    }
}

fn fulfill_read_into_error<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    request: ReadIntoRequest,
    error: v8::Local<'s, v8::Value>,
) {
    match request.kind {
        ReadIntoRequestKind::Js { resolver } => {
            let resolver_l = v8::Local::new(scope, &resolver);
            resolver_l.reject(scope, error);
        }
        ReadIntoRequestKind::Native(req) => {
            req.error_steps(scope, error);
        }
    }
}

// ---------------------------------------------------------------------------
// Public install
// ---------------------------------------------------------------------------

pub fn install(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
    let tmpl = reader_class_template(scope);
    let class_fn = tmpl.get_function(scope).unwrap();
    let key = v8::String::new(scope, "ReadableStreamBYOBReader").unwrap();
    global.set(scope, key.into(), class_fn.into());
}
