//! `WritableStreamDefaultWriter` — spec §4.4.
//!
//! IDL (§4.4.1):
//! ```webidl
//! [Exposed=*]
//! interface WritableStreamDefaultWriter {
//!   constructor(WritableStream stream);
//!   readonly attribute Promise<undefined> closed;
//!   readonly attribute unrestricted double? desiredSize;
//!   readonly attribute Promise<undefined> ready;
//!   Promise<undefined> abort(optional any reason);
//!   Promise<undefined> close();
//!   undefined releaseLock();
//!   Promise<undefined> write(optional any chunk);
//! };
//! ```
//!
//! Storage (D-2 audit):
//! - `[[stream]]`         → V8 priv sym `[[stream]]` on the writer wrapper (SLOT)
//! - `[[closedPromise]]`  → V8 priv sym `[[closedPromise]]` (SLOT for getter)
//!   + paired Resolver in Rust state (so we can resolve/reject the promise)
//! - `[[readyPromise]]`   → V8 priv sym `[[readyPromise]]` (SLOT for getter)
//!   + paired Resolver in Rust state
//!
//! Promise lifecycle (CRITICAL #44 / design §II.10):
//! `EnsureReadyPromiseRejected` and `EnsureClosedPromiseRejected` need to
//! REPLACE the promise wholesale if the existing one is settled. We track
//! "settled" by clearing the Resolver after first resolve/reject. When the
//! resolver is None and we need to reject, we allocate a fresh
//! pre-rejected promise+resolver pair and update the priv-sym slot.

use std::cell::RefCell;

use zeroship_runtime_macros::{v8_class, v8_constructor};

use crate::state::OpError;
use crate::streams::algorithms;
use crate::streams::promise_resolve;
use crate::streams::slots::{self, CLOSED_PROMISE, READY_PROMISE, STREAM, WRITER};
use crate::streams::writable::{is_writable_stream, with_ws_state, WSState};

/// Brand priv-sym for receiver checks.
const WRITER_BRAND: &str = "[[ws.writer.brand]]";

// ---------------------------------------------------------------------------
// Writer state — Box<WritableStreamDefaultWriter> in internal field 0
// ---------------------------------------------------------------------------

/// Boxed state behind the JS `WritableStreamDefaultWriter` wrapper. Lives
/// in internal field 0; reclaimed by the V8 weak finalizer registered via
/// the `#[v8_class]` macro.
///
/// MAC-02 migration: parallels the readers. Self::new validates the
/// stream argument and stashes it in `pending_stream` for the post_init
/// hook (`after_install`) to consume. The hook does box-install-dependent
/// setup: WRITER_BRAND priv-sym, [[stream]] / stream.[[writer]] wires,
/// and the four-way state-driven closedPromise / readyPromise init.
#[allow(missing_debug_implementations)]
pub struct WritableStreamDefaultWriter {
    /// Resolver paired with the priv-sym `[[closedPromise]]`. Becomes None
    /// after first resolve/reject — subsequent EnsureClosedPromiseRejected
    /// allocates a fresh pre-rejected promise.
    pub closed_resolver: RefCell<Option<v8::Global<v8::PromiseResolver>>>,
    /// Resolver paired with the priv-sym `[[readyPromise]]`. None after
    /// first resolve/reject. EnsureReadyPromiseRejected allocates fresh
    /// when None.
    pub ready_resolver: RefCell<Option<v8::Global<v8::PromiseResolver>>>,
    /// Stream stashed by the constructor body for `after_install`.
    /// `None` for writers built via `acquire_writable_stream_default_writer`
    /// (the Rust-side helper handles SetUpWritableStreamDefaultWriter
    /// directly without going through post_init).
    pub pending_stream: RefCell<Option<v8::Global<v8::Object>>>,
}

impl WritableStreamDefaultWriter {
    /// Allocate the boxed state with no stashed stream — used by the
    /// `acquire_*` Rust helper which runs SetUpWritableStreamDefaultWriter
    /// directly rather than through the macro's post_init hook.
    fn new_for_internal() -> Self {
        Self {
            closed_resolver: RefCell::new(None),
            ready_resolver: RefCell::new(None),
            pending_stream: RefCell::new(None),
        }
    }
}

#[v8_class]
#[v8_to_string_tag = "WritableStreamDefaultWriter"]
impl WritableStreamDefaultWriter {
    /// `new WritableStreamDefaultWriter(stream)` — spec §4.4.3.
    /// Validates the argument is a WritableStream that is not already
    /// locked, then stashes the stream for `after_install` to consume.
    #[v8_constructor(post_init = "after_install")]
    fn new(
        scope: &mut v8::PinScope,
        stream: v8::Local<v8::Value>,
    ) -> Result<Self, OpError> {
        let stream = v8::Local::<v8::Object>::try_from(stream).map_err(|_| {
            OpError::type_error(
                "WritableStreamDefaultWriter: argument must be a WritableStream",
            )
        })?;
        if !is_writable_stream(scope, stream) {
            return Err(OpError::type_error(
                "WritableStreamDefaultWriter: argument must be a WritableStream",
            ));
        }
        if algorithms::is_writable_stream_locked(scope, stream) {
            return Err(OpError::type_error(
                "WritableStreamDefaultWriter: stream is already locked",
            ));
        }
        let stream_g = v8::Global::new(scope, stream);
        Ok(Self {
            closed_resolver: RefCell::new(None),
            ready_resolver: RefCell::new(None),
            pending_stream: RefCell::new(Some(stream_g)),
        })
    }

    /// `SetUpWritableStreamDefaultWriter(writer, stream)` — spec §4.5.4.
    /// Runs after the macro has installed the Box in field 0 and the
    /// finalizer is registered.
    ///
    /// Set the WRITER_BRAND priv-sym BEFORE the first `with_state` call:
    /// `with_state` itself runs `is_default_writer` (which checks the
    /// brand) and returns None if the brand is missing. Without writing
    /// the brand first, `pending_stream` would be unreachable.
    pub(crate) fn after_install(
        scope: &mut v8::PinScope,
        this: v8::Local<v8::Object>,
    ) -> Result<(), OpError> {
        let brand = slots::private_sym(scope, WRITER_BRAND);
        let true_v: v8::Local<v8::Value> = v8::Boolean::new(scope, true).into();
        this.set_private(scope, brand, true_v);

        let stream_g = with_state(scope, this, |s| s.pending_stream.borrow_mut().take())
            .ok_or_else(|| OpError::error("after_install: with_state returned None"))?
            .ok_or_else(|| OpError::error("after_install: missing pending_stream"))?;
        let stream = v8::Local::new(scope, &stream_g);
        finalize_writer(scope, this, stream);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Wrapper helpers
// ---------------------------------------------------------------------------

pub fn is_default_writer(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>) -> bool {
    let tag = slots::private_sym(scope, WRITER_BRAND);
    obj.has_private(scope, tag).unwrap_or(false)
}

pub fn with_state<R>(
    scope: &mut v8::PinScope,
    writer: v8::Local<v8::Object>,
    f: impl FnOnce(&WritableStreamDefaultWriter) -> R,
) -> Option<R> {
    if !is_default_writer(scope, writer) {
        return None;
    }
    let raw_v8_field = writer.get_internal_field(scope, 0)?;
    let ext = v8::Local::<v8::External>::try_from(raw_v8_field).ok()?;
    let ptr = ext.value() as *const WritableStreamDefaultWriter;
    if ptr.is_null() {
        return None;
    }
    let inst = unsafe { &*ptr };
    Some(f(inst))
}

// ---------------------------------------------------------------------------
// Internal "AcquireWritableStreamDefaultWriter" path — Rust-side construction
// ---------------------------------------------------------------------------

/// `AcquireWritableStreamDefaultWriter(stream)` — spec §4.5.1.
///
/// Build a fresh writer wrapper bound to `stream`. Used by
/// `WritableStream.getWriter()`.
///
/// The macro-emitted constructor's box install + post_init only runs for
/// JS-side `new WritableStreamDefaultWriter(stream)`. Internal-only mints
/// allocate the Box manually here, mirroring the readers + AbortSignal.
pub fn acquire_writable_stream_default_writer<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
) -> Result<v8::Local<'s, v8::Object>, String> {
    if algorithms::is_writable_stream_locked(scope, stream) {
        return Err("WritableStream.getWriter: stream is already locked".to_string());
    }
    let tmpl = WritableStreamDefaultWriter::install(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let writer_obj = inst_tmpl
        .new_instance(scope)
        .ok_or_else(|| "alloc writer instance".to_string())?;
    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    writer_obj.set_prototype(scope, proto_v);
    setup_writer_internal(scope, writer_obj, stream);
    Ok(writer_obj)
}

/// Box install + brand priv-sym + `SetUpWritableStreamDefaultWriter`
/// state dispatch for the Rust-side (`acquire_*`) path. The lock-already
/// check happens in the caller (`acquire_*`) so this helper is
/// infallible.
fn setup_writer_internal(
    scope: &mut v8::PinScope,
    writer: v8::Local<v8::Object>,
    stream: v8::Local<v8::Object>,
) {
    // Build state.
    let state = WritableStreamDefaultWriter::new_for_internal();
    let boxed = Box::new(state);
    let raw_ptr = Box::into_raw(boxed);
    let raw_addr = raw_ptr as usize;
    let ext = v8::External::new(scope, raw_ptr as *mut std::ffi::c_void);
    writer.set_internal_field(0, ext.into());

    // Brand for receiver-check.
    let brand = slots::private_sym(scope, WRITER_BRAND);
    let true_v: v8::Local<v8::Value> = v8::Boolean::new(scope, true).into();
    writer.set_private(scope, brand, true_v);

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        writer,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut WritableStreamDefaultWriter));
        }),
    );
    std::mem::forget(weak);

    finalize_writer(scope, writer, stream);
}

/// `SetUpWritableStreamDefaultWriter(writer, stream)` step 3+ — wire the
/// writer ↔ stream slots and dispatch on stream state to initialize the
/// closedPromise / readyPromise pair. Shared between the JS path's
/// `after_install` hook (which writes the brand priv-sym before calling
/// here) and the Rust `setup_writer_internal` path. Runs after the box
/// has been installed in field 0 AND the brand priv-sym is set, so
/// `with_state` resolves correctly.
fn finalize_writer(
    scope: &mut v8::PinScope,
    writer: v8::Local<v8::Object>,
    stream: v8::Local<v8::Object>,
) {
    // Wire writer.[[stream]] = stream and stream.[[writer]] = writer.
    slots::write_slot(scope, writer, STREAM, stream.into());
    slots::write_slot(scope, stream, WRITER, writer.into());

    // Initialize closedPromise / readyPromise per spec §4.5.4 dispatch.
    let st = with_ws_state(scope, stream, |s| s.state.get()).unwrap_or(WSState::Writable);
    match st {
        WSState::Writable => {
            // ReadyPromise: pending if (CloseQueuedOrInFlight is false AND
            // backpressure is true), else resolved.
            let close_queued = algorithms::writable_stream_close_queued_or_in_flight(scope, stream);
            let bp = with_ws_state(scope, stream, |s| s.backpressure.get()).unwrap_or(false);
            if !close_queued && bp {
                let (p, r) = make_pending_promise(scope);
                slots::write_slot(scope, writer, READY_PROMISE, p.into());
                with_state(scope, writer, |w| {
                    *w.ready_resolver.borrow_mut() = Some(r);
                });
            } else {
                let p = make_resolved_promise(scope);
                slots::write_slot(scope, writer, READY_PROMISE, p.into());
                // No resolver needed (already resolved).
            }
            // closedPromise: pending.
            let (cp, cr) = make_pending_promise(scope);
            slots::write_slot(scope, writer, CLOSED_PROMISE, cp.into());
            with_state(scope, writer, |w| {
                *w.closed_resolver.borrow_mut() = Some(cr);
            });
        }
        WSState::Erroring => {
            let stored = slots::read_slot(scope, stream, slots::STORED_ERROR);
            let p = make_rejected_promise(scope, stored);
            promise_resolve::set_promise_is_handled_to_true(scope, p);
            slots::write_slot(scope, writer, READY_PROMISE, p.into());
            let (cp, cr) = make_pending_promise(scope);
            slots::write_slot(scope, writer, CLOSED_PROMISE, cp.into());
            with_state(scope, writer, |w| {
                *w.closed_resolver.borrow_mut() = Some(cr);
            });
        }
        WSState::Closed => {
            let p1 = make_resolved_promise(scope);
            slots::write_slot(scope, writer, READY_PROMISE, p1.into());
            let p2 = make_resolved_promise(scope);
            slots::write_slot(scope, writer, CLOSED_PROMISE, p2.into());
        }
        WSState::Errored => {
            let stored = slots::read_slot(scope, stream, slots::STORED_ERROR);
            let p1 = make_rejected_promise(scope, stored);
            promise_resolve::set_promise_is_handled_to_true(scope, p1);
            slots::write_slot(scope, writer, READY_PROMISE, p1.into());
            let p2 = make_rejected_promise(scope, stored);
            promise_resolve::set_promise_is_handled_to_true(scope, p2);
            slots::write_slot(scope, writer, CLOSED_PROMISE, p2.into());
        }
    }
}

// ---------------------------------------------------------------------------
// Promise helpers
// ---------------------------------------------------------------------------

fn make_pending_promise<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> (v8::Local<'s, v8::Promise>, v8::Global<v8::PromiseResolver>) {
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let g = v8::Global::new(scope, resolver);
    (promise, g)
}

fn make_resolved_promise<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Promise> {
    algorithms::resolved_undefined_promise(scope)
}

fn make_rejected_promise<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    reason: v8::Local<'s, v8::Value>,
) -> v8::Local<'s, v8::Promise> {
    algorithms::rejected_with_promise(scope, reason)
}

// ---------------------------------------------------------------------------
// IDL methods — getters and methods
// ---------------------------------------------------------------------------

fn closed_getter_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    if !is_default_writer(scope, this) {
        let msg = v8::String::new(scope, "closed: receiver not a writer").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    let v = slots::read_slot(scope, this, CLOSED_PROMISE);
    rv.set(v);
}

fn desired_size_getter_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    if !is_default_writer(scope, this) {
        let msg = v8::String::new(scope, "desiredSize: receiver not a writer").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    // If [[stream]] undefined, throw TypeError per spec §4.4.5.
    let stream_v = slots::read_slot(scope, this, STREAM);
    if stream_v.is_undefined() {
        let msg = v8::String::new(
            scope,
            "Cannot get desiredSize: writer is not associated with a stream",
        )
        .unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    let Ok(stream) = v8::Local::<v8::Object>::try_from(stream_v) else {
        rv.set(v8::null(scope).into());
        return;
    };
    match writable_stream_default_writer_get_desired_size(scope, this, stream) {
        None => rv.set(v8::null(scope).into()),
        Some(n) => rv.set(v8::Number::new(scope, n).into()),
    }
}

fn ready_getter_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    if !is_default_writer(scope, this) {
        let msg = v8::String::new(scope, "ready: receiver not a writer").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    let v = slots::read_slot(scope, this, READY_PROMISE);
    rv.set(v);
}

fn abort_method_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'s>,
) {
    let this = args.this();
    if !is_default_writer(scope, this) {
        let msg = v8::String::new(scope, "abort: receiver not a writer").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    let stream_v = slots::read_slot(scope, this, STREAM);
    let Ok(stream) = v8::Local::<v8::Object>::try_from(stream_v) else {
        let msg = v8::String::new(scope, "Cannot abort a stream using a released writer").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    };
    let reason = args.get(0);
    let promise = algorithms::writable_stream_abort(scope, stream, reason);
    rv.set(promise.into());
}

fn close_method_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'s>,
) {
    let this = args.this();
    if !is_default_writer(scope, this) {
        let msg = v8::String::new(scope, "close: receiver not a writer").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    let stream_v = slots::read_slot(scope, this, STREAM);
    let Ok(stream) = v8::Local::<v8::Object>::try_from(stream_v) else {
        let msg = v8::String::new(scope, "Cannot close a stream using a released writer").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    };
    if algorithms::writable_stream_close_queued_or_in_flight(scope, stream) {
        let msg = v8::String::new(scope, "Cannot close an already-closing stream").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    let promise = algorithms::writable_stream_close(scope, stream);
    rv.set(promise.into());
}

fn release_lock_method_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let this = args.this();
    if !is_default_writer(scope, this) {
        let msg = v8::String::new(scope, "releaseLock: receiver not a writer").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    writable_stream_default_writer_release(scope, this);
}

fn write_method_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'s>,
) {
    let this = args.this();
    if !is_default_writer(scope, this) {
        let msg = v8::String::new(scope, "write: receiver not a writer").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    let stream_v = slots::read_slot(scope, this, STREAM);
    if stream_v.is_undefined() {
        let msg = v8::String::new(scope, "Cannot write to a stream using a released writer").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    let chunk = args.get(0);
    let promise = writable_stream_default_writer_write(scope, this, chunk);
    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Spec algorithms — §4.6
// ---------------------------------------------------------------------------

/// `WritableStreamDefaultWriterGetDesiredSize(writer)` — §4.6.7.
pub fn writable_stream_default_writer_get_desired_size(
    scope: &mut v8::PinScope,
    _writer: v8::Local<v8::Object>,
    stream: v8::Local<v8::Object>,
) -> Option<f64> {
    let st = with_ws_state(scope, stream, |s| s.state.get())?;
    match st {
        WSState::Errored | WSState::Erroring => None,
        WSState::Closed => Some(0.0),
        WSState::Writable => {
            let controller_v = slots::read_slot(scope, stream, slots::CONTROLLER);
            let controller = v8::Local::<v8::Object>::try_from(controller_v).ok()?;
            crate::streams::writable_controller::writable_stream_default_controller_get_desired_size(
                scope, controller,
            )
        }
    }
}

/// `WritableStreamDefaultWriterRelease(writer)` — §4.6.8.
pub fn writable_stream_default_writer_release(
    scope: &mut v8::PinScope,
    writer: v8::Local<v8::Object>,
) {
    let stream_v = slots::read_slot(scope, writer, STREAM);
    let Ok(stream) = v8::Local::<v8::Object>::try_from(stream_v) else {
        return;
    };
    let msg = v8::String::new(
        scope,
        "Writer was released and can no longer be used to monitor the stream's closedness",
    )
    .unwrap();
    let exc = v8::Exception::type_error(scope, msg);
    let exc_l: v8::Local<v8::Value> = exc.into();

    writable_stream_default_writer_ensure_ready_promise_rejected(scope, writer, exc_l);
    writable_stream_default_writer_ensure_closed_promise_rejected(scope, writer, exc_l);

    // Detach.
    slots::delete_slot(scope, stream, WRITER);
    slots::delete_slot(scope, writer, STREAM);
}

/// `WritableStreamDefaultWriterEnsureReadyPromiseRejected(writer, error)` — §4.6.5.
///
/// If the existing readyPromise is pending, reject it. Else allocate a
/// fresh pre-rejected promise and update the priv-sym slot.
pub fn writable_stream_default_writer_ensure_ready_promise_rejected<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    writer: v8::Local<v8::Object>,
    error: v8::Local<'s, v8::Value>,
) {
    // Take the resolver — None means already settled (per our paired
    // storage convention).
    let resolver = with_state(scope, writer, |w| w.ready_resolver.borrow_mut().take()).flatten();
    if let Some(resolver_g) = resolver {
        // Pending — reject the existing promise.
        let resolver_l = v8::Local::new(scope, &resolver_g);
        resolver_l.reject(scope, error);
        // Mark handled per spec.
        let p_v = slots::read_slot(scope, writer, READY_PROMISE);
        if let Ok(p) = v8::Local::<v8::Promise>::try_from(p_v) {
            promise_resolve::set_promise_is_handled_to_true(scope, p);
        }
    } else {
        // Already settled — replace with a fresh pre-rejected promise.
        let p = make_rejected_promise(scope, error);
        promise_resolve::set_promise_is_handled_to_true(scope, p);
        slots::write_slot(scope, writer, READY_PROMISE, p.into());
    }
}

/// `WritableStreamDefaultWriterEnsureClosedPromiseRejected(writer, error)` — §4.6.4.
pub fn writable_stream_default_writer_ensure_closed_promise_rejected<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    writer: v8::Local<v8::Object>,
    error: v8::Local<'s, v8::Value>,
) {
    let resolver = with_state(scope, writer, |w| w.closed_resolver.borrow_mut().take()).flatten();
    if let Some(resolver_g) = resolver {
        let resolver_l = v8::Local::new(scope, &resolver_g);
        resolver_l.reject(scope, error);
        let p_v = slots::read_slot(scope, writer, CLOSED_PROMISE);
        if let Ok(p) = v8::Local::<v8::Promise>::try_from(p_v) {
            promise_resolve::set_promise_is_handled_to_true(scope, p);
        }
    } else {
        let p = make_rejected_promise(scope, error);
        promise_resolve::set_promise_is_handled_to_true(scope, p);
        slots::write_slot(scope, writer, CLOSED_PROMISE, p.into());
    }
}

/// Resolve the writer's readyPromise with undefined. Used by
/// `WritableStreamUpdateBackpressure(stream, false)`.
pub fn writable_stream_default_writer_resolve_ready_promise(
    scope: &mut v8::PinScope,
    writer: v8::Local<v8::Object>,
) {
    let resolver = with_state(scope, writer, |w| w.ready_resolver.borrow_mut().take()).flatten();
    if let Some(resolver_g) = resolver {
        let resolver_l = v8::Local::new(scope, &resolver_g);
        let undef = v8::undefined(scope);
        resolver_l.resolve(scope, undef.into());
    }
}

/// Replace the writer's readyPromise with a fresh pending one. Used by
/// `WritableStreamUpdateBackpressure(stream, true)`.
pub fn writable_stream_default_writer_reset_ready_promise(
    scope: &mut v8::PinScope,
    writer: v8::Local<v8::Object>,
) {
    let (p, r) = make_pending_promise(scope);
    slots::write_slot(scope, writer, READY_PROMISE, p.into());
    with_state(scope, writer, |w| {
        *w.ready_resolver.borrow_mut() = Some(r);
    });
}

/// Resolve the writer's closedPromise with undefined. Used by
/// `WritableStreamFinishInFlightClose`.
pub fn writable_stream_default_writer_resolve_closed_promise(
    scope: &mut v8::PinScope,
    writer: v8::Local<v8::Object>,
) {
    let resolver = with_state(scope, writer, |w| w.closed_resolver.borrow_mut().take()).flatten();
    if let Some(resolver_g) = resolver {
        let resolver_l = v8::Local::new(scope, &resolver_g);
        let undef = v8::undefined(scope);
        resolver_l.resolve(scope, undef.into());
    }
}

/// `WritableStreamDefaultWriterCloseWithErrorPropagation(writer)` — §4.6.3.
pub fn writable_stream_default_writer_close_with_error_propagation<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    writer: v8::Local<v8::Object>,
) -> v8::Local<'s, v8::Promise> {
    let stream_v = slots::read_slot(scope, writer, STREAM);
    let Ok(stream) = v8::Local::<v8::Object>::try_from(stream_v) else {
        let msg = v8::String::new(scope, "Cannot close a stream using a released writer").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        return algorithms::rejected_with_promise(scope, exc.into());
    };
    let st = with_ws_state(scope, stream, |s| s.state.get()).unwrap_or(WSState::Closed);
    if algorithms::writable_stream_close_queued_or_in_flight(scope, stream) || st == WSState::Closed {
        return algorithms::resolved_undefined_promise(scope);
    }
    if st == WSState::Errored {
        let stored = slots::read_slot(scope, stream, slots::STORED_ERROR);
        return algorithms::rejected_with_promise(scope, stored);
    }
    debug_assert!(st == WSState::Writable || st == WSState::Erroring);
    algorithms::writable_stream_close(scope, stream)
}

/// `WritableStreamDefaultWriterWrite(writer, chunk)` — §4.6.10.
pub fn writable_stream_default_writer_write<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    writer: v8::Local<v8::Object>,
    chunk: v8::Local<'s, v8::Value>,
) -> v8::Local<'s, v8::Promise> {
    let stream_v = slots::read_slot(scope, writer, STREAM);
    let Ok(stream) = v8::Local::<v8::Object>::try_from(stream_v) else {
        let msg = v8::String::new(scope, "Cannot write to a stream using a released writer").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        return algorithms::rejected_with_promise(scope, exc.into());
    };
    let controller_v = slots::read_slot(scope, stream, slots::CONTROLLER);
    let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) else {
        return algorithms::resolved_undefined_promise(scope);
    };
    // GetChunkSize per spec.
    let chunk_size = crate::streams::writable_controller::writable_stream_default_controller_get_chunk_size(
        scope, controller, chunk,
    );

    // Reload writer.[[stream]] — per ref impl, we need to confirm the
    // writer is still attached after possible reentrancy from the size
    // algorithm.
    let stream_v_after = slots::read_slot(scope, writer, STREAM);
    if !same_value(scope, stream_v_after, stream.into()) {
        let msg = v8::String::new(scope, "Cannot write to a stream using a released writer").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        return algorithms::rejected_with_promise(scope, exc.into());
    }

    let st = with_ws_state(scope, stream, |s| s.state.get()).unwrap_or(WSState::Errored);
    if st == WSState::Errored {
        let stored = slots::read_slot(scope, stream, slots::STORED_ERROR);
        return algorithms::rejected_with_promise(scope, stored);
    }
    if algorithms::writable_stream_close_queued_or_in_flight(scope, stream) || st == WSState::Closed {
        let msg = v8::String::new(
            scope,
            "The stream is closing or closed and cannot be written to",
        )
        .unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        return algorithms::rejected_with_promise(scope, exc.into());
    }
    if st == WSState::Erroring {
        let stored = slots::read_slot(scope, stream, slots::STORED_ERROR);
        return algorithms::rejected_with_promise(scope, stored);
    }
    debug_assert!(st == WSState::Writable);

    // AddWriteRequest + ControllerWrite.
    let promise = algorithms::writable_stream_add_write_request(scope, stream);

    crate::streams::writable_controller::writable_stream_default_controller_write(
        scope, controller, chunk, chunk_size,
    );

    promise
}

fn same_value<'s>(
    _scope: &mut v8::PinScope<'s, '_>,
    a: v8::Local<v8::Value>,
    b: v8::Local<v8::Value>,
) -> bool {
    a == b
}

// ---------------------------------------------------------------------------
// Public install
// ---------------------------------------------------------------------------

pub fn install(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
    // Macro-emitted FunctionTemplate carries the constructor + Symbol.toStringTag.
    let tmpl = WritableStreamDefaultWriter::install(scope);

    // Patch in the IDL methods on the prototype. They stay raw
    // FunctionCallbacks for the same reason as the readers (need direct
    // args.this() + Promise alloc). See the headers.rs `install_global`
    // pattern for prior art.
    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    let proto: v8::Local<v8::Object> = proto_v.try_into().unwrap();

    install_proto_getter(scope, proto, "closed", closed_getter_callback);
    install_proto_getter(scope, proto, "desiredSize", desired_size_getter_callback);
    install_proto_getter(scope, proto, "ready", ready_getter_callback);

    install_proto_method_on_object(scope, proto, "abort", abort_method_callback);
    install_proto_method_on_object(scope, proto, "close", close_method_callback);
    install_proto_method_on_object(scope, proto, "releaseLock", release_lock_method_callback);
    install_proto_method_on_object(scope, proto, "write", write_method_callback);

    let key = v8::String::new(scope, "WritableStreamDefaultWriter").unwrap();
    global.set(scope, key.into(), class_fn.into());
}

fn install_proto_getter(
    scope: &mut v8::PinScope,
    proto: v8::Local<v8::Object>,
    name: &str,
    cb: impl v8::MapFnTo<v8::FunctionCallback>,
) {
    let key = v8::String::new(scope, name).unwrap();
    let getter_tmpl = v8::FunctionTemplate::new(scope, cb);
    let getter_fn = getter_tmpl.get_function(scope).unwrap();
    let mut desc = v8::PropertyDescriptor::new_from_get_set(
        getter_fn.into(),
        v8::undefined(scope).into(),
    );
    desc.set_configurable(true);
    desc.set_enumerable(true);
    proto.define_property(scope, key.into(), &desc);
}

fn install_proto_method_on_object(
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
