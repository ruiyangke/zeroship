//! `ReadableStreamDefaultController` — spec §3.6 + §3.10 algorithms.
//!
//! Hand-rolled rather than macro-driven. Methods need `args.this()` to
//! key off the controller wrapper's V8 identity (private symbols
//! `streamObj`, etc).
//!
//! IDL (§3.6):
//! ```webidl
//! [Exposed=*]
//! interface ReadableStreamDefaultController {
//!   readonly attribute unrestricted double? desiredSize;
//!   undefined close();
//!   undefined enqueue(optional any chunk);
//!   undefined error(optional any e);
//! };
//! ```
//!
//! Internal slots (§3.6.5):
//! - `[[cancelAlgorithm]]`, `[[pullAlgorithm]]`               → Rust enum AlgorithmFn
//! - `[[strategySizeAlgorithm]]`                              → Rust enum SizeAlgorithm
//! - `[[strategyHWM]]`                                        → Rust f64
//! - `[[queue]]`, `[[queueTotalSize]]`                        → Rust ValueQueue (queue.rs)
//! - `[[started]]`, `[[pulling]]`, `[[pullAgain]]`, `[[closeRequested]]` → Rust Cell<bool>
//! - `[[stream]]` (back-ref)                                   → V8 priv sym `streamObj`
//!
//! Internal methods (spec §3.6.6):
//! - `[[CancelSteps]](reason)`  → `cancel_steps`
//! - `[[PullSteps]](readRequest)` → `pull_steps`
//! - `[[ReleaseSteps]]()`        → `release_steps`

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use crate::streams::algorithms;
use crate::streams::promise_resolve;
use crate::streams::queue::{is_non_negative_number, ValueQueue};
use crate::streams::readable::{NativeReadableController, NativeSource, StreamState};
use crate::streams::slots::{self, CONTROLLER};

const STREAM_OBJ_SLOT: &str = "[[ctrl.streamObj]]";

// ---------------------------------------------------------------------------
// Controller state
// ---------------------------------------------------------------------------

/// `Box<DefaultControllerState>` lives in the controller wrapper's V8
/// internal field 0.
#[allow(missing_debug_implementations)]
pub struct DefaultControllerState {
    pub queue: ValueQueue,
    pub strategy_hwm: f64,
    pub strategy_size: SizeAlgorithm,
    pub pull_algorithm: AlgorithmFn,
    pub cancel_algorithm: AlgorithmFn,
    pub started: Cell<bool>,
    pub close_requested: Cell<bool>,
    pub pulling: Cell<bool>,
    pub pull_again: Cell<bool>,
}

impl DefaultControllerState {
    fn new(
        hwm: f64,
        size: SizeAlgorithm,
        pull_algorithm: AlgorithmFn,
        cancel_algorithm: AlgorithmFn,
    ) -> Self {
        Self {
            queue: ValueQueue::new(),
            strategy_hwm: hwm,
            strategy_size: size,
            pull_algorithm,
            cancel_algorithm,
            started: Cell::new(false),
            close_requested: Cell::new(false),
            pulling: Cell::new(false),
            pull_again: Cell::new(false),
        }
    }
}

// ---------------------------------------------------------------------------
// SizeAlgorithm — spec §6.2 / §6.3 + user-supplied
// ---------------------------------------------------------------------------

/// Size algorithm enum (D-2: pure-Rust slot for default streams; the
/// `Js` variant carries a `v8::Global<Function>` for user callbacks).
#[allow(missing_debug_implementations)]
pub enum SizeAlgorithm {
    /// Default for default streams when user provided nothing — always returns 1.
    DefaultCount,
    /// Spec CountQueuingStrategy size — returns 1.
    Count,
    /// Spec ByteLengthQueuingStrategy size — returns chunk.byteLength.
    ByteLength,
    /// User-supplied JS function. Called with `(chunk)` and `this = undefined`
    /// per WebIDL §3.7 callback function rules.
    Js(v8::Global<v8::Function>),
}

impl SizeAlgorithm {
    /// Invoke the size algorithm. Returns `Err(exc_global)` if the user
    /// callback throws (spec: propagate the exception via the controller's
    /// enqueue path which calls `error(stream, e)`).
    pub fn invoke(
        &self,
        scope: &mut v8::PinScope,
        chunk: v8::Local<v8::Value>,
    ) -> Result<f64, v8::Global<v8::Value>> {
        match self {
            SizeAlgorithm::DefaultCount | SizeAlgorithm::Count => Ok(1.0),
            SizeAlgorithm::ByteLength => {
                if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(chunk) {
                    Ok(view.byte_length() as f64)
                } else if let Ok(buf) = v8::Local::<v8::ArrayBuffer>::try_from(chunk) {
                    Ok(buf.byte_length() as f64)
                } else {
                    let msg = v8::String::new(scope, "chunk has no byteLength").unwrap();
                    let exc = v8::Exception::type_error(scope, msg);
                    Err(v8::Global::new(scope, exc))
                }
            }
            SizeAlgorithm::Js(fn_g) => {
                let fn_l = v8::Local::new(scope, fn_g);
                let this_l = v8::undefined(scope);
                let (call_outcome, exc_g) = {
                    v8::tc_scope!(let tc, scope);
                    let result = fn_l.call(tc, this_l.into(), &[chunk]);
                    if tc.has_caught() {
                        let exc = tc.exception().unwrap();
                        (None, Some(v8::Global::new(tc, exc)))
                    } else {
                        // ToNumber on the result.
                        match result.and_then(|v| v.to_number(tc)) {
                            Some(n) => (Some(n.value()), None),
                            None => {
                                let exc = tc.exception().unwrap_or_else(|| {
                                    let msg = v8::String::new(tc, "size: ToNumber failed").unwrap();
                                    v8::Exception::type_error(tc, msg)
                                });
                                (None, Some(v8::Global::new(tc, exc)))
                            }
                        }
                    }
                };
                match (call_outcome, exc_g) {
                    (Some(n), _) => Ok(n),
                    (None, Some(e)) => Err(e),
                    (None, None) => {
                        let msg = v8::String::new(scope, "size: unreachable").unwrap();
                        let exc = v8::Exception::type_error(scope, msg);
                        Err(v8::Global::new(scope, exc))
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// CallOutcome helper — used to bridge tc_scope!'s borrow lifetime
// ---------------------------------------------------------------------------

enum CallOutcome {
    Threw(v8::Global<v8::Value>),
    Returned(v8::Global<v8::Value>),
    Undefined,
}

// ---------------------------------------------------------------------------
// AlgorithmFn — pull / cancel / start
// ---------------------------------------------------------------------------

/// Pull / cancel / start algorithm — variants:
///  - `Js`: user-supplied JS function from underlyingSource.
///  - `Native`: Rust trait method (NativeSource).
///  - `Noop`: spec default ("return a promise resolved with undefined").
#[allow(missing_debug_implementations)]
pub enum AlgorithmFn {
    /// User-supplied JS callback. `this_obj` is the underlyingSource
    /// dictionary itself, per spec.
    Js {
        function: v8::Global<v8::Function>,
        this_obj: v8::Global<v8::Value>,
    },
    /// No-op: returns a resolved Promise<undefined>.
    Noop,
    /// Native (Rust trait method) — used by `from_native_source`. The
    /// future is driven by the runtime loop; this dispatch sets up the
    /// type surface but defers wiring to the next chunk.
    #[allow(dead_code)]
    Native(
        Box<
            dyn FnMut(
                v8::Global<v8::Object>,
            )
                -> Pin<Box<dyn Future<Output = Result<v8::Global<v8::Value>, v8::Global<v8::Value>>>>>,
        >,
    ),
    /// Native (Rust closure) for cancel — accepts the reason as
    /// Option<Global>.
    #[allow(dead_code)]
    NativeReason(
        Box<
            dyn FnMut(
                Option<v8::Global<v8::Value>>,
            )
                -> Pin<Box<dyn Future<Output = Result<(), v8::Global<v8::Value>>>>>,
        >,
    ),
}

impl AlgorithmFn {
    /// Invoke the pull/start algorithm with a single argument
    /// (controller wrapper). Returns the promise the user/native code
    /// produced (or `resolved(undefined)` for Noop).
    pub fn invoke_with_controller<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        controller_obj: v8::Local<v8::Object>,
    ) -> v8::Local<'s, v8::Promise> {
        match self {
            AlgorithmFn::Noop => algorithms::resolved_undefined_promise(scope),
            AlgorithmFn::Js { function, this_obj } => {
                let f = v8::Local::new(scope, function);
                let this = v8::Local::new(scope, this_obj);
                let outcome = {
                    v8::tc_scope!(let tc, scope);
                    let result = f.call(tc, this, &[controller_obj.into()]);
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
            // Native algorithms aren't driven in this dispatch — they
            // require the runtime loop wiring (§VII.5). For now, treat
            // them as a deferred no-op that resolves immediately.
            AlgorithmFn::Native(_) | AlgorithmFn::NativeReason(_) => {
                algorithms::resolved_undefined_promise(scope)
            }
        }
    }

    /// Invoke the cancel algorithm with a `reason` argument. Returns the
    /// resulting Promise<undefined> per spec.
    pub fn invoke_with_reason<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        reason: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Promise> {
        match self {
            AlgorithmFn::Noop => algorithms::resolved_undefined_promise(scope),
            AlgorithmFn::Js { function, this_obj } => {
                let f = v8::Local::new(scope, function);
                let this = v8::Local::new(scope, this_obj);
                let outcome = {
                    v8::tc_scope!(let tc, scope);
                    let result = f.call(tc, this, &[reason]);
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
            AlgorithmFn::Native(_) | AlgorithmFn::NativeReason(_) => {
                algorithms::resolved_undefined_promise(scope)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// V8 wrapper helpers
// ---------------------------------------------------------------------------

/// Reach into a controller wrapper's boxed state.
pub fn with_controller_state<R>(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
    f: impl FnOnce(&DefaultControllerState) -> R,
) -> Option<R> {
    let raw_v8_field = controller.get_internal_field(scope, 0)?;
    let ext = v8::Local::<v8::External>::try_from(raw_v8_field).ok()?;
    let ptr = ext.value() as *const DefaultControllerState;
    if ptr.is_null() {
        return None;
    }
    // SAFETY: External points at a Box<DefaultControllerState> set during
    // construction; dropped only by the V8 weak finalizer which fires
    // after all callbacks complete.
    let inst = unsafe { &*ptr };
    Some(f(inst))
}

fn stream_obj<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller: v8::Local<v8::Object>,
) -> Option<v8::Local<'s, v8::Object>> {
    let v = slots::read_slot(scope, controller, STREAM_OBJ_SLOT);
    v8::Local::<v8::Object>::try_from(v).ok()
}

// ---------------------------------------------------------------------------
// Class template construction
// ---------------------------------------------------------------------------

fn controller_class_template<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::FunctionTemplate> {
    let ctor_tmpl = v8::FunctionTemplate::new(scope, illegal_constructor_callback);
    let class_name = v8::String::new(scope, "ReadableStreamDefaultController").unwrap();
    ctor_tmpl.set_class_name(class_name);
    ctor_tmpl
        .instance_template(scope)
        .set_internal_field_count(1);

    let proto = ctor_tmpl.prototype_template(scope);

    // desiredSize getter (§3.6.5.1)
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

    install_proto_method(scope, proto, "close", close_method_callback);
    install_proto_method(scope, proto, "enqueue", enqueue_method_callback);
    install_proto_method(scope, proto, "error", error_method_callback);

    let tag_sym = v8::Symbol::get_to_string_tag(scope);
    let tag_value = v8::String::new(scope, "ReadableStreamDefaultController").unwrap();
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
    // Per WHATWG spec, the controller class has no public constructor.
    let msg = v8::String::new(
        scope,
        "ReadableStreamDefaultController: illegal constructor",
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
    if !is_default_controller(scope, this) {
        let msg = v8::String::new(scope, "desiredSize: receiver not a controller").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    match readable_stream_default_controller_get_desired_size(scope, this) {
        None => rv.set(v8::null(scope).into()),
        Some(n) => rv.set(v8::Number::new(scope, n).into()),
    }
}

fn close_method_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let this = args.this();
    if !is_default_controller(scope, this) {
        let msg = v8::String::new(scope, "close: receiver not a controller").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    if !readable_stream_default_controller_can_close_or_enqueue(scope, this) {
        let msg = v8::String::new(
            scope,
            "ReadableStreamDefaultController.close: stream not in a closable state",
        )
        .unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    readable_stream_default_controller_close(scope, this);
}

fn enqueue_method_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    _rv: v8::ReturnValue<'s>,
) {
    let this = args.this();
    if !is_default_controller(scope, this) {
        let msg = v8::String::new(scope, "enqueue: receiver not a controller").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    if !readable_stream_default_controller_can_close_or_enqueue(scope, this) {
        let msg = v8::String::new(
            scope,
            "ReadableStreamDefaultController.enqueue: stream not in a state that admits enqueue",
        )
        .unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    let chunk = args.get(0);
    if let Err(exc_g) = readable_stream_default_controller_enqueue(scope, this, chunk) {
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
    if !is_default_controller(scope, this) {
        let msg = v8::String::new(scope, "error: receiver not a controller").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    let reason = args.get(0);
    readable_stream_default_controller_error(scope, this, reason);
}

// ---------------------------------------------------------------------------
// Spec algorithms — §3.10
// ---------------------------------------------------------------------------

/// `ReadableStreamDefaultControllerCanCloseOrEnqueue(controller)` — §3.10.5.
pub fn readable_stream_default_controller_can_close_or_enqueue(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) -> bool {
    let close_requested = with_controller_state(scope, controller, |s| s.close_requested.get())
        .unwrap_or(true);
    if close_requested {
        return false;
    }
    let stream = match stream_obj(scope, controller) {
        Some(s) => s,
        None => return false,
    };
    crate::streams::readable::with_rs_state(scope, stream, |s| s.state.get())
        .map(|st| st == StreamState::Readable)
        .unwrap_or(false)
}

/// `ReadableStreamDefaultControllerGetDesiredSize(controller)` — §3.10.7.
///
/// Returns:
/// - None (JS null) if state is errored
/// - Some(0)        if state is closed
/// - Some(hwm - queueTotalSize) otherwise
pub fn readable_stream_default_controller_get_desired_size(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) -> Option<f64> {
    let stream = stream_obj(scope, controller)?;
    let state = crate::streams::readable::with_rs_state(scope, stream, |s| s.state.get())?;
    if state == StreamState::Errored {
        return None;
    }
    if state == StreamState::Closed {
        return Some(0.0);
    }
    with_controller_state(scope, controller, |s| s.strategy_hwm - s.queue.total_size())
}

/// `ReadableStreamDefaultControllerHasBackpressure(controller)` — §3.10.8.
///
/// Returns true iff `desiredSize` is `<= 0`. Used internally by
/// `ShouldCallPull`.
pub fn readable_stream_default_controller_has_backpressure(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) -> bool {
    !readable_stream_default_controller_should_call_pull(scope, controller)
}

/// `ReadableStreamDefaultControllerShouldCallPull(controller)` — §3.10.10.
///
/// Returns true iff:
/// 1. `CanCloseOrEnqueue` is true, AND
/// 2. controller.[[started]] is true, AND
/// 3. (the stream is locked AND there are pending read requests) OR
///    desiredSize > 0.
pub fn readable_stream_default_controller_should_call_pull(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) -> bool {
    if !readable_stream_default_controller_can_close_or_enqueue(scope, controller) {
        return false;
    }
    let started = with_controller_state(scope, controller, |s| s.started.get()).unwrap_or(false);
    if !started {
        return false;
    }
    let stream = match stream_obj(scope, controller) {
        Some(s) => s,
        None => return false,
    };
    if algorithms::is_readable_stream_locked(scope, stream)
        && algorithms::readable_stream_get_num_read_requests(scope, stream) > 0
    {
        return true;
    }
    let desired = readable_stream_default_controller_get_desired_size(scope, controller);
    matches!(desired, Some(n) if n > 0.0)
}

/// `ReadableStreamDefaultControllerCallPullIfNeeded(controller)` — §3.10.3.
///
/// Drives backpressure-aware pull invocation. The reentrancy guard
/// uses `pullAgain`: if a pull is already in flight, set `pullAgain=true`
/// and return; the in-flight pull's reaction will re-call this method
/// after clearing the flag.
pub fn readable_stream_default_controller_call_pull_if_needed(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) {
    if !readable_stream_default_controller_should_call_pull(scope, controller) {
        return;
    }
    let pulling = with_controller_state(scope, controller, |s| s.pulling.get()).unwrap_or(false);
    if pulling {
        with_controller_state(scope, controller, |s| s.pull_again.set(true));
        return;
    }
    debug_assert!(!with_controller_state(scope, controller, |s| s.pull_again.get()).unwrap_or(true));
    with_controller_state(scope, controller, |s| s.pulling.set(true));

    // The Js variant of pull_algorithm needs `&` access; we localize the
    // function and re-run via an immediate call. Native variants are not
    // driven in this dispatch.
    let pull_promise = invoke_pull_algorithm(scope, controller);

    // upon_promise — react with cleanup logic.
    let controller_global = v8::Global::new(scope, controller);
    let controller_global2 = controller_global.clone();
    promise_resolve::upon_promise(
        scope,
        pull_promise,
        Some(Box::new(move |scope, _value| {
            let controller = v8::Local::new(scope, &controller_global);
            with_controller_state(scope, controller, |s| {
                s.pulling.set(false);
            });
            let pull_again = with_controller_state(scope, controller, |s| s.pull_again.get())
                .unwrap_or(false);
            if pull_again {
                with_controller_state(scope, controller, |s| s.pull_again.set(false));
                readable_stream_default_controller_call_pull_if_needed(scope, controller);
            }
        })),
        Some(Box::new(move |scope, reason| {
            let controller = v8::Local::new(scope, &controller_global2);
            readable_stream_default_controller_error(scope, controller, reason);
        })),
    );
}

fn invoke_pull_algorithm<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller: v8::Local<v8::Object>,
) -> v8::Local<'s, v8::Promise> {
    // We need to read AlgorithmFn::Js's function/this_obj globals and invoke
    // it without holding the borrow across the JS call. Pattern: take a
    // snapshot of the globals (clone-cheap), drop the borrow, then invoke.
    let snapshot = with_controller_state(scope, controller, |s| algorithm_snapshot(&s.pull_algorithm))
        .flatten();
    let Some(snap) = snapshot else {
        return algorithms::resolved_undefined_promise(scope);
    };
    snap.invoke_with_controller(scope, controller)
}

/// Cheap snapshot of an AlgorithmFn so we can borrow-cell-out the
/// state lock before invoking the function (which calls into JS,
/// potentially re-entering controller methods).
enum AlgorithmSnapshot {
    Noop,
    Js {
        function: v8::Global<v8::Function>,
        this_obj: v8::Global<v8::Value>,
    },
}

impl AlgorithmSnapshot {
    fn invoke_with_controller<'s>(
        self,
        scope: &mut v8::PinScope<'s, '_>,
        controller_obj: v8::Local<v8::Object>,
    ) -> v8::Local<'s, v8::Promise> {
        let af = match self {
            AlgorithmSnapshot::Noop => AlgorithmFn::Noop,
            AlgorithmSnapshot::Js { function, this_obj } => {
                AlgorithmFn::Js { function, this_obj }
            }
        };
        af.invoke_with_controller(scope, controller_obj)
    }

    fn invoke_with_reason<'s>(
        self,
        scope: &mut v8::PinScope<'s, '_>,
        reason: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Promise> {
        let af = match self {
            AlgorithmSnapshot::Noop => AlgorithmFn::Noop,
            AlgorithmSnapshot::Js { function, this_obj } => {
                AlgorithmFn::Js { function, this_obj }
            }
        };
        af.invoke_with_reason(scope, reason)
    }
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

/// `ReadableStreamDefaultControllerEnqueue(controller, chunk)` — §3.10.6.
pub fn readable_stream_default_controller_enqueue<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller: v8::Local<v8::Object>,
    chunk: v8::Local<'s, v8::Value>,
) -> Result<(), v8::Global<v8::Value>> {
    if !readable_stream_default_controller_can_close_or_enqueue(scope, controller) {
        return Ok(());
    }
    let stream = stream_obj(scope, controller)
        .ok_or_else(|| {
            let msg = v8::String::new(scope, "controller has no stream").unwrap();
            v8::Global::new(scope, v8::Exception::type_error(scope, msg))
        })?;

    // Fast path: stream is locked AND there's a pending read request.
    // Per spec: if stream has default reader and num read requests > 0,
    // call ReadableStreamFulfillReadRequest directly instead of queueing.
    if algorithms::is_readable_stream_locked(scope, stream)
        && algorithms::readable_stream_get_num_read_requests(scope, stream) > 0
    {
        algorithms::readable_stream_fulfill_read_request(scope, stream, chunk, false);
    } else {
        // Compute size via the strategy. On error, ReadableStreamDefaultControllerError
        // and re-throw the exception.
        let size_algo = with_controller_state(scope, controller, |s| size_algo_snapshot(&s.strategy_size))
            .flatten()
            .unwrap_or(SizeAlgoSnapshot::DefaultCount);
        let size_result = size_algo.invoke(scope, chunk);
        let size = match size_result {
            Ok(n) => n,
            Err(exc_g) => {
                let exc = v8::Local::new(scope, &exc_g);
                readable_stream_default_controller_error(scope, controller, exc);
                return Err(exc_g);
            }
        };
        if !is_non_negative_number(size) {
            let msg = v8::String::new(scope, "size returned a non-finite or negative value").unwrap();
            let exc = v8::Exception::range_error(scope, msg);
            let exc_v: v8::Local<v8::Value> = exc.into();
            let exc_g = v8::Global::new(scope, exc_v);
            let exc_l = v8::Local::new(scope, &exc_g);
            readable_stream_default_controller_error(scope, controller, exc_l);
            return Err(exc_g);
        }
        // Enqueue.
        let chunk_g = v8::Global::new(scope, chunk);
        with_controller_state(scope, controller, |s| {
            s.queue.enqueue_value_with_size(chunk_g, size);
        });
    }
    readable_stream_default_controller_call_pull_if_needed(scope, controller);
    Ok(())
}

/// Cheap snapshot of a SizeAlgorithm — same pattern as AlgorithmSnapshot
/// (avoids holding the borrow across user-callback execution).
enum SizeAlgoSnapshot {
    DefaultCount,
    Count,
    ByteLength,
    Js(v8::Global<v8::Function>),
}

impl SizeAlgoSnapshot {
    fn invoke<'s>(
        self,
        scope: &mut v8::PinScope<'s, '_>,
        chunk: v8::Local<'s, v8::Value>,
    ) -> Result<f64, v8::Global<v8::Value>> {
        let sa = match self {
            SizeAlgoSnapshot::DefaultCount => SizeAlgorithm::DefaultCount,
            SizeAlgoSnapshot::Count => SizeAlgorithm::Count,
            SizeAlgoSnapshot::ByteLength => SizeAlgorithm::ByteLength,
            SizeAlgoSnapshot::Js(f) => SizeAlgorithm::Js(f),
        };
        sa.invoke(scope, chunk)
    }
}

fn size_algo_snapshot(sa: &SizeAlgorithm) -> Option<SizeAlgoSnapshot> {
    match sa {
        SizeAlgorithm::DefaultCount => Some(SizeAlgoSnapshot::DefaultCount),
        SizeAlgorithm::Count => Some(SizeAlgoSnapshot::Count),
        SizeAlgorithm::ByteLength => Some(SizeAlgoSnapshot::ByteLength),
        SizeAlgorithm::Js(f) => Some(SizeAlgoSnapshot::Js(f.clone())),
    }
}

/// `ReadableStreamDefaultControllerClose(controller)` — §3.10.4.
pub fn readable_stream_default_controller_close(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) {
    if !readable_stream_default_controller_can_close_or_enqueue(scope, controller) {
        return;
    }
    let stream = match stream_obj(scope, controller) {
        Some(s) => s,
        None => return,
    };
    with_controller_state(scope, controller, |s| s.close_requested.set(true));

    // If the queue is empty, transition the stream to closed immediately.
    let q_empty = with_controller_state(scope, controller, |s| s.queue.is_empty()).unwrap_or(true);
    if q_empty {
        readable_stream_default_controller_clear_algorithms(scope, controller);
        algorithms::readable_stream_close(scope, stream);
    }
    // Otherwise the queue drains via PullSteps; the last DequeueValue
    // triggers the closed transition.
}

/// `ReadableStreamDefaultControllerError(controller, e)` — §3.10.5.
pub fn readable_stream_default_controller_error<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller: v8::Local<v8::Object>,
    error: v8::Local<'s, v8::Value>,
) {
    let stream = match stream_obj(scope, controller) {
        Some(s) => s,
        None => return,
    };
    let st = match crate::streams::readable::with_rs_state(scope, stream, |s| s.state.get()) {
        Some(s) => s,
        None => return,
    };
    if st != StreamState::Readable {
        return;
    }
    with_controller_state(scope, controller, |s| s.queue.reset_queue());
    readable_stream_default_controller_clear_algorithms(scope, controller);
    algorithms::readable_stream_error(scope, stream, error);
}

/// `ReadableStreamDefaultControllerClearAlgorithms(controller)` — §3.10.4.
fn readable_stream_default_controller_clear_algorithms(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) {
    // SAFETY: we need &mut access to the state to drop the algorithms.
    // Use the same External pointer dance as `with_controller_state` but
    // produce a `&mut`.
    let raw = match controller
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
    {
        Some(e) => e.value() as *mut DefaultControllerState,
        None => return,
    };
    if raw.is_null() {
        return;
    }
    let state = unsafe { &mut *raw };
    state.pull_algorithm = AlgorithmFn::Noop;
    state.cancel_algorithm = AlgorithmFn::Noop;
    state.strategy_size = SizeAlgorithm::DefaultCount;
}

// ---------------------------------------------------------------------------
// Internal methods (spec §3.6.6) — [[CancelSteps]], [[PullSteps]], [[ReleaseSteps]]
// ---------------------------------------------------------------------------

/// `[[CancelSteps]](reason)` — §3.6.6.1.
///
/// 1. Reset queue.
/// 2. Let result = cancelAlgorithm(reason).
/// 3. ClearAlgorithms.
/// 4. Return result.
///
/// Called from `ReadableStreamCancel` (algorithms.rs).
pub fn cancel_steps<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
    reason: v8::Local<'s, v8::Value>,
) -> v8::Local<'s, v8::Promise> {
    let controller_v = slots::read_slot(scope, stream, CONTROLLER);
    let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) else {
        return algorithms::resolved_undefined_promise(scope);
    };
    with_controller_state(scope, controller, |s| s.queue.reset_queue());

    // Snapshot cancel_algorithm, clear algorithms, then invoke. Order
    // matters: clear before invoke so reentrant errors don't double-call.
    let snapshot = with_controller_state(scope, controller, |s| algorithm_snapshot(&s.cancel_algorithm))
        .flatten();
    readable_stream_default_controller_clear_algorithms(scope, controller);
    let Some(snap) = snapshot else {
        return algorithms::resolved_undefined_promise(scope);
    };
    snap.invoke_with_reason(scope, reason)
}

/// `[[PullSteps]](readRequest)` — §3.6.6.2.
///
/// 1. If queue is non-empty:
///    a. chunk = DequeueValue(queue).
///    b. If closeRequested AND queue is empty: ClearAlgorithms; close stream.
///    c. Else: CallPullIfNeeded.
///    d. Call readRequest's chunkSteps with chunk.
/// 2. Else:
///    a. Call ReadableStreamAddReadRequest(stream, readRequest).
///    b. CallPullIfNeeded.
pub fn pull_steps(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
    read_request: crate::streams::readable_default_reader::ReadRequest,
) {
    let controller_v = slots::read_slot(scope, stream, CONTROLLER);
    let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) else {
        return;
    };
    let q_non_empty =
        with_controller_state(scope, controller, |s| !s.queue.is_empty()).unwrap_or(false);

    if q_non_empty {
        let entry = with_controller_state(scope, controller, |s| s.queue.dequeue_value()).flatten();
        if let Some(entry) = entry {
            // If close was requested and queue is now empty, close the stream.
            let close_now = with_controller_state(scope, controller, |s| {
                s.close_requested.get() && s.queue.is_empty()
            })
            .unwrap_or(false);
            if close_now {
                readable_stream_default_controller_clear_algorithms(scope, controller);
                algorithms::readable_stream_close(scope, stream);
            } else {
                readable_stream_default_controller_call_pull_if_needed(scope, controller);
            }
            // Fulfill the read request with the chunk.
            let chunk = v8::Local::new(scope, &entry.value);
            crate::streams::readable_default_reader::fulfill_read_request_chunk(
                scope, read_request, chunk,
            );
        }
    } else {
        // Push read request onto reader's queue, then ask for a pull.
        crate::streams::readable_default_reader::enqueue_read_request(scope, stream, read_request);
        readable_stream_default_controller_call_pull_if_needed(scope, controller);
    }
}

/// `[[ReleaseSteps]]()` — §3.6.6.3. No-op for default controllers.
pub fn release_steps(_scope: &mut v8::PinScope, _stream: v8::Local<v8::Object>) {}

// ---------------------------------------------------------------------------
// SetUp* helpers — §3.10.1, §3.10.2
// ---------------------------------------------------------------------------

/// `SetUpReadableStreamDefaultController(stream, controller, startAlgorithm,
///  pullAlgorithm, cancelAlgorithm, hwm, sizeAlgorithm)` — §3.10.1.
fn set_up_readable_stream_default_controller(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
    start_algorithm: AlgorithmFn,
    pull_algorithm: AlgorithmFn,
    cancel_algorithm: AlgorithmFn,
    hwm: f64,
    size_algorithm: SizeAlgorithm,
) -> Result<(), String> {
    // Build the controller wrapper.
    let tmpl = controller_class_template(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let controller_obj = inst_tmpl
        .new_instance(scope)
        .ok_or_else(|| "alloc controller instance".to_string())?;

    // Wire the prototype manually so methods are visible.
    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    controller_obj.set_prototype(scope, proto_v);

    let state = DefaultControllerState::new(hwm, size_algorithm, pull_algorithm, cancel_algorithm);
    let boxed = Box::new(state);
    let raw_ptr = Box::into_raw(boxed);
    let raw_addr = raw_ptr as usize;
    let ext = v8::External::new(scope, raw_ptr as *mut std::ffi::c_void);
    controller_obj.set_internal_field(0, ext.into());
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        controller_obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut DefaultControllerState));
        }),
    );
    std::mem::forget(weak);

    // Wire bidirectional refs:
    //  stream.[[controller]] = controller_obj  (via priv sym CONTROLLER)
    //  controller.streamObj  = stream          (via priv sym STREAM_OBJ_SLOT)
    slots::write_slot(scope, stream, CONTROLLER, controller_obj.into());
    slots::write_slot(scope, controller_obj, STREAM_OBJ_SLOT, stream.into());

    // Run startAlgorithm; on its promise's fulfill set started=true and
    // call CallPullIfNeeded; on rejection error the controller.
    let start_promise = invoke_start_algorithm(scope, controller_obj, start_algorithm);
    let controller_global = v8::Global::new(scope, controller_obj);
    let controller_global2 = controller_global.clone();
    promise_resolve::upon_promise(
        scope,
        start_promise,
        Some(Box::new(move |scope, _v| {
            let controller = v8::Local::new(scope, &controller_global);
            with_controller_state(scope, controller, |s| s.started.set(true));
            readable_stream_default_controller_call_pull_if_needed(scope, controller);
        })),
        Some(Box::new(move |scope, reason| {
            let controller = v8::Local::new(scope, &controller_global2);
            readable_stream_default_controller_error(scope, controller, reason);
        })),
    );
    Ok(())
}

fn invoke_start_algorithm<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller_obj: v8::Local<v8::Object>,
    start_algorithm: AlgorithmFn,
) -> v8::Local<'s, v8::Promise> {
    start_algorithm.invoke_with_controller(scope, controller_obj)
}

/// `SetUpReadableStreamDefaultControllerFromUnderlyingSource(stream,
///  underlyingSource, hwm, sizeAlgorithm)` — §3.10.2.
///
/// Reads the user-supplied dictionary's `start`, `pull`, `cancel`
/// callbacks; defaults missing ones to Noop.
pub fn set_up_readable_stream_default_controller_from_underlying_source(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
    underlying_source: v8::Local<v8::Value>,
    strategy: v8::Local<v8::Value>,
) -> Result<(), String> {
    let (hwm, size_algo) = crate::streams::readable::parse_strategy(scope, strategy, 1.0)
        .map_err(|e| e.message)?;
    set_up_readable_stream_default_controller_from_underlying_source_with_strategy(
        scope,
        stream,
        underlying_source,
        hwm,
        size_algo,
    )
}

/// Same as above but takes already-parsed `hwm` and `size_algo`. Used
/// by the constructor where the spec mandates strategy be converted
/// BEFORE underlyingSource (so a throwing strategy.size getter wins
/// over a throwing underlyingSource.start getter).
pub fn set_up_readable_stream_default_controller_from_underlying_source_with_strategy(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
    underlying_source: v8::Local<v8::Value>,
    hwm: f64,
    size_algo: SizeAlgorithm,
) -> Result<(), String> {

    // Pull out start / pull / cancel callbacks.
    let mut start_alg = AlgorithmFn::Noop;
    let mut pull_alg = AlgorithmFn::Noop;
    let mut cancel_alg = AlgorithmFn::Noop;

    if let Ok(us_obj) = v8::Local::<v8::Object>::try_from(underlying_source) {
        // start
        let start_key = v8::String::new(scope, "start").unwrap();
        let start_v = us_obj
            .get(scope, start_key.into())
            .unwrap_or_else(|| v8::undefined(scope).into());
        if !start_v.is_undefined() {
            let Ok(fn_l) = v8::Local::<v8::Function>::try_from(start_v) else {
                return Err("underlyingSource.start must be a function".to_string());
            };
            start_alg = AlgorithmFn::Js {
                function: v8::Global::new(scope, fn_l),
                this_obj: {
                    let v: v8::Local<v8::Value> = us_obj.into();
                    v8::Global::new(scope, v)
                },
            };
        }
        // pull
        let pull_key = v8::String::new(scope, "pull").unwrap();
        let pull_v = us_obj
            .get(scope, pull_key.into())
            .unwrap_or_else(|| v8::undefined(scope).into());
        if !pull_v.is_undefined() {
            let Ok(fn_l) = v8::Local::<v8::Function>::try_from(pull_v) else {
                return Err("underlyingSource.pull must be a function".to_string());
            };
            pull_alg = AlgorithmFn::Js {
                function: v8::Global::new(scope, fn_l),
                this_obj: {
                    let v: v8::Local<v8::Value> = us_obj.into();
                    v8::Global::new(scope, v)
                },
            };
        }
        // cancel
        let cancel_key = v8::String::new(scope, "cancel").unwrap();
        let cancel_v = us_obj
            .get(scope, cancel_key.into())
            .unwrap_or_else(|| v8::undefined(scope).into());
        if !cancel_v.is_undefined() {
            let Ok(fn_l) = v8::Local::<v8::Function>::try_from(cancel_v) else {
                return Err("underlyingSource.cancel must be a function".to_string());
            };
            cancel_alg = AlgorithmFn::Js {
                function: v8::Global::new(scope, fn_l),
                this_obj: {
                    let v: v8::Local<v8::Value> = us_obj.into();
                    v8::Global::new(scope, v)
                },
            };
        }
    }

    set_up_readable_stream_default_controller(
        scope,
        stream,
        start_alg,
        pull_alg,
        cancel_alg,
        hwm,
        size_algo,
    )
}

/// Native variant — used by `from_native_source`.
pub fn set_up_readable_stream_default_controller_native<S: NativeSource + 'static>(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
    source: S,
    hwm: f64,
) {
    let source_rc = Rc::new(RefCell::new(source));
    let pull = {
        let source_rc = source_rc.clone();
        AlgorithmFn::Native(Box::new(move |controller_obj| {
            let source_rc = source_rc.clone();
            Box::pin(async move {
                let mut controller = NativeReadableController { controller_obj };
                let res = source_rc.borrow_mut().pull(&mut controller).await;
                // Sentinel: NativeSource pull resolves to undefined
                // semantically; the runtime loop ignores the value. Map
                // to an Err sentinel only if the trait method signaled
                // an error. The caller (current dispatch) does not drive
                // this future yet; the type surface exists so the next
                // dispatch can attach a runtime-loop driver without
                // breaking the trait shape.
                match res {
                    Ok(()) => Err(unreachable_sentinel()),
                    Err(e) => Err(e),
                }
            })
        }))
    };
    let cancel = {
        let source_rc = source_rc.clone();
        AlgorithmFn::NativeReason(Box::new(move |reason| {
            let source_rc = source_rc.clone();
            Box::pin(async move { source_rc.borrow_mut().cancel(reason).await })
        }))
    };
    // start: synchronous Noop here. The async start hook ships in a
    // later dispatch alongside the runtime-loop driver for native pulls.
    let _ = set_up_readable_stream_default_controller(
        scope,
        stream,
        AlgorithmFn::Noop,
        pull,
        cancel,
        hwm,
        SizeAlgorithm::DefaultCount,
    );
}

/// Sentinel returned by NativeSource pull's success path. The current
/// dispatch's AlgorithmFn::Native variant doesn't drive the future yet,
/// so this value is never observed by the runtime. The next dispatch
/// will rewire AlgorithmFn::Native's contract to use OpResult::JsValue
/// directly (so this sentinel can go away).
fn unreachable_sentinel() -> v8::Global<v8::Value> {
    // We construct a fresh Global from a dummy isolate pointer is not
    // possible without a scope. Since this code path is unreachable in
    // this dispatch (the runtime loop driver lands next), we panic with
    // a clear message if it ever runs.
    panic!(
        "AlgorithmFn::Native pull driver not wired in this dispatch — see streams-native.md §VII.5"
    )
}

// ---------------------------------------------------------------------------
// is_default_controller — sentinel check for receiver type
// ---------------------------------------------------------------------------

/// True iff `obj` looks like a ReadableStreamDefaultController wrapper.
/// Best-effort: returns true if internal field 0 has a non-null External.
/// In a multi-class context, we'd need a class-tag check; for this dispatch
/// the only class with an internal field 0 is our controller class
/// (modulo the stream/reader, which are also tagged but accessed
/// elsewhere).
pub(crate) fn is_default_controller(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
) -> bool {
    obj.get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
        .map(|e| !e.value().is_null())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Public install
// ---------------------------------------------------------------------------

pub fn install(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
    let tmpl = controller_class_template(scope);
    let class_fn = tmpl.get_function(scope).unwrap();
    let key = v8::String::new(scope, "ReadableStreamDefaultController").unwrap();
    global.set(scope, key.into(), class_fn.into());
}
