//! `WritableStream` — spec §4.2.
//!
//! Hand-rolled rather than macro-driven for the same reason as
//! `readable.rs`: every method needs `args.this()` access and our spec
//! algorithms key off the wrapper's V8 identity (private symbols
//! `[[writer]]`, `[[storedError]]`, `[[controller]]`).
//!
//! IDL surface (§4.2):
//! ```webidl
//! [Exposed=*]
//! interface WritableStream {
//!   constructor(optional object underlyingSink, optional QueuingStrategy strategy = {});
//!   readonly attribute boolean locked;
//!   Promise<undefined> abort(optional any reason);
//!   Promise<undefined> close();
//!   WritableStreamDefaultWriter getWriter();
//! };
//! ```
//!
//! Storage (D-2 audit, design §XV.4):
//! - `[[state]]`                          → Rust Cell<WSState>            (SLOT)
//! - `[[backpressure]]`                   → Rust Cell<bool>               (SLOT)
//! - `[[closeRequest]]`                   → Rust paired (Promise+Resolver)(SLOT)
//! - `[[inFlightWriteRequest]]`           → Rust RefCell<Option<…>>       (SLOT)
//! - `[[inFlightCloseRequest]]`           → Rust RefCell<Option<…>>       (SLOT)
//! - `[[pendingAbortRequest]]`            → Rust RefCell<Option<…>>       (SLOT)
//! - `[[writeRequests]]` (Promises)       → Rust VecDeque<Global<Promise>>(SLOT)
//!   + parallel write-request resolvers
//! - `[[storedError]]`                    → V8 priv sym `[[storedError]]` (SLOT)
//! - `[[writer]]`                         → V8 priv sym `[[writer]]`      (SLOT)
//! - `[[controller]]`                     → V8 priv sym `[[controller]]`  (SLOT)

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;

use crate::streams::budget::{try_alloc_stream, StreamBudgetGuard};
use crate::streams::writable_controller as ctlr;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// `[[state]]` — spec §4.2.5. The four observable states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WSState {
    /// "writable"
    Writable,
    /// "closed"
    Closed,
    /// "erroring" — transient state while errors propagate
    Erroring,
    /// "errored"
    Errored,
}

/// `Box<WSStreamState>` is stored in the wrapper's V8 internal field 0.
/// Per D-2 / §XV.4: this struct holds ONLY pure-Rust slots. The
/// `[[storedError]]` / `[[writer]]` / `[[controller]]` slots live in
/// V8 private symbols.
#[allow(missing_debug_implementations)]
pub struct WSStreamState {
    /// SLOT: [[state]]
    pub state: Cell<WSState>,
    /// SLOT: [[backpressure]]
    pub backpressure: Cell<bool>,
    /// SLOT: [[closeRequest]] — Promise+Resolver pair (None when no close pending).
    pub close_request: RefCell<Option<PromisePair>>,
    /// SLOT: [[inFlightWriteRequest]] — the resolver of the currently-writing
    /// chunk's promise. The corresponding promise was already returned to JS
    /// by writer.write() and is held in `write_requests` until the spec moves
    /// it to in-flight via WritableStreamMarkFirstWriteRequestInFlight.
    pub in_flight_write_request: RefCell<Option<v8::Global<v8::PromiseResolver>>>,
    /// SLOT: [[inFlightCloseRequest]] — the resolver of the in-flight close.
    pub in_flight_close_request: RefCell<Option<v8::Global<v8::PromiseResolver>>>,
    /// SLOT: [[pendingAbortRequest]]
    pub pending_abort_request: RefCell<Option<PendingAbortRequest>>,
    /// SLOT: [[writeRequests]] — list of Promises (returned to JS). Per
    /// CRITICAL #13 / design §II.8: spec says this is a list of Promises,
    /// NOT Resolvers. We store both: the promises here (used by
    /// FinishErroring to iterate and reject), and the matching resolvers
    /// in `write_request_resolvers` so we can resolve/reject them on
    /// completion.
    pub write_requests: RefCell<VecDeque<v8::Global<v8::Promise>>>,
    pub write_request_resolvers: RefCell<VecDeque<v8::Global<v8::PromiseResolver>>>,
    /// D-18 budget guard.
    _budget: StreamBudgetGuard,
}

/// A Promise+Resolver pair. The Promise was returned to JS; the Resolver
/// is held by Rust to resolve/reject when the operation finishes.
#[allow(missing_debug_implementations)]
pub struct PromisePair {
    pub promise: v8::Global<v8::Promise>,
    pub resolver: v8::Global<v8::PromiseResolver>,
}

/// `[[pendingAbortRequest]]` per spec §4.5: { promise, reason, wasAlreadyErroring }.
#[allow(missing_debug_implementations)]
pub struct PendingAbortRequest {
    pub resolver: v8::Global<v8::PromiseResolver>,
    pub reason: v8::Global<v8::Value>,
    pub was_already_erroring: bool,
}

impl WSStreamState {
    pub fn new(budget: StreamBudgetGuard) -> Self {
        Self {
            state: Cell::new(WSState::Writable),
            backpressure: Cell::new(false),
            close_request: RefCell::new(None),
            in_flight_write_request: RefCell::new(None),
            in_flight_close_request: RefCell::new(None),
            pending_abort_request: RefCell::new(None),
            write_requests: RefCell::new(VecDeque::new()),
            write_request_resolvers: RefCell::new(VecDeque::new()),
            _budget: budget,
        }
    }
}

// ---------------------------------------------------------------------------
// V8 wrapper helpers
// ---------------------------------------------------------------------------

/// Confirm `obj` is a WritableStream wrapper (its internal field 0 is
/// an External pointing at a `WSStreamState`). The discriminator is
/// the presence of the WS-specific class brand on the prototype, but
/// for a single-class context we use the External-non-null sentinel.
pub fn is_writable_stream(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
) -> bool {
    // Tag via private symbol — set during construction. Prevents confusion
    // with other classes that also have an External in field 0.
    let tag = crate::streams::slots::private_sym(scope, "[[ws.brand]]");
    obj.has_private(scope, tag).unwrap_or(false)
}

/// Reach into a JS WritableStream wrapper's WSStreamState. Returns None if
/// the object is not a WritableStream.
pub fn with_ws_state<R>(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
    f: impl FnOnce(&WSStreamState) -> R,
) -> Option<R> {
    if !is_writable_stream(scope, stream) {
        return None;
    }
    let raw_v8_field = stream.get_internal_field(scope, 0)?;
    let ext = v8::Local::<v8::External>::try_from(raw_v8_field).ok()?;
    let ptr = ext.value() as *const WSStreamState;
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the External was set during construction to a Box<WSStreamState>
    // (see `build_stream_wrapper` and `constructor_callback`). The Box is
    // dropped only by the V8 weak finalizer, which fires after all JS
    // callbacks complete (single-threaded per isolate).
    let inst = unsafe { &*ptr };
    Some(f(inst))
}

// ---------------------------------------------------------------------------
// from_native_sink — Rust-only constructor (D-9, §I.1)
// ---------------------------------------------------------------------------

/// Build a JS WritableStream from a Rust sink.
///
/// Mirror of `ReadableStream::from_native_source`. Per design §VIII.2.
#[doc(hidden)]
pub fn from_native_sink<'s, S: NativeSink + 'static>(
    scope: &mut v8::PinScope<'s, '_>,
    sink: S,
    hwm: f64,
) -> v8::Local<'s, v8::Object> {
    let stream = build_stream_wrapper(scope);
    ctlr::set_up_writable_stream_default_controller_native(scope, stream, sink, hwm);
    stream
}

/// Public wrapper around `build_stream_wrapper` for internal use by
/// `transform.rs` (TransformStream's writable half is a programmatically-
/// constructed WritableStream).
#[doc(hidden)]
pub fn build_value_stream_wrapper_for_internal<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::Object> {
    build_stream_wrapper(scope)
}

/// Construct the bare `WritableStream` JS wrapper — no controller wired.
fn build_stream_wrapper<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::Object> {
    let tmpl = stream_class_template(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let stream_obj = inst_tmpl.new_instance(scope).unwrap();

    let budget = try_alloc_stream().expect("from_native_sink: budget exceeded");
    let inst = WSStreamState::new(budget);
    let boxed = Box::new(inst);
    let raw_ptr = Box::into_raw(boxed);
    let raw_addr = raw_ptr as usize;
    let ext = v8::External::new(scope, raw_ptr as *mut std::ffi::c_void);
    stream_obj.set_internal_field(0, ext.into());

    // Tag with brand priv-sym so receiver checks can distinguish from
    // other classes that also embed an External in field 0.
    let tag = crate::streams::slots::private_sym(scope, "[[ws.brand]]");
    let true_v: v8::Local<v8::Value> = v8::Boolean::new(scope, true).into();
    stream_obj.set_private(scope, tag, true_v);

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        stream_obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut WSStreamState));
        }),
    );
    std::mem::forget(weak);

    // Prefer `globalThis.WritableStream.prototype` so internally-built
    // wrappers share JS class identity. Fall back to the just-built
    // template when install_native_writable_stream hasn't run yet.
    let proto_v = global_class_prototype_ws(scope, "WritableStream").unwrap_or_else(|| {
        let class_fn = tmpl.get_function(scope).unwrap();
        let proto_key = v8::String::new(scope, "prototype").unwrap();
        class_fn.get(scope, proto_key.into()).unwrap()
    });
    stream_obj.set_prototype(scope, proto_v);

    stream_obj
}

fn global_class_prototype_ws<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    name: &str,
) -> Option<v8::Local<'s, v8::Value>> {
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, name)?;
    let class_v = global.get(scope, key.into())?;
    let class_obj = v8::Local::<v8::Object>::try_from(class_v).ok()?;
    let proto_key = v8::String::new(scope, "prototype")?;
    class_obj.get(scope, proto_key.into())
}

// ---------------------------------------------------------------------------
// Class template construction
// ---------------------------------------------------------------------------

fn stream_class_template<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::FunctionTemplate> {
    let ctor_tmpl = v8::FunctionTemplate::new(scope, constructor_callback);
    let class_name = v8::String::new(scope, "WritableStream").unwrap();
    ctor_tmpl.set_class_name(class_name);
    ctor_tmpl
        .instance_template(scope)
        .set_internal_field_count(1);

    let proto = ctor_tmpl.prototype_template(scope);

    // locked getter (§4.2.5.1)
    {
        let key = v8::String::new(scope, "locked").unwrap();
        let getter_tmpl = v8::FunctionTemplate::new(scope, locked_getter_callback);
        proto.set_accessor_property(
            key.into(),
            Some(getter_tmpl.into()),
            None,
            v8::PropertyAttribute::NONE,
        );
    }

    install_method(scope, proto, "abort", abort_method_callback);
    install_method(scope, proto, "close", close_method_callback);
    install_method(scope, proto, "getWriter", get_writer_method_callback);

    let tag_sym = v8::Symbol::get_to_string_tag(scope);
    let tag_value = v8::String::new(scope, "WritableStream").unwrap();
    proto.set_with_attr(
        tag_sym.into(),
        tag_value.into(),
        v8::PropertyAttribute::READ_ONLY | v8::PropertyAttribute::DONT_ENUM,
    );

    ctor_tmpl
}

fn install_method(
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
// constructor — `new WritableStream(underlyingSink?, strategy?)`
// ---------------------------------------------------------------------------

fn constructor_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    if !args.is_construct_call() {
        let msg = v8::String::new(scope, "WritableStream: must be called with 'new'").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }

    let stream_obj = args.this();
    let underlying_sink = args.get(0);
    let strategy = args.get(1);

    // Per spec §4.2.4 step 1: underlyingSink defaults to null when undefined.
    // Then step 2: convert via UnderlyingSink dictionary; step 3: 'type' in
    // dict → RangeError.
    //
    // Per WebIDL — strategy is converted FIRST (its size getter fires
    // before underlyingSink's accessors), then underlyingSink. Match the
    // ReadableStream constructor's ordering for WPT consistency.
    let (hwm, size_algo) = match crate::streams::readable::parse_strategy_local(scope, strategy, 1.0) {
        Ok(v) => v,
        Err(e) => {
            crate::streams::readable::throw_op_error(scope, &e);
            return;
        }
    };

    // 'type' rejection on underlyingSink. Spec: per §4.2.4 step 3, if
    // underlyingSinkDict has a "type" entry the constructor throws
    // RangeError (NOT TypeError — spec says "Invalid type is specified").
    if let Ok(us) = v8::Local::<v8::Object>::try_from(underlying_sink) {
        let type_key = v8::String::new(scope, "type").unwrap();
        let type_v = match us.get(scope, type_key.into()) {
            Some(v) => v,
            None => return,
        };
        if !type_v.is_undefined() {
            let msg = v8::String::new(scope, "WritableStream: invalid type is specified").unwrap();
            let exc = v8::Exception::range_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    }

    // Allocate budget + Box<WSStreamState>.
    let budget = match try_alloc_stream() {
        Ok(g) => g,
        Err(m) => {
            let msg = v8::String::new(scope, m).unwrap();
            let exc = v8::Exception::range_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };
    let inst = WSStreamState::new(budget);
    let boxed = Box::new(inst);
    let raw_ptr = Box::into_raw(boxed);
    let raw_addr = raw_ptr as usize;
    let ext = v8::External::new(scope, raw_ptr as *mut std::ffi::c_void);
    stream_obj.set_internal_field(0, ext.into());

    // Tag for receiver-check.
    let tag = crate::streams::slots::private_sym(scope, "[[ws.brand]]");
    let true_v: v8::Local<v8::Value> = v8::Boolean::new(scope, true).into();
    stream_obj.set_private(scope, tag, true_v);

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        stream_obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut WSStreamState));
        }),
    );
    std::mem::forget(weak);

    if let Err(msg) = ctlr::set_up_writable_stream_default_controller_from_underlying_sink_with_strategy(
        scope,
        stream_obj,
        underlying_sink,
        hwm,
        size_algo,
    ) {
        let v8_msg = v8::String::new(scope, &msg).unwrap();
        let exc = v8::Exception::type_error(scope, v8_msg);
        scope.throw_exception(exc);
    }
}

// ---------------------------------------------------------------------------
// `locked` getter (§4.2.5.1)
// ---------------------------------------------------------------------------

fn locked_getter_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    if !is_writable_stream(scope, this) {
        let msg = v8::String::new(scope, "WritableStream.locked: receiver is not a WritableStream").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    let locked = crate::streams::algorithms::is_writable_stream_locked(scope, this);
    rv.set(v8::Boolean::new(scope, locked).into());
}

// ---------------------------------------------------------------------------
// `abort(reason)` (§4.2.5.4)
// ---------------------------------------------------------------------------

fn abort_method_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'s>,
) {
    let this = args.this();
    if !is_writable_stream(scope, this) {
        let msg = v8::String::new(scope, "WritableStream.abort: receiver is not a WritableStream").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    if crate::streams::algorithms::is_writable_stream_locked(scope, this) {
        let msg = v8::String::new(
            scope,
            "WritableStream.abort: cannot abort a stream that already has a writer",
        )
        .unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    let reason = args.get(0);
    let promise = crate::streams::algorithms::writable_stream_abort(scope, this, reason);
    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// `close()` (§4.2.5.5)
// ---------------------------------------------------------------------------

fn close_method_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'s>,
) {
    let this = args.this();
    if !is_writable_stream(scope, this) {
        let msg = v8::String::new(scope, "WritableStream.close: receiver is not a WritableStream").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    if crate::streams::algorithms::is_writable_stream_locked(scope, this) {
        let msg = v8::String::new(
            scope,
            "WritableStream.close: cannot close a stream that already has a writer",
        )
        .unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    if crate::streams::algorithms::writable_stream_close_queued_or_in_flight(scope, this) {
        let msg = v8::String::new(scope, "WritableStream.close: cannot close an already-closing stream").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    let promise = crate::streams::algorithms::writable_stream_close(scope, this);
    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// `getWriter()` (§4.2.5.6)
// ---------------------------------------------------------------------------

fn get_writer_method_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    if !is_writable_stream(scope, this) {
        let msg = v8::String::new(scope, "WritableStream.getWriter: receiver is not a WritableStream").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    let writer = match crate::streams::writable_writer::acquire_writable_stream_default_writer(scope, this) {
        Ok(w) => w,
        Err(err) => {
            let msg = v8::String::new(scope, &err).unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };
    rv.set(writer.into());
}

// ---------------------------------------------------------------------------
// NativeSink trait — §VIII.2
// ---------------------------------------------------------------------------

/// Native UnderlyingSink — Rust trait that mirrors WebIDL's
/// `UnderlyingSink` dictionary, but typed.
///
/// Per design §VIII.2. Used by `from_native_sink` to build a JS-visible
/// WritableStream from a Rust consumer (e.g. compression encoder, fetch
/// upload buffer).
pub trait NativeSink: 'static {
    fn start(
        &mut self,
        _controller: &mut NativeWritableController,
    ) -> Result<(), v8::Global<v8::Value>> {
        Ok(())
    }

    fn write(
        &mut self,
        chunk: v8::Global<v8::Value>,
        controller: &mut NativeWritableController,
    ) -> Pin<Box<dyn Future<Output = Result<(), v8::Global<v8::Value>>> + 'static>>;

    fn close(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), v8::Global<v8::Value>>> + 'static>> {
        Box::pin(async { Ok(()) })
    }

    fn abort(
        &mut self,
        _reason: Option<v8::Global<v8::Value>>,
    ) -> Pin<Box<dyn Future<Output = Result<(), v8::Global<v8::Value>>> + 'static>> {
        Box::pin(async { Ok(()) })
    }
}

/// Thin wrapper around the controller wrapper, exposed to NativeSink
/// trait impls.
#[allow(missing_debug_implementations)]
pub struct NativeWritableController {
    pub(crate) controller_obj: v8::Global<v8::Object>,
}

impl NativeWritableController {
    /// `controller.error(scope, exc)` — drive the spec's
    /// WritableStreamDefaultControllerError from a NativeSink.
    pub fn error<'s>(
        &mut self,
        scope: &mut v8::PinScope<'s, '_>,
        reason: v8::Local<'s, v8::Value>,
    ) {
        let controller_obj = v8::Local::new(scope, &self.controller_obj);
        ctlr::writable_stream_default_controller_error_if_needed(scope, controller_obj, reason);
    }

    /// Returns the controller's AbortSignal (placeholder until
    /// AbortSignal native lands — see §II.9 / dispatch brief).
    pub fn signal<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> v8::Local<'s, v8::Value> {
        let controller_obj = v8::Local::new(scope, &self.controller_obj);
        ctlr::signal_value(scope, controller_obj)
    }
}

// ---------------------------------------------------------------------------
// Public install — wire onto `globalThis`
// ---------------------------------------------------------------------------

/// Install `globalThis.WritableStream` only — DefaultController and
/// DefaultWriter are installed by their respective modules.
pub fn install_native_writable_stream(
    scope: &mut v8::PinScope,
    global: v8::Local<v8::Object>,
) {
    let stream_tmpl = stream_class_template(scope);
    let stream_class_fn = stream_tmpl.get_function(scope).unwrap();
    let key = v8::String::new(scope, "WritableStream").unwrap();
    global.set(scope, key.into(), stream_class_fn.into());
}
