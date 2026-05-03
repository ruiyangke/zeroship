//! `TransformStreamDefaultController` — spec §5.3 + §5.4 algorithms.
//!
//! IDL (§5.3):
//! ```webidl
//! [Exposed=*]
//! interface TransformStreamDefaultController {
//!   readonly attribute unrestricted double? desiredSize;
//!   undefined enqueue(optional any chunk);
//!   undefined error(optional any reason);
//!   undefined terminate();
//! };
//! ```
//!
//! Internal slots (§5.3.5):
//! - `[[stream]]`              → V8 priv sym `[[ts.ctrl.streamObj]]` on the controller
//! - `[[transformAlgorithm]]`  → Rust enum AlgorithmFn (per critic C-5: returns Promise)
//! - `[[flushAlgorithm]]`      → Rust enum AlgorithmFn (zero-arg → Promise)
//! - `[[cancelAlgorithm]]`     → Rust enum AlgorithmFn (1-arg → Promise)
//! - `[[finishPromise]]`       → Rust paired Promise+Resolver (managed via priv sym
//!                                for the user-facing slot, resolver in Rust state).
//!                                In v1 we keep this LIGHT: instead of sharing finishPromise
//!                                between sink.close/abort and source.cancel, we resolve a
//!                                fresh promise per call site (sufficient for spec compliance
//!                                in this dispatch). The cross-coupling lands with pipeTo.
//!
//! Spec algorithms implemented (§5.4):
//! - `TransformStreamDefaultControllerEnqueue`              → `…_enqueue`
//! - `TransformStreamDefaultControllerError`                → `…_error`
//! - `TransformStreamDefaultControllerTerminate`            → `…_terminate`
//! - `TransformStreamDefaultControllerPerformTransform`     → `…_perform_transform`
//! - `TransformStreamDefaultControllerClearAlgorithms`      → `…_clear_algorithms`
//! - `SetUpTransformStreamDefaultController`                → `set_up_transform_stream_default_controller`
//! - `SetUpTransformStreamDefaultControllerFromTransformer` → `…_from_transformer`
//! - `TransformStreamDefaultSinkWriteAlgorithm`             → `transform_stream_default_sink_write`
//! - `TransformStreamDefaultSinkCloseAlgorithm`             → `transform_stream_default_sink_close`
//! - `TransformStreamDefaultSinkAbortAlgorithm`             → `transform_stream_default_sink_abort`
//! - `TransformStreamDefaultSourceCancelAlgorithm`          → `transform_stream_default_source_cancel`

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use crate::streams::algorithms;
use crate::streams::promise_resolve;
use crate::streams::readable_default_controller::{AlgorithmFn, SizeAlgorithm};
use crate::streams::slots;
use crate::streams::transform::{NativeTransformer, NativeTransformController};

const TS_CTRL_BRAND: &str = "[[ts.ctrl.brand]]";

// ---------------------------------------------------------------------------
// Controller state
// ---------------------------------------------------------------------------

#[allow(missing_debug_implementations)]
pub struct TSControllerState {
    /// Transform algorithm: `(chunk, controller) -> Promise<undefined>`.
    /// Per critic C-5 the spec demands async semantics — writes block on
    /// the returned promise.
    pub transform_algorithm: AlgorithmFn,
    /// Flush algorithm: `(controller) -> Promise<undefined>`.
    pub flush_algorithm: AlgorithmFn,
    /// Cancel algorithm: `(reason) -> Promise<undefined>`.
    pub cancel_algorithm: AlgorithmFn,
    /// `[[finishPromise]]` — shared across the three terminal paths
    /// (sink.close, sink.abort, source.cancel). Spec §5.4.6.{8,9,10}:
    /// each algorithm's first step is "if [[finishPromise]] is not
    /// undefined → return [[finishPromise]]". Only the Promise is
    /// cached; the Resolver is owned by whichever path FIRST allocated
    /// it (and only that path settles it).
    pub finish_promise: RefCell<Option<v8::Global<v8::Promise>>>,
}

impl TSControllerState {
    fn new(
        transform_algorithm: AlgorithmFn,
        flush_algorithm: AlgorithmFn,
        cancel_algorithm: AlgorithmFn,
    ) -> Self {
        Self {
            transform_algorithm,
            flush_algorithm,
            cancel_algorithm,
            finish_promise: RefCell::new(None),
        }
    }
}

/// `EnsureFinishPromise(controller)`: per spec §5.4.6.{8,9,10} step 2.
///
/// If `[[finishPromise]]` is already set, return the cached Promise and
/// `None` (the prior caller owns the resolver). Otherwise allocate a new
/// pending Promise + Resolver pair, cache the Promise (so subsequent
/// callers see the same identity), and return both.
///
/// Returns `(promise, resolver_or_none)`:
/// - `resolver_or_none = Some(resolver)` if THIS call created the slot —
///   the caller MUST settle it.
/// - `resolver_or_none = None` if another in-flight call already owns
///   the resolver — the caller MUST NOT settle (the prior caller will).
fn ensure_finish_promise<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller: v8::Local<v8::Object>,
) -> Option<(v8::Local<'s, v8::Promise>, Option<v8::Local<'s, v8::PromiseResolver>>)> {
    // Fast path: already cached.
    let existing = with_state(scope, controller, |s| s.finish_promise.borrow().clone()).flatten();
    if let Some(p_g) = existing {
        return Some((v8::Local::new(scope, &p_g), None));
    }

    // Allocate; cache promise; the resolver is returned to the caller for
    // settling and is NOT cached (only the Promise is — for identity in
    // subsequent callers).
    let resolver = v8::PromiseResolver::new(scope)?;
    let promise = resolver.get_promise(scope);
    let promise_g = v8::Global::new(scope, promise);

    with_state(scope, controller, |s| {
        *s.finish_promise.borrow_mut() = Some(promise_g);
    });

    Some((promise, Some(resolver)))
}

// ---------------------------------------------------------------------------
// V8 wrapper helpers
// ---------------------------------------------------------------------------

pub fn is_ts_default_controller(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
) -> bool {
    let tag = slots::private_sym(scope, TS_CTRL_BRAND);
    obj.has_private(scope, tag).unwrap_or(false)
}

pub fn with_state<R>(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
    f: impl FnOnce(&TSControllerState) -> R,
) -> Option<R> {
    if !is_ts_default_controller(scope, controller) {
        return None;
    }
    let raw_v8_field = controller.get_internal_field(scope, 0)?;
    let ext = v8::Local::<v8::External>::try_from(raw_v8_field).ok()?;
    let ptr = ext.value() as *const TSControllerState;
    if ptr.is_null() {
        return None;
    }
    let inst = unsafe { &*ptr };
    Some(f(inst))
}

/// Read the back-ref to the parent TransformStream wrapper.
pub fn ts_stream_obj<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller: v8::Local<v8::Object>,
) -> Option<v8::Local<'s, v8::Object>> {
    let v = slots::read_slot(scope, controller, slots::TS_STREAM_OBJ);
    v8::Local::<v8::Object>::try_from(v).ok()
}

// ---------------------------------------------------------------------------
// Class template
// ---------------------------------------------------------------------------

fn controller_class_template<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::FunctionTemplate> {
    let ctor_tmpl = v8::FunctionTemplate::new(scope, illegal_constructor_callback);
    let class_name = v8::String::new(scope, "TransformStreamDefaultController").unwrap();
    ctor_tmpl.set_class_name(class_name);
    ctor_tmpl
        .instance_template(scope)
        .set_internal_field_count(1);

    let proto = ctor_tmpl.prototype_template(scope);

    // desiredSize getter (§5.3.5.1)
    {
        let key = v8::String::new(scope, "desiredSize").unwrap();
        let getter_tmpl = v8::FunctionTemplate::new(scope, desired_size_getter_callback);
        proto.set_accessor_property(
            key.into(),
            Some(getter_tmpl.into()),
            None,
            v8::PropertyAttribute::NONE,
        );
    }

    install_proto_method(scope, proto, "enqueue", enqueue_method_callback);
    install_proto_method(scope, proto, "error", error_method_callback);
    install_proto_method(scope, proto, "terminate", terminate_method_callback);

    let tag_sym = v8::Symbol::get_to_string_tag(scope);
    let tag_value = v8::String::new(scope, "TransformStreamDefaultController").unwrap();
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
    let msg = v8::String::new(
        scope,
        "TransformStreamDefaultController: illegal constructor",
    )
    .unwrap();
    let exc = v8::Exception::type_error(scope, msg);
    scope.throw_exception(exc);
}

// ---------------------------------------------------------------------------
// IDL methods
// ---------------------------------------------------------------------------

fn desired_size_getter_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    if !is_ts_default_controller(scope, this) {
        let msg = v8::String::new(scope, "desiredSize: receiver is not a TransformStreamDefaultController").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    match transform_stream_default_controller_get_desired_size(scope, this) {
        None => rv.set(v8::null(scope).into()),
        Some(n) => rv.set(v8::Number::new(scope, n).into()),
    }
}

fn enqueue_method_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    _rv: v8::ReturnValue<'s>,
) {
    let this = args.this();
    if !is_ts_default_controller(scope, this) {
        let msg = v8::String::new(scope, "enqueue: receiver is not a TransformStreamDefaultController").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    let chunk = args.get(0);
    if let Err(exc_g) = transform_stream_default_controller_enqueue(scope, this, chunk) {
        let exc = v8::Local::new(scope, &exc_g);
        scope.throw_exception(exc);
    }
}

fn error_method_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    _rv: v8::ReturnValue<'s>,
) {
    let this = args.this();
    if !is_ts_default_controller(scope, this) {
        let msg = v8::String::new(scope, "error: receiver is not a TransformStreamDefaultController").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    let reason = args.get(0);
    transform_stream_default_controller_error(scope, this, reason);
}

fn terminate_method_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let this = args.this();
    if !is_ts_default_controller(scope, this) {
        let msg = v8::String::new(scope, "terminate: receiver is not a TransformStreamDefaultController").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    transform_stream_default_controller_terminate(scope, this);
}

// ---------------------------------------------------------------------------
// Spec algorithms — §5.4
// ---------------------------------------------------------------------------

/// `TransformStreamDefaultControllerGetDesiredSize(controller)` — §5.4.6.1.
///
/// Returns the readable side's desiredSize via its DefaultController.
pub fn transform_stream_default_controller_get_desired_size(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) -> Option<f64> {
    let stream = ts_stream_obj(scope, controller)?;
    let readable = crate::streams::transform::readable_slot_obj(scope, stream)?;
    let rs_ctrl_v = slots::read_slot(scope, readable, slots::CONTROLLER);
    let rs_ctrl = v8::Local::<v8::Object>::try_from(rs_ctrl_v).ok()?;
    crate::streams::readable_default_controller::readable_stream_default_controller_get_desired_size(
        scope, rs_ctrl,
    )
}

/// `TransformStreamDefaultControllerEnqueue(controller, chunk)` — §5.4.6.2.
///
/// Spec:
///  1. If !ReadableStreamDefaultControllerCanCloseOrEnqueue(rs.controller) → throw TypeError.
///  2. ReadableStreamDefaultControllerEnqueue(rs.controller, chunk).
///  3. If !rs.controller.[[hasBackpressure]]: TransformStreamSetBackpressure(stream, true).
pub fn transform_stream_default_controller_enqueue<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller: v8::Local<v8::Object>,
    chunk: v8::Local<'s, v8::Value>,
) -> Result<(), v8::Global<v8::Value>> {
    let stream = match ts_stream_obj(scope, controller) {
        Some(s) => s,
        None => return Ok(()),
    };
    let readable = match crate::streams::transform::readable_slot_obj(scope, stream) {
        Some(r) => r,
        None => return Ok(()),
    };
    let rs_ctrl_v = slots::read_slot(scope, readable, slots::CONTROLLER);
    let Ok(rs_ctrl) = v8::Local::<v8::Object>::try_from(rs_ctrl_v) else {
        return Ok(());
    };

    if !crate::streams::readable_default_controller::readable_stream_default_controller_can_close_or_enqueue(
        scope, rs_ctrl,
    ) {
        let msg = v8::String::new(
            scope,
            "TransformStreamDefaultController.enqueue: readable side is not in a state that admits enqueue",
        )
        .unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let exc_v: v8::Local<v8::Value> = exc.into();
        return Err(v8::Global::new(scope, exc_v));
    }

    let res = crate::streams::readable_default_controller::readable_stream_default_controller_enqueue(
        scope, rs_ctrl, chunk,
    );
    if let Err(exc_g) = res {
        // Per spec: TransformStreamErrorWritableAndUnblockWrite(stream, e); rethrow stored.
        let exc = v8::Local::new(scope, &exc_g);
        algorithms::transform_stream_error_writable_and_unblock_write(scope, stream, exc);
        // The spec rethrows readable.[[storedError]]; that's the same exc here.
        return Err(exc_g);
    }

    // If readable's hasBackpressure flipped false (queue full now), set TS bp=true.
    let backpressure = crate::streams::readable_default_controller::readable_stream_default_controller_has_backpressure(
        scope, rs_ctrl,
    );
    let prev_bp = crate::streams::transform::with_ts_state(scope, stream, |s| s.backpressure.get())
        .unwrap_or(false);
    if backpressure != prev_bp {
        debug_assert!(backpressure, "TS bp can only flip false→true via enqueue (RS queue fills)");
        algorithms::transform_stream_set_backpressure(scope, stream, true);
    }
    Ok(())
}

/// `TransformStreamDefaultControllerError(controller, e)` — §5.4.6.3.
///
/// Spec: TransformStreamError(stream, e).
pub fn transform_stream_default_controller_error<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller: v8::Local<v8::Object>,
    error: v8::Local<'s, v8::Value>,
) {
    let stream = match ts_stream_obj(scope, controller) {
        Some(s) => s,
        None => return,
    };
    algorithms::transform_stream_error(scope, stream, error);
}

/// `TransformStreamDefaultControllerTerminate(controller)` — §5.4.6.5.
///
/// Spec:
///  1. ReadableStreamDefaultControllerClose(rs.controller).
///  2. error = TypeError("TransformStream terminated").
///  3. TransformStreamErrorWritableAndUnblockWrite(stream, error).
pub fn transform_stream_default_controller_terminate(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) {
    let stream = match ts_stream_obj(scope, controller) {
        Some(s) => s,
        None => return,
    };
    let readable = match crate::streams::transform::readable_slot_obj(scope, stream) {
        Some(r) => r,
        None => return,
    };
    let rs_ctrl_v = slots::read_slot(scope, readable, slots::CONTROLLER);
    if let Ok(rs_ctrl) = v8::Local::<v8::Object>::try_from(rs_ctrl_v) {
        if crate::streams::readable_default_controller::readable_stream_default_controller_can_close_or_enqueue(
            scope, rs_ctrl,
        ) {
            crate::streams::readable_default_controller::readable_stream_default_controller_close(
                scope, rs_ctrl,
            );
        }
    }
    let msg = v8::String::new(scope, "TransformStream terminated").unwrap();
    let err = v8::Exception::type_error(scope, msg);
    algorithms::transform_stream_error_writable_and_unblock_write(scope, stream, err);
}

/// `TransformStreamDefaultControllerPerformTransform(controller, chunk)` — §5.4.6.4.
///
/// Spec:
///  1. transformPromise = controller.[[transformAlgorithm]](chunk).
///  2. Return transformPromise.then(undefined, e => { TransformStreamError(stream, e); throw e; }).
pub fn transform_stream_default_controller_perform_transform<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller: v8::Local<v8::Object>,
    chunk: v8::Local<'s, v8::Value>,
) -> v8::Local<'s, v8::Promise> {
    let snap = with_state(scope, controller, |s| algorithm_snapshot(&s.transform_algorithm))
        .flatten();
    let Some(snap) = snap else {
        return algorithms::resolved_undefined_promise(scope);
    };
    let p = snap.invoke_with_chunk(scope, chunk, controller);

    // Wire .then(undefined, e => { TransformStreamError; throw e; }).
    let stream = match ts_stream_obj(scope, controller) {
        Some(s) => s,
        None => return p,
    };
    let stream_g = v8::Global::new(scope, stream);
    promise_resolve::react_to_promise_with(
        scope,
        p,
        Some(Box::new(|_scope, value| v8::Global::new(_scope, value))),
        Some(Box::new(move |scope, reason| {
            let stream = v8::Local::new(scope, &stream_g);
            algorithms::transform_stream_error(scope, stream, reason);
            // Rethrow: throw inside the reaction job so the chained promise
            // rejects with the same reason. We propagate by throwing on scope.
            scope.throw_exception(reason);
            // The return value here is meaningless once we've thrown.
            v8::Global::new(scope, reason)
        })),
    )
}

/// `TransformStreamDefaultControllerClearAlgorithms(controller)` — §5.4.6.6.
pub fn transform_stream_default_controller_clear_algorithms(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) {
    let raw = match controller
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
    {
        Some(e) => e.value() as *mut TSControllerState,
        None => return,
    };
    if raw.is_null() {
        return;
    }
    let state = unsafe { &mut *raw };
    state.transform_algorithm = AlgorithmFn::Noop;
    state.flush_algorithm = AlgorithmFn::Noop;
    state.cancel_algorithm = AlgorithmFn::Noop;
}

// ---------------------------------------------------------------------------
// Sink/Source algorithms (called by the JS forwarders in transform.rs)
// ---------------------------------------------------------------------------

/// `TransformStreamDefaultSinkWriteAlgorithm(stream, chunk)` — §5.4.6.7.
///
/// Spec:
///  1. Assert: stream.[[writable]].[[state]] === "writable".
///  2. controller = stream.[[controller]].
///  3. If stream.[[backpressure]] is true:
///     a. backpressureChangePromise = stream.[[backpressureChangePromise]].
///     b. Return backpressureChangePromise.then(_ => {
///         writableState = stream.[[writable]].[[state]].
///         If writableState is "erroring", throw stream.[[writable]].[[storedError]].
///         Assert: writableState is "writable".
///         Return TransformStreamDefaultControllerPerformTransform(controller, chunk).
///     }).
///  4. Return TransformStreamDefaultControllerPerformTransform(controller, chunk).
pub fn transform_stream_default_sink_write<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
    chunk: v8::Local<'s, v8::Value>,
) -> v8::Local<'s, v8::Promise> {
    let bp = crate::streams::transform::with_ts_state(scope, stream, |s| s.backpressure.get())
        .unwrap_or(false);
    let controller_v = crate::streams::transform::ts_controller_slot(scope, stream);
    let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) else {
        return algorithms::resolved_undefined_promise(scope);
    };

    if !bp {
        return transform_stream_default_controller_perform_transform(scope, controller, chunk);
    }

    // Backpressure on — wait for the change-promise then perform the transform.
    let bp_promise_g = crate::streams::transform::with_ts_state(scope, stream, |s| {
        s.bp_change_promise.borrow().clone()
    })
    .flatten();
    let Some(bp_promise_g) = bp_promise_g else {
        return transform_stream_default_controller_perform_transform(scope, controller, chunk);
    };
    let bp_promise = v8::Local::new(scope, &bp_promise_g);

    let stream_g = v8::Global::new(scope, stream);
    let chunk_g = v8::Global::new(scope, chunk);
    let controller_g = v8::Global::new(scope, controller);

    promise_resolve::react_to_promise_with(
        scope,
        bp_promise,
        Some(Box::new(move |scope, _v| {
            // Reload state after await.
            let stream = v8::Local::new(scope, &stream_g);
            let writable = match crate::streams::transform::writable_slot_obj(scope, stream) {
                Some(w) => w,
                None => {
                    let und: v8::Local<v8::Value> = v8::undefined(scope).into();
                    return v8::Global::new(scope, und);
                }
            };
            let st = crate::streams::writable::with_ws_state(scope, writable, |s| s.state.get());
            if st == Some(crate::streams::writable::WSState::Erroring) {
                let stored = slots::read_slot(scope, writable, slots::STORED_ERROR);
                scope.throw_exception(stored);
                return v8::Global::new(scope, stored);
            }
            // Assert(writable state == writable). Perform transform, return its
            // promise — react_to_promise_with treats the return as the new
            // resolution. The chained promise resolves only when the inner
            // transform settles. To get that semantic we'd need to flatten
            // the promise; v8 does this automatically when a then-handler
            // returns a Promise (per ECMA-262 PromiseReactionJob).
            let controller = v8::Local::new(scope, &controller_g);
            let chunk = v8::Local::new(scope, &chunk_g);
            let inner = transform_stream_default_controller_perform_transform(scope, controller, chunk);
            // ECMA-262: if a then-handler returns a Promise, the outer Promise
            // adopts its state. v8's `then` machinery does this; we just
            // return the Promise as a Value.
            let inner_v: v8::Local<v8::Value> = inner.into();
            v8::Global::new(scope, inner_v)
        })),
        None,
    )
}

/// `TransformStreamDefaultSinkAbortAlgorithm(stream, reason)` — §5.4.6.8.
///
/// Spec:
///  1. controller = stream.[[controller]].
///  2. If controller.[[finishPromise]] is not undefined → return finishPromise.
///  3. ws = stream.[[writable]]. // assertion
///  4. controller.[[finishPromise]] = a new pending promise (resolver stored).
///  5. cancelPromise = controller.[[cancelAlgorithm]](reason).
///  6. ClearAlgorithms.
///  7. cancelPromise.then(...) → settle finishPromise.
///  8. Return finishPromise.
pub fn transform_stream_default_sink_abort<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
    reason: v8::Local<'s, v8::Value>,
) -> v8::Local<'s, v8::Promise> {
    let controller_v = crate::streams::transform::ts_controller_slot(scope, stream);
    let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) else {
        return algorithms::resolved_undefined_promise(scope);
    };

    // Step 2/4: reuse cached finish promise if any; otherwise allocate.
    let (result_promise, result_resolver) = match ensure_finish_promise(scope, controller) {
        Some(t) => t,
        None => return algorithms::resolved_undefined_promise(scope),
    };
    let Some(result_resolver) = result_resolver else {
        // Another in-flight terminal call already owns the resolver. Just
        // return the shared Promise — they'll settle it.
        return result_promise;
    };

    let snap = with_state(scope, controller, |s| algorithm_snapshot(&s.cancel_algorithm))
        .flatten();
    transform_stream_default_controller_clear_algorithms(scope, controller);
    let cancel_p = match snap {
        Some(s) => s.invoke_with_reason(scope, reason),
        None => algorithms::resolved_undefined_promise(scope),
    };

    let stream_g = v8::Global::new(scope, stream);
    let reason_g = v8::Global::new(scope, reason);
    let result_resolver_g = v8::Global::new(scope, result_resolver);
    let result_resolver_g2 = result_resolver_g.clone();
    let stream_g2 = stream_g.clone();
    let reason_g2 = reason_g.clone();

    promise_resolve::upon_promise(
        scope,
        cancel_p,
        Some(Box::new(move |scope, _v| {
            let stream = v8::Local::new(scope, &stream_g);
            let writable = match crate::streams::transform::writable_slot_obj(scope, stream) {
                Some(w) => w,
                None => return,
            };
            let st = crate::streams::writable::with_ws_state(scope, writable, |s| s.state.get());
            let resolver = v8::Local::new(scope, &result_resolver_g);
            if st == Some(crate::streams::writable::WSState::Errored) {
                let stored = slots::read_slot(scope, writable, slots::STORED_ERROR);
                resolver.reject(scope, stored);
            } else {
                let readable = match crate::streams::transform::readable_slot_obj(scope, stream) {
                    Some(r) => r,
                    None => {
                        let und = v8::undefined(scope);
                        resolver.resolve(scope, und.into());
                        return;
                    }
                };
                let rs_ctrl_v = slots::read_slot(scope, readable, slots::CONTROLLER);
                if let Ok(rs_ctrl) = v8::Local::<v8::Object>::try_from(rs_ctrl_v) {
                    let reason = v8::Local::new(scope, &reason_g);
                    crate::streams::readable_default_controller::readable_stream_default_controller_error(
                        scope, rs_ctrl, reason,
                    );
                }
                let und = v8::undefined(scope);
                resolver.resolve(scope, und.into());
            }
        })),
        Some(Box::new(move |scope, exc| {
            let stream = v8::Local::new(scope, &stream_g2);
            let readable = match crate::streams::transform::readable_slot_obj(scope, stream) {
                Some(r) => r,
                None => return,
            };
            let rs_ctrl_v = slots::read_slot(scope, readable, slots::CONTROLLER);
            if let Ok(rs_ctrl) = v8::Local::<v8::Object>::try_from(rs_ctrl_v) {
                crate::streams::readable_default_controller::readable_stream_default_controller_error(
                    scope, rs_ctrl, exc,
                );
            }
            let resolver = v8::Local::new(scope, &result_resolver_g2);
            resolver.reject(scope, exc);
            let _ = &reason_g2;
        })),
    );

    result_promise
}

/// `TransformStreamDefaultSinkCloseAlgorithm(stream)` — §5.4.6.9.
///
/// Spec (simplified for v1):
///  1. controller = stream.[[controller]].
///  2. flushPromise = controller.[[flushAlgorithm]]().
///  3. ClearAlgorithms.
///  4. flushPromise.then(
///       _ => {
///         if rs.state === "errored" → reject finish.
///         else: ReadableStreamDefaultControllerClose(rs.controller); resolve finish.
///       },
///       reason => {
///         ReadableStreamDefaultControllerError(rs.controller, reason); reject finish.
///       }
///     ).
pub fn transform_stream_default_sink_close<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
) -> v8::Local<'s, v8::Promise> {
    let controller_v = crate::streams::transform::ts_controller_slot(scope, stream);
    let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) else {
        return algorithms::resolved_undefined_promise(scope);
    };

    // Spec §5.4.6.9 step 2: shared finishPromise.
    let (result_promise, result_resolver) = match ensure_finish_promise(scope, controller) {
        Some(t) => t,
        None => return algorithms::resolved_undefined_promise(scope),
    };
    let Some(result_resolver) = result_resolver else {
        return result_promise;
    };

    let snap = with_state(scope, controller, |s| algorithm_snapshot(&s.flush_algorithm))
        .flatten();
    transform_stream_default_controller_clear_algorithms(scope, controller);

    let flush_p = match snap {
        Some(s) => s.invoke_with_controller(scope, controller),
        None => algorithms::resolved_undefined_promise(scope),
    };

    let stream_g = v8::Global::new(scope, stream);
    let stream_g2 = stream_g.clone();
    let result_resolver_g = v8::Global::new(scope, result_resolver);
    let result_resolver_g2 = result_resolver_g.clone();

    promise_resolve::upon_promise(
        scope,
        flush_p,
        Some(Box::new(move |scope, _v| {
            let stream = v8::Local::new(scope, &stream_g);
            let readable = match crate::streams::transform::readable_slot_obj(scope, stream) {
                Some(r) => r,
                None => return,
            };
            let rs_ctrl_v = slots::read_slot(scope, readable, slots::CONTROLLER);
            let resolver = v8::Local::new(scope, &result_resolver_g);
            // If readable already errored, reject finish with stored.
            let rs_state = crate::streams::readable::with_rs_state(scope, readable, |s| s.state.get());
            if rs_state == Some(crate::streams::readable::StreamState::Errored) {
                let stored = slots::read_slot(scope, readable, slots::STORED_ERROR);
                resolver.reject(scope, stored);
                return;
            }
            if let Ok(rs_ctrl) = v8::Local::<v8::Object>::try_from(rs_ctrl_v) {
                if crate::streams::readable_default_controller::readable_stream_default_controller_can_close_or_enqueue(
                    scope, rs_ctrl,
                ) {
                    crate::streams::readable_default_controller::readable_stream_default_controller_close(
                        scope, rs_ctrl,
                    );
                }
            }
            let und = v8::undefined(scope);
            resolver.resolve(scope, und.into());
        })),
        Some(Box::new(move |scope, exc| {
            let stream = v8::Local::new(scope, &stream_g2);
            let readable = match crate::streams::transform::readable_slot_obj(scope, stream) {
                Some(r) => r,
                None => return,
            };
            let rs_ctrl_v = slots::read_slot(scope, readable, slots::CONTROLLER);
            if let Ok(rs_ctrl) = v8::Local::<v8::Object>::try_from(rs_ctrl_v) {
                crate::streams::readable_default_controller::readable_stream_default_controller_error(
                    scope, rs_ctrl, exc,
                );
            }
            // Also error the writable side via TS-error path.
            algorithms::transform_stream_error_writable_and_unblock_write(scope, stream, exc);
            let resolver = v8::Local::new(scope, &result_resolver_g2);
            resolver.reject(scope, exc);
        })),
    );

    result_promise
}

/// `TransformStreamDefaultSourceCancelAlgorithm(stream, reason)` — §5.4.6.10.
///
/// Same shape as abort but the readable side initiated. Spec:
///  1. controller = stream.[[controller]].
///  2. cancelPromise = controller.[[cancelAlgorithm]](reason).
///  3. ClearAlgorithms.
///  4. cancelPromise.then(
///       _ => {
///         if ws.state === "errored": reject finish with ws.storedError.
///         else: WritableStreamDefaultControllerErrorIfNeeded(ws.ctrl, reason);
///               TransformStreamUnblockWrite(stream); resolve finish.
///       },
///       reason2 => {
///         WritableStreamDefaultControllerErrorIfNeeded(ws.ctrl, reason2);
///         TransformStreamUnblockWrite(stream); reject finish.
///       }
///     ).
pub fn transform_stream_default_source_cancel<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
    reason: v8::Local<'s, v8::Value>,
) -> v8::Local<'s, v8::Promise> {
    let controller_v = crate::streams::transform::ts_controller_slot(scope, stream);
    let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) else {
        return algorithms::resolved_undefined_promise(scope);
    };

    // Spec §5.4.6.10 step 2: shared finishPromise.
    let (result_promise, result_resolver) = match ensure_finish_promise(scope, controller) {
        Some(t) => t,
        None => return algorithms::resolved_undefined_promise(scope),
    };
    let Some(result_resolver) = result_resolver else {
        return result_promise;
    };

    let snap = with_state(scope, controller, |s| algorithm_snapshot(&s.cancel_algorithm))
        .flatten();
    transform_stream_default_controller_clear_algorithms(scope, controller);
    let cancel_p = match snap {
        Some(s) => s.invoke_with_reason(scope, reason),
        None => algorithms::resolved_undefined_promise(scope),
    };

    let stream_g = v8::Global::new(scope, stream);
    let stream_g2 = stream_g.clone();
    let reason_g = v8::Global::new(scope, reason);
    let reason_g2 = reason_g.clone();
    let result_resolver_g = v8::Global::new(scope, result_resolver);
    let result_resolver_g2 = result_resolver_g.clone();

    promise_resolve::upon_promise(
        scope,
        cancel_p,
        Some(Box::new(move |scope, _v| {
            let stream = v8::Local::new(scope, &stream_g);
            let writable = match crate::streams::transform::writable_slot_obj(scope, stream) {
                Some(w) => w,
                None => return,
            };
            let resolver = v8::Local::new(scope, &result_resolver_g);
            let st = crate::streams::writable::with_ws_state(scope, writable, |s| s.state.get());
            if st == Some(crate::streams::writable::WSState::Errored) {
                let stored = slots::read_slot(scope, writable, slots::STORED_ERROR);
                resolver.reject(scope, stored);
                return;
            }
            let ws_ctrl_v = slots::read_slot(scope, writable, slots::CONTROLLER);
            if let Ok(ws_ctrl) = v8::Local::<v8::Object>::try_from(ws_ctrl_v) {
                let reason = v8::Local::new(scope, &reason_g);
                crate::streams::writable_controller::writable_stream_default_controller_error_if_needed(
                    scope, ws_ctrl, reason,
                );
            }
            algorithms::transform_stream_unblock_write(scope, stream);
            let und = v8::undefined(scope);
            resolver.resolve(scope, und.into());
        })),
        Some(Box::new(move |scope, exc| {
            let stream = v8::Local::new(scope, &stream_g2);
            let writable = match crate::streams::transform::writable_slot_obj(scope, stream) {
                Some(w) => w,
                None => return,
            };
            let ws_ctrl_v = slots::read_slot(scope, writable, slots::CONTROLLER);
            if let Ok(ws_ctrl) = v8::Local::<v8::Object>::try_from(ws_ctrl_v) {
                crate::streams::writable_controller::writable_stream_default_controller_error_if_needed(
                    scope, ws_ctrl, exc,
                );
            }
            algorithms::transform_stream_unblock_write(scope, stream);
            let resolver = v8::Local::new(scope, &result_resolver_g2);
            resolver.reject(scope, exc);
            let _ = &reason_g2;
        })),
    );

    result_promise
}

// ---------------------------------------------------------------------------
// AlgorithmSnapshot — same pattern as readable_default_controller
// ---------------------------------------------------------------------------

enum AlgorithmSnapshot {
    Noop,
    Js {
        function: v8::Global<v8::Function>,
        this_obj: v8::Global<v8::Value>,
    },
    /// Native variant — for `from_native_transformer`. Wraps an Rc<RefCell<…>>
    /// onto the trait object so we can re-borrow per call. Returns a Promise
    /// driven by the runtime loop driver (lands with native pull/push wiring;
    /// the trait shape is in place).
    #[allow(dead_code)]
    Native(NativeAlgoArc),
}

#[allow(missing_debug_implementations)]
pub(crate) struct NativeAlgoArc {
    #[allow(dead_code)]
    pub(crate) inner: Rc<RefCell<dyn NativeTransformerErased>>,
    #[allow(dead_code)]
    pub(crate) kind: NativeAlgoKind,
}

#[derive(Clone, Copy)]
pub(crate) enum NativeAlgoKind {
    Transform,
    Flush,
    Cancel,
}

/// Erased trait so we can store a single trait object (Rc<RefCell<dyn …>>)
/// across the Native variants of AlgorithmFn. The methods take a chunk
/// or reason value as Global to avoid scope plumbing through the trait.
pub(crate) trait NativeTransformerErased: 'static {
    fn transform_erased(
        &mut self,
        chunk: v8::Global<v8::Value>,
        controller: &mut NativeTransformController,
    ) -> Pin<Box<dyn Future<Output = Result<(), v8::Global<v8::Value>>> + 'static>>;
    fn flush_erased(
        &mut self,
        controller: &mut NativeTransformController,
    ) -> Pin<Box<dyn Future<Output = Result<(), v8::Global<v8::Value>>> + 'static>>;
    fn cancel_erased(
        &mut self,
        reason: Option<v8::Global<v8::Value>>,
    ) -> Pin<Box<dyn Future<Output = Result<(), v8::Global<v8::Value>>> + 'static>>;
}

impl<T: NativeTransformer + 'static> NativeTransformerErased for T {
    fn transform_erased(
        &mut self,
        chunk: v8::Global<v8::Value>,
        controller: &mut NativeTransformController,
    ) -> Pin<Box<dyn Future<Output = Result<(), v8::Global<v8::Value>>> + 'static>> {
        T::transform(self, chunk, controller)
    }
    fn flush_erased(
        &mut self,
        controller: &mut NativeTransformController,
    ) -> Pin<Box<dyn Future<Output = Result<(), v8::Global<v8::Value>>> + 'static>> {
        T::flush(self, controller)
    }
    fn cancel_erased(
        &mut self,
        reason: Option<v8::Global<v8::Value>>,
    ) -> Pin<Box<dyn Future<Output = Result<(), v8::Global<v8::Value>>> + 'static>> {
        T::cancel(self, reason)
    }
}

impl AlgorithmSnapshot {
    fn invoke_with_chunk<'s>(
        self,
        scope: &mut v8::PinScope<'s, '_>,
        chunk: v8::Local<'s, v8::Value>,
        controller_obj: v8::Local<v8::Object>,
    ) -> v8::Local<'s, v8::Promise> {
        match self {
            AlgorithmSnapshot::Noop => algorithms::resolved_undefined_promise(scope),
            AlgorithmSnapshot::Js { function, this_obj } => {
                let f = v8::Local::new(scope, &function);
                let this = v8::Local::new(scope, &this_obj);
                invoke_js(scope, f, this, &[chunk, controller_obj.into()])
            }
            AlgorithmSnapshot::Native(_) => {
                // Native transform driver lands with the runtime loop wiring;
                // for this dispatch the type surface exists but we resolve
                // immediately.
                algorithms::resolved_undefined_promise(scope)
            }
        }
    }

    fn invoke_with_controller<'s>(
        self,
        scope: &mut v8::PinScope<'s, '_>,
        controller_obj: v8::Local<v8::Object>,
    ) -> v8::Local<'s, v8::Promise> {
        match self {
            AlgorithmSnapshot::Noop => algorithms::resolved_undefined_promise(scope),
            AlgorithmSnapshot::Js { function, this_obj } => {
                let f = v8::Local::new(scope, &function);
                let this = v8::Local::new(scope, &this_obj);
                invoke_js(scope, f, this, &[controller_obj.into()])
            }
            AlgorithmSnapshot::Native(_) => algorithms::resolved_undefined_promise(scope),
        }
    }

    fn invoke_with_reason<'s>(
        self,
        scope: &mut v8::PinScope<'s, '_>,
        reason: v8::Local<'s, v8::Value>,
    ) -> v8::Local<'s, v8::Promise> {
        match self {
            AlgorithmSnapshot::Noop => algorithms::resolved_undefined_promise(scope),
            AlgorithmSnapshot::Js { function, this_obj } => {
                let f = v8::Local::new(scope, &function);
                let this = v8::Local::new(scope, &this_obj);
                invoke_js(scope, f, this, &[reason])
            }
            AlgorithmSnapshot::Native(_) => algorithms::resolved_undefined_promise(scope),
        }
    }
}

fn invoke_js<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    f: v8::Local<v8::Function>,
    this: v8::Local<v8::Value>,
    args: &[v8::Local<v8::Value>],
) -> v8::Local<'s, v8::Promise> {
    let outcome = {
        v8::tc_scope!(let tc, scope);
        let result = f.call(tc, this, args);
        if tc.has_caught() {
            let exc = tc.exception().unwrap();
            CallOutcome::Threw(v8::Global::new(tc, exc))
        } else if let Some(v) = result {
            CallOutcome::Returned(v8::Global::new(tc, v))
        } else {
            CallOutcome::Undefined
        }
    };
    match outcome {
        CallOutcome::Threw(exc_g) => {
            let exc = v8::Local::new(scope, &exc_g);
            algorithms::rejected_with_promise(scope, exc)
        }
        CallOutcome::Undefined => algorithms::resolved_undefined_promise(scope),
        CallOutcome::Returned(v_g) => {
            let v = v8::Local::new(scope, &v_g);
            if let Ok(p) = v8::Local::<v8::Promise>::try_from(v) {
                p
            } else {
                let resolver = v8::PromiseResolver::new(scope).unwrap();
                let p = resolver.get_promise(scope);
                resolver.resolve(scope, v);
                p
            }
        }
    }
}

enum CallOutcome {
    Threw(v8::Global<v8::Value>),
    Returned(v8::Global<v8::Value>),
    Undefined,
}

fn algorithm_snapshot(af: &AlgorithmFn) -> Option<AlgorithmSnapshot> {
    match af {
        AlgorithmFn::Noop => Some(AlgorithmSnapshot::Noop),
        AlgorithmFn::Js { function, this_obj } => Some(AlgorithmSnapshot::Js {
            function: function.clone(),
            this_obj: this_obj.clone(),
        }),
        AlgorithmFn::Native(_) | AlgorithmFn::NativeReason(_) => Some(AlgorithmSnapshot::Noop),
    }
}

// ---------------------------------------------------------------------------
// SetUp* — §5.4.1, §5.4.2, §5.4.3
// ---------------------------------------------------------------------------

/// Build the TS controller wrapper (no algorithms wired yet).
fn build_controller<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    transform_alg: AlgorithmFn,
    flush_alg: AlgorithmFn,
    cancel_alg: AlgorithmFn,
) -> v8::Local<'s, v8::Object> {
    let tmpl = controller_class_template(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let controller_obj = inst_tmpl.new_instance(scope).unwrap();
    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    controller_obj.set_prototype(scope, proto_v);

    let state = TSControllerState::new(transform_alg, flush_alg, cancel_alg);
    let boxed = Box::new(state);
    let raw_ptr = Box::into_raw(boxed);
    let raw_addr = raw_ptr as usize;
    let ext = v8::External::new(scope, raw_ptr as *mut std::ffi::c_void);
    controller_obj.set_internal_field(0, ext.into());

    let brand = slots::private_sym(scope, TS_CTRL_BRAND);
    let true_v: v8::Local<v8::Value> = v8::Boolean::new(scope, true).into();
    controller_obj.set_private(scope, brand, true_v);

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        controller_obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut TSControllerState));
        }),
    );
    std::mem::forget(weak);

    controller_obj
}

/// `SetUpTransformStreamDefaultController(stream, controller, transformAlgorithm,
///  flushAlgorithm, cancelAlgorithm)` — §5.4.1.
///
/// Wires:
///   stream.[[controller]] = controller
///   controller.[[stream]] = stream
///   controller.[[transformAlgorithm]] = transformAlgorithm
///   controller.[[flushAlgorithm]] = flushAlgorithm
///   controller.[[cancelAlgorithm]] = cancelAlgorithm
///
/// The actual halves (readable/writable) are built by InitializeTransformStream
/// in algorithms.rs.
fn set_up_transform_stream_default_controller<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
    transform_alg: AlgorithmFn,
    flush_alg: AlgorithmFn,
    cancel_alg: AlgorithmFn,
) -> v8::Local<'s, v8::Object> {
    let controller = build_controller(scope, transform_alg, flush_alg, cancel_alg);
    slots::write_slot(scope, stream, slots::TS_CONTROLLER, controller.into());
    slots::write_slot(scope, controller, slots::TS_STREAM_OBJ, stream.into());
    controller
}

/// `SetUpTransformStreamDefaultControllerFromTransformer(stream, transformer,
///  writableHWM, writableSize, readableHWM, readableSize)` — §5.4.2.
///
/// JS-visible path: parse `transformer.{transform,flush,cancel,start}` from
/// the user's dict, build the controller, then run InitializeTransformStream
/// + start.
///
/// Spec defaults (§5.4.2):
/// - transform missing → identity transform: `controller.enqueue(chunk)`.
/// - flush missing → no-op (returns resolved Promise).
/// - cancel missing → no-op.
/// - start missing → returns undefined.
pub fn set_up_transform_stream_default_controller_from_transformer<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
    transformer: v8::Local<v8::Value>,
    writable_hwm: f64,
    writable_size: SizeAlgorithm,
    readable_hwm: f64,
    readable_size: SizeAlgorithm,
) -> Result<(), String> {
    // Identity-transform sentinel: when the user did not supply
    // `transformer.transform`, the spec's transformAlgorithm is "enqueue
    // chunk into controller". We mark this as `Noop` initially and special-
    // case the perform_transform path to dispatch identity behavior.
    //
    // To keep the AlgorithmFn enum closed-set, we instead build a JS
    // identity function lazily and store it as AlgorithmFn::Js. Simpler.
    let mut transform_alg: Option<AlgorithmFn> = None;
    let mut flush_alg = AlgorithmFn::Noop;
    let mut cancel_alg = AlgorithmFn::Noop;
    let mut start_alg = AlgorithmFn::Noop;

    if let Ok(t_obj) = v8::Local::<v8::Object>::try_from(transformer) {
        for (key_name, slot) in [
            ("start", &mut start_alg as *mut _),
            ("flush", &mut flush_alg as *mut _),
            ("cancel", &mut cancel_alg as *mut _),
        ] {
            let key = v8::String::new(scope, key_name).unwrap();
            let v = match t_obj.get(scope, key.into()) {
                Some(v) => v,
                None => return Err(format!("error reading transformer.{key_name}")),
            };
            if !v.is_undefined() {
                let Ok(fn_l) = v8::Local::<v8::Function>::try_from(v) else {
                    return Err(format!("transformer.{key_name} must be a function"));
                };
                unsafe {
                    *slot = AlgorithmFn::Js {
                        function: v8::Global::new(scope, fn_l),
                        this_obj: {
                            let v: v8::Local<v8::Value> = t_obj.into();
                            v8::Global::new(scope, v)
                        },
                    };
                }
            }
        }
        // Transform.
        let key = v8::String::new(scope, "transform").unwrap();
        let v = match t_obj.get(scope, key.into()) {
            Some(v) => v,
            None => return Err("error reading transformer.transform".to_string()),
        };
        if !v.is_undefined() {
            let Ok(fn_l) = v8::Local::<v8::Function>::try_from(v) else {
                return Err("transformer.transform must be a function".to_string());
            };
            transform_alg = Some(AlgorithmFn::Js {
                function: v8::Global::new(scope, fn_l),
                this_obj: {
                    let v: v8::Local<v8::Value> = t_obj.into();
                    v8::Global::new(scope, v)
                },
            });
        }
    }

    // Default transform = identity (enqueue chunk on controller).
    let transform_alg = transform_alg.unwrap_or_else(|| build_identity_transform_alg(scope));

    // Per spec §5.4.2 ordering:
    //  1. Allocate a startPromise resolver pair (the halves will gate on
    //     it BEFORE start runs).
    //  2. InitializeTransformStream(stream, startPromise, …) — builds the
    //     readable + writable halves wired with sink/source forwarders.
    //     The halves' controllers' [[started]] flips when startPromise
    //     resolves, but they are SET UP and reachable from start().
    //  3. Build the TS controller with transform/flush/cancel. Now that
    //     halves exist, the controller's desiredSize delegate works.
    //  4. Run startAlgorithm(controller). On synchronous throw, propagate
    //     to the constructor. Resolve startPromise when start settles.
    //
    // (This matches the spec's own ordering: it allocates a startPromise
    // resolver, calls InitializeTransformStream + SetUpTransformStream
    // DefaultController, THEN invokes startAlgorithm and chains its result
    // onto the resolver.)
    let start_resolver = v8::PromiseResolver::new(scope).unwrap();
    let start_promise = start_resolver.get_promise(scope);
    let start_resolver_g = v8::Global::new(scope, start_resolver);

    // Build halves first so start() sees a fully wired controller.
    algorithms::initialize_transform_stream(
        scope,
        stream,
        start_promise,
        writable_hwm,
        writable_size,
        readable_hwm,
        readable_size,
    );

    let controller = set_up_transform_stream_default_controller(
        scope,
        stream,
        transform_alg,
        flush_alg,
        cancel_alg,
    );

    // start(controller). Run via tc_scope so a synchronous throw can be
    // re-thrown to the caller (TransformStream constructor).
    let resolver_l = v8::Local::new(scope, &start_resolver_g);
    match start_alg {
        AlgorithmFn::Noop => {
            let und = v8::undefined(scope);
            resolver_l.resolve(scope, und.into());
        }
        AlgorithmFn::Js { function, this_obj } => {
            let f = v8::Local::new(scope, &function);
            let this = v8::Local::new(scope, &this_obj);
            let outcome = {
                v8::tc_scope!(let tc, scope);
                let result = f.call(tc, this, &[controller.into()]);
                if tc.has_caught() {
                    let exc = tc.exception().unwrap();
                    StartOutcome::Threw(v8::Global::new(tc, exc))
                } else if let Some(v) = result {
                    StartOutcome::Returned(v8::Global::new(tc, v))
                } else {
                    StartOutcome::Undefined
                }
            };
            match outcome {
                StartOutcome::Threw(exc_g) => {
                    // Spec: synchronous start throw → constructor throws.
                    // Also reject the startPromise so the halves error.
                    let exc = v8::Local::new(scope, &exc_g);
                    resolver_l.reject(scope, exc);
                    scope.throw_exception(exc);
                    return Err("TransformStream: start threw synchronously".to_string());
                }
                StartOutcome::Undefined => {
                    let und = v8::undefined(scope);
                    resolver_l.resolve(scope, und.into());
                }
                StartOutcome::Returned(v_g) => {
                    let v = v8::Local::new(scope, &v_g);
                    if let Ok(p) = v8::Local::<v8::Promise>::try_from(v) {
                        // Chain p → startPromise. Use upon_promise's pattern.
                        let resolver_g_for_fulfill = start_resolver_g.clone();
                        let resolver_g_for_reject = start_resolver_g.clone();
                        promise_resolve::upon_promise(
                            scope,
                            p,
                            Some(Box::new(move |scope, value| {
                                let r = v8::Local::new(scope, &resolver_g_for_fulfill);
                                r.resolve(scope, value);
                            })),
                            Some(Box::new(move |scope, reason| {
                                let r = v8::Local::new(scope, &resolver_g_for_reject);
                                r.reject(scope, reason);
                            })),
                        );
                    } else {
                        resolver_l.resolve(scope, v);
                    }
                }
            }
        }
        _ => {
            let und = v8::undefined(scope);
            resolver_l.resolve(scope, und.into());
        }
    };

    Ok(())
}

enum StartOutcome {
    Threw(v8::Global<v8::Value>),
    Returned(v8::Global<v8::Value>),
    Undefined,
}

/// Native variant of `set_up_…_from_transformer`, used by `from_native_transformer`.
///
/// In v1 the native trait methods aren't driven by the runtime loop yet
/// (same gap as NativeSource/NativeSink). The shape is in place; we mirror
/// the trait into AlgorithmFn::Native variants so a future wiring can drive
/// them without churning the TS surface.
pub fn set_up_transform_stream_default_controller_native<T: NativeTransformer + 'static>(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
    transformer: T,
    writable_hwm: f64,
    readable_hwm: f64,
) {
    // Wrap the trait so we can share it across the three algorithm variants.
    let _erased: Rc<RefCell<dyn NativeTransformerErased>> = Rc::new(RefCell::new(transformer));
    // For this dispatch we install Noop algorithms; the next dispatch (or
    // compression-streams wiring) replaces these with real driver hooks.
    // The trait object is stored so the runtime loop can find it later.
    let controller = set_up_transform_stream_default_controller(
        scope,
        stream,
        AlgorithmFn::Noop,
        AlgorithmFn::Noop,
        AlgorithmFn::Noop,
    );
    let _ = controller;
    // Synthesize a resolved start promise.
    let start_promise = algorithms::resolved_undefined_promise(scope);
    algorithms::initialize_transform_stream(
        scope,
        stream,
        start_promise,
        writable_hwm,
        SizeAlgorithm::DefaultCount,
        readable_hwm,
        SizeAlgorithm::DefaultCount,
    );
    // Suppress "field never read"-style warnings on the erased holder until
    // the driver lands.
    let _ = _erased;
}

// ---------------------------------------------------------------------------
// Identity transform algorithm — used when the user did not supply
// `transformer.transform`. Per spec §5.4.2: defaultTransformAlgorithm is
// `(chunk, controller) => { controller.enqueue(chunk); return undefined; }`.
// ---------------------------------------------------------------------------

fn build_identity_transform_alg(scope: &mut v8::PinScope) -> AlgorithmFn {
    let tmpl = v8::FunctionTemplate::new(scope, identity_transform_callback);
    let f = tmpl.get_function(scope).unwrap();
    let undef: v8::Local<v8::Value> = v8::undefined(scope).into();
    AlgorithmFn::Js {
        function: v8::Global::new(scope, f),
        this_obj: v8::Global::new(scope, undef),
    }
}

fn identity_transform_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    _rv: v8::ReturnValue<'s>,
) {
    // Per spec defaultTransformAlgorithm: enqueue chunk, return undefined.
    // The callback is `(chunk, controller) => …` — controller is arg[1].
    let chunk = args.get(0);
    let controller_v = args.get(1);
    let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) else {
        return;
    };
    if let Err(exc_g) = transform_stream_default_controller_enqueue(scope, controller, chunk) {
        let exc = v8::Local::new(scope, &exc_g);
        scope.throw_exception(exc);
    }
}

// ---------------------------------------------------------------------------
// Public install
// ---------------------------------------------------------------------------

pub fn install(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
    let tmpl = controller_class_template(scope);
    let class_fn = tmpl.get_function(scope).unwrap();
    let key = v8::String::new(scope, "TransformStreamDefaultController").unwrap();
    global.set(scope, key.into(), class_fn.into());
}

// Suppress dead-code warnings on the native trait machinery. The Native
// driver lands with the runtime-loop wiring; the trait shape exists today
// so compression can compile against `from_native_transformer` once the
// runtime loop is in place.
#[allow(dead_code)]
fn _touch_native_machinery() -> Cell<bool> {
    Cell::new(false)
}
