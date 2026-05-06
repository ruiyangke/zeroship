//! `WritableStreamDefaultController` — spec §4.3 + §4.7 algorithms.
//!
//! IDL (§4.3):
//! ```webidl
//! [Exposed=*]
//! interface WritableStreamDefaultController {
//!   readonly attribute AbortSignal signal;
//!   undefined error(optional any e);
//! };
//! ```
//!
//! Internal slots (§4.3.5):
//! - `[[abortAlgorithm]]`, `[[closeAlgorithm]]`, `[[writeAlgorithm]]` → Rust enum AlgorithmFn
//! - `[[strategySizeAlgorithm]]`                                       → Rust enum SizeAlgorithm
//! - `[[strategyHWM]]`                                                 → Rust f64
//! - `[[queue]]`, `[[queueTotalSize]]`                                  → Rust ValueQueue (queue.rs)
//! - `[[started]]`                                                      → Rust Cell<bool>
//! - `[[abortController]]`                                              → V8 priv sym `[[abortController]]`
//!   (placeholder until native AbortSignal lands)
//! - `[[stream]]` (back-ref)                                            → V8 priv sym `streamObj`
//!
//! Spec algorithms implemented (per §4.7):
//! - `WritableStreamDefaultControllerWrite`              → `writable_stream_default_controller_write`
//! - `WritableStreamDefaultControllerClose`              → `…close`
//! - `WritableStreamDefaultControllerError`              → `…error`
//! - `WritableStreamDefaultControllerErrorIfNeeded`      → `…error_if_needed`
//! - `WritableStreamDefaultControllerGetBackpressure`    → `…get_backpressure`
//! - `WritableStreamDefaultControllerGetChunkSize`       → `…get_chunk_size`
//! - `WritableStreamDefaultControllerGetDesiredSize`     → `…get_desired_size`
//! - `WritableStreamDefaultControllerAdvanceQueueIfNeeded` → `…advance_queue_if_needed`
//! - `WritableStreamDefaultControllerProcessClose`       → `…process_close`
//! - `WritableStreamDefaultControllerProcessWrite`       → `…process_write`
//! - `WritableStreamDefaultControllerClearAlgorithms`    → `…clear_algorithms`
//! - `SetUpWritableStreamDefaultController`              → `set_up_writable_stream_default_controller`
//! - `SetUpWritableStreamDefaultControllerFromUnderlyingSink` → `…_from_underlying_sink`
//! - Internal methods `[[AbortSteps]]`, `[[ErrorSteps]]` → `abort_steps`, `error_steps`

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use crate::streams::algorithms;
use crate::streams::promise_resolve;
use crate::streams::queue::{is_non_negative_number, ValueQueue, ValueQueueEntry};
use crate::streams::readable_default_controller::{AlgorithmFn, SizeAlgorithm};
use crate::streams::writable::{NativeSink, WSState};

const STREAM_OBJ_SLOT: &str = "[[ws.ctrl.streamObj]]";
const ABORT_CONTROLLER_SLOT: &str = "[[ws.ctrl.abortController]]";
/// Brand priv-sym to distinguish a WS controller from other classes that
/// may share the "External in field 0" shape.
const CTRL_BRAND: &str = "[[ws.ctrl.brand]]";

// ---------------------------------------------------------------------------
// Close sentinel
// ---------------------------------------------------------------------------
//
// Per ref impl: `closeSentinel` is a Symbol pushed onto the queue when
// close() is called. ProcessClose's `PeekQueueValue` returns the sentinel
// to signal "close, not write". In Rust we mark via a dedicated bool on
// the queue entry — simpler than carrying a Symbol global around.

/// One queue entry. `is_close_sentinel = true` means: "this entry triggers
/// the sink's close algorithm; don't pass a chunk to write()".
#[allow(missing_debug_implementations)]
struct WSQueueEntry {
    /// The chunk (or undefined for the close sentinel).
    value: v8::Global<v8::Value>,
    /// Per-entry size. Always 0 for the close sentinel per spec
    /// `EnqueueValueWithSize(controller, closeSentinel, 0)`.
    size: f64,
    /// True iff this entry is the close sentinel.
    is_close_sentinel: bool,
}

// ---------------------------------------------------------------------------
// Controller state
// ---------------------------------------------------------------------------

#[allow(missing_debug_implementations)]
pub struct WSControllerState {
    /// SLOT: [[queue]] + [[queueTotalSize]] — using a private wrapper that
    /// supports the close-sentinel marker.
    queue: RefCell<std::collections::VecDeque<WSQueueEntry>>,
    queue_total_size: Cell<f64>,
    pub strategy_hwm: f64,
    pub strategy_size: SizeAlgorithm,
    pub write_algorithm: AlgorithmFn,
    pub close_algorithm: AlgorithmFn,
    pub abort_algorithm: AlgorithmFn,
    /// SLOT: [[started]]
    pub started: Cell<bool>,
}

impl WSControllerState {
    fn new(
        hwm: f64,
        size: SizeAlgorithm,
        write_algorithm: AlgorithmFn,
        close_algorithm: AlgorithmFn,
        abort_algorithm: AlgorithmFn,
    ) -> Self {
        Self {
            queue: RefCell::new(std::collections::VecDeque::new()),
            queue_total_size: Cell::new(0.0),
            strategy_hwm: hwm,
            strategy_size: size,
            write_algorithm,
            close_algorithm,
            abort_algorithm,
            started: Cell::new(false),
        }
    }

    fn enqueue(&self, value: v8::Global<v8::Value>, size: f64, is_close_sentinel: bool) {
        debug_assert!(size.is_finite() && size >= 0.0);
        self.queue.borrow_mut().push_back(WSQueueEntry {
            value,
            size,
            is_close_sentinel,
        });
        self.queue_total_size.set(self.queue_total_size.get() + size);
    }

    fn dequeue(&self) -> Option<WSQueueEntry> {
        let entry = self.queue.borrow_mut().pop_front()?;
        let new_total = self.queue_total_size.get() - entry.size;
        self.queue_total_size
            .set(if new_total < 0.0 { 0.0 } else { new_total });
        Some(entry)
    }

    fn peek_close_sentinel(&self) -> Option<bool> {
        self.queue.borrow().front().map(|e| e.is_close_sentinel)
    }

    fn queue_is_empty(&self) -> bool {
        self.queue.borrow().is_empty()
    }

    fn reset_queue(&self) {
        self.queue.borrow_mut().clear();
        self.queue_total_size.set(0.0);
    }

    fn queue_total_size(&self) -> f64 {
        self.queue_total_size.get()
    }
}

// ---------------------------------------------------------------------------
// V8 wrapper helpers
// ---------------------------------------------------------------------------

pub fn is_ws_default_controller(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
) -> bool {
    let tag = crate::streams::slots::private_sym(scope, CTRL_BRAND);
    obj.has_private(scope, tag).unwrap_or(false)
}

pub fn with_controller_state<R>(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
    f: impl FnOnce(&WSControllerState) -> R,
) -> Option<R> {
    if !is_ws_default_controller(scope, controller) {
        return None;
    }
    let raw_v8_field = controller.get_internal_field(scope, 0)?;
    let ext = v8::Local::<v8::External>::try_from(raw_v8_field).ok()?;
    let ptr = ext.value() as *const WSControllerState;
    if ptr.is_null() {
        return None;
    }
    let inst = unsafe { &*ptr };
    Some(f(inst))
}

pub fn stream_obj<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller: v8::Local<v8::Object>,
) -> Option<v8::Local<'s, v8::Object>> {
    let v = crate::streams::slots::read_slot(scope, controller, STREAM_OBJ_SLOT);
    v8::Local::<v8::Object>::try_from(v).ok()
}

pub(crate) fn signal_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller: v8::Local<v8::Object>,
) -> v8::Local<'s, v8::Value> {
    crate::streams::slots::read_slot(scope, controller, ABORT_CONTROLLER_SLOT)
}

// ---------------------------------------------------------------------------
// Class template
// ---------------------------------------------------------------------------

fn controller_class_template<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::FunctionTemplate> {
    let ctor_tmpl = v8::FunctionTemplate::new(scope, illegal_constructor_callback);
    let class_name = v8::String::new(scope, "WritableStreamDefaultController").unwrap();
    ctor_tmpl.set_class_name(class_name);
    ctor_tmpl
        .instance_template(scope)
        .set_internal_field_count(1);

    let proto = ctor_tmpl.prototype_template(scope);

    // signal getter (§4.3.5.1)
    {
        let key = v8::String::new(scope, "signal").unwrap();
        let getter_tmpl = v8::FunctionTemplate::new(scope, signal_getter_callback);
        proto.set_accessor_property(
            key.into(),
            Some(getter_tmpl.into()),
            None,
            v8::PropertyAttribute::NONE,
        );
    }

    install_proto_method(scope, proto, "error", error_method_callback);

    let tag_sym = v8::Symbol::get_to_string_tag(scope);
    let tag_value = v8::String::new(scope, "WritableStreamDefaultController").unwrap();
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
        "WritableStreamDefaultController: illegal constructor",
    )
    .unwrap();
    let exc = v8::Exception::type_error(scope, msg);
    scope.throw_exception(exc);
}

// ---------------------------------------------------------------------------
// IDL methods
// ---------------------------------------------------------------------------

fn signal_getter_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    if !is_ws_default_controller(scope, this) {
        let msg = v8::String::new(scope, "signal: receiver not a WritableStreamDefaultController").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    // Per spec §4.3.5.1: returns this.[[abortController]].signal.
    //
    // KNOWN GAP: native AbortSignal/AbortController is not yet built (it's
    // part of the fetch design landing later). For now we return the
    // placeholder stored at construction time (an inert object). When
    // AbortSignal lands, the wiring in `set_up_writable_stream_default_controller`
    // becomes real. Tests that depend on the .signal IDL surface continue
    // to pass; tests that depend on .signal.aborted / event firing are
    // skipped in the WPT runner with a clear reason.
    let v = signal_value(scope, this);
    rv.set(v);
}

fn error_method_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    _rv: v8::ReturnValue<'s>,
) {
    let this = args.this();
    if !is_ws_default_controller(scope, this) {
        let msg = v8::String::new(scope, "error: receiver not a WritableStreamDefaultController").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    let stream = match stream_obj(scope, this) {
        Some(s) => s,
        None => return,
    };
    // Per ref impl: error() is a no-op unless state == "writable".
    let st = match crate::streams::writable::with_ws_state(scope, stream, |s| s.state.get()) {
        Some(s) => s,
        None => return,
    };
    if st != WSState::Writable {
        return;
    }
    let e = args.get(0);
    writable_stream_default_controller_error(scope, this, e);
}

// ---------------------------------------------------------------------------
// Spec algorithms — §4.7
// ---------------------------------------------------------------------------

/// `WritableStreamDefaultControllerGetDesiredSize(controller)` — §4.7.10.
///
/// Returns: hwm - queueTotalSize.
pub fn writable_stream_default_controller_get_desired_size(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) -> Option<f64> {
    with_controller_state(scope, controller, |s| s.strategy_hwm - s.queue_total_size())
}

/// `WritableStreamDefaultControllerGetBackpressure(controller)` — §4.7.7.
///
/// Returns desiredSize <= 0.
pub fn writable_stream_default_controller_get_backpressure(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) -> bool {
    let ds = writable_stream_default_controller_get_desired_size(scope, controller).unwrap_or(0.0);
    ds <= 0.0
}

/// `WritableStreamDefaultControllerGetChunkSize(controller, chunk)` — §4.7.8.
///
/// Returns the chunk's size per the strategy's `size` algorithm. If
/// `size()` throws, calls ErrorIfNeeded(e) and returns 1 (fallback).
pub fn writable_stream_default_controller_get_chunk_size<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller: v8::Local<v8::Object>,
    chunk: v8::Local<'s, v8::Value>,
) -> f64 {
    let snapshot = with_controller_state(scope, controller, |s| size_algo_snapshot(&s.strategy_size))
        .flatten();
    let Some(snap) = snapshot else {
        return 1.0;
    };
    match snap.invoke(scope, chunk) {
        Ok(n) => n,
        Err(exc_g) => {
            let exc = v8::Local::new(scope, &exc_g);
            writable_stream_default_controller_error_if_needed(scope, controller, exc);
            1.0
        }
    }
}

/// `WritableStreamDefaultControllerWrite(controller, chunk, chunkSize)` — §4.7.13.
pub fn writable_stream_default_controller_write<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller: v8::Local<v8::Object>,
    chunk: v8::Local<'s, v8::Value>,
    chunk_size: f64,
) {
    if !is_non_negative_number(chunk_size) || !chunk_size.is_finite() {
        // EnqueueValueWithSize would throw RangeError; per spec the caller
        // is `Write` which catches and calls ErrorIfNeeded.
        let msg = v8::String::new(scope, "size returned a non-finite or negative value").unwrap();
        let exc = v8::Exception::range_error(scope, msg);
        let exc_v: v8::Local<v8::Value> = exc.into();
        writable_stream_default_controller_error_if_needed(scope, controller, exc_v);
        return;
    }
    let chunk_g = v8::Global::new(scope, chunk);
    with_controller_state(scope, controller, |s| {
        s.enqueue(chunk_g, chunk_size, false);
    });

    let stream = match stream_obj(scope, controller) {
        Some(s) => s,
        None => return,
    };
    if !algorithms::writable_stream_close_queued_or_in_flight(scope, stream)
        && crate::streams::writable::with_ws_state(scope, stream, |s| s.state.get())
            .map(|st| st == WSState::Writable)
            .unwrap_or(false)
    {
        let bp = writable_stream_default_controller_get_backpressure(scope, controller);
        algorithms::writable_stream_update_backpressure(scope, stream, bp);
    }

    writable_stream_default_controller_advance_queue_if_needed(scope, controller);
}

/// `WritableStreamDefaultControllerClose(controller)` — §4.7.4.
pub fn writable_stream_default_controller_close(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) {
    let undef: v8::Local<v8::Value> = v8::undefined(scope).into();
    let undef_g = v8::Global::new(scope, undef);
    with_controller_state(scope, controller, |s| {
        s.enqueue(undef_g, 0.0, true);
    });
    writable_stream_default_controller_advance_queue_if_needed(scope, controller);
}

/// `WritableStreamDefaultControllerError(controller, e)` — §4.7.5.
pub fn writable_stream_default_controller_error<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller: v8::Local<v8::Object>,
    error: v8::Local<'s, v8::Value>,
) {
    // Per ref impl: assert(stream._state === 'writable'); ClearAlgorithms;
    // StartErroring(stream, error).
    writable_stream_default_controller_clear_algorithms(scope, controller);
    let stream = match stream_obj(scope, controller) {
        Some(s) => s,
        None => return,
    };
    algorithms::writable_stream_start_erroring(scope, stream, error);
}

/// `WritableStreamDefaultControllerErrorIfNeeded(controller, error)` — §4.7.6.
pub fn writable_stream_default_controller_error_if_needed<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller: v8::Local<v8::Object>,
    error: v8::Local<'s, v8::Value>,
) {
    let stream = match stream_obj(scope, controller) {
        Some(s) => s,
        None => return,
    };
    let st = match crate::streams::writable::with_ws_state(scope, stream, |s| s.state.get()) {
        Some(s) => s,
        None => return,
    };
    if st == WSState::Writable {
        writable_stream_default_controller_error(scope, controller, error);
    }
}

/// `WritableStreamDefaultControllerClearAlgorithms(controller)` — §4.7.3.
pub fn writable_stream_default_controller_clear_algorithms(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) {
    let raw = match controller
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
    {
        Some(e) => e.value() as *mut WSControllerState,
        None => return,
    };
    if raw.is_null() {
        return;
    }
    let state = unsafe { &mut *raw };
    state.write_algorithm = AlgorithmFn::Noop;
    state.close_algorithm = AlgorithmFn::Noop;
    state.abort_algorithm = AlgorithmFn::Noop;
    state.strategy_size = SizeAlgorithm::DefaultCount;
}

/// `WritableStreamDefaultControllerAdvanceQueueIfNeeded(controller)` — §4.7.2.
pub fn writable_stream_default_controller_advance_queue_if_needed(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) {
    let started = with_controller_state(scope, controller, |s| s.started.get()).unwrap_or(false);
    if !started {
        return;
    }
    let stream = match stream_obj(scope, controller) {
        Some(s) => s,
        None => return,
    };
    // If [[inFlightWriteRequest]] is not undefined, return.
    let in_flight = crate::streams::writable::with_ws_state(scope, stream, |s| {
        s.in_flight_write_request.borrow().is_some()
    })
    .unwrap_or(false);
    if in_flight {
        return;
    }
    let st = match crate::streams::writable::with_ws_state(scope, stream, |s| s.state.get()) {
        Some(s) => s,
        None => return,
    };
    debug_assert!(st != WSState::Closed && st != WSState::Errored);
    if st == WSState::Erroring {
        algorithms::writable_stream_finish_erroring(scope, stream);
        return;
    }
    let q_empty = with_controller_state(scope, controller, |s| s.queue_is_empty()).unwrap_or(true);
    if q_empty {
        return;
    }
    let is_close = with_controller_state(scope, controller, |s| {
        s.peek_close_sentinel().unwrap_or(false)
    })
    .unwrap_or(false);
    if is_close {
        writable_stream_default_controller_process_close(scope, controller);
    } else {
        writable_stream_default_controller_process_write(scope, controller);
    }
}

/// `WritableStreamDefaultControllerProcessClose(controller)` — §4.7.11.
pub fn writable_stream_default_controller_process_close(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) {
    let stream = match stream_obj(scope, controller) {
        Some(s) => s,
        None => return,
    };
    algorithms::writable_stream_mark_close_request_in_flight(scope, stream);
    // DequeueValue (close sentinel).
    with_controller_state(scope, controller, |s| {
        s.dequeue();
    });
    debug_assert!(with_controller_state(scope, controller, |s| s.queue_is_empty()).unwrap_or(true));

    let close_promise = invoke_close_algorithm(scope, controller);
    writable_stream_default_controller_clear_algorithms(scope, controller);
    let stream_g = v8::Global::new(scope, stream);
    let stream_g2 = stream_g.clone();
    promise_resolve::upon_promise(
        scope,
        close_promise,
        Some(Box::new(move |scope, _v| {
            let stream = v8::Local::new(scope, &stream_g);
            algorithms::writable_stream_finish_in_flight_close(scope, stream);
        })),
        Some(Box::new(move |scope, reason| {
            let stream = v8::Local::new(scope, &stream_g2);
            algorithms::writable_stream_finish_in_flight_close_with_error(scope, stream, reason);
        })),
    );
}

/// `WritableStreamDefaultControllerProcessWrite(controller, chunk)` — §4.7.12.
pub fn writable_stream_default_controller_process_write(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) {
    let stream = match stream_obj(scope, controller) {
        Some(s) => s,
        None => return,
    };

    algorithms::writable_stream_mark_first_write_request_in_flight(scope, stream);

    // Peek the head chunk (need to invoke writeAlgorithm with it).
    let chunk_g = with_controller_state(scope, controller, |s| {
        s.queue.borrow().front().map(|e| e.value.clone())
    })
    .flatten();
    let Some(chunk_g) = chunk_g else {
        return;
    };
    let chunk_l = v8::Local::new(scope, &chunk_g);

    let write_promise = invoke_write_algorithm(scope, controller, chunk_l);

    let stream_g = v8::Global::new(scope, stream);
    let controller_g = v8::Global::new(scope, controller);
    let stream_g2 = stream_g.clone();
    let controller_g2 = controller_g.clone();

    promise_resolve::upon_promise(
        scope,
        write_promise,
        Some(Box::new(move |scope, _v| {
            let stream = v8::Local::new(scope, &stream_g);
            let controller = v8::Local::new(scope, &controller_g);
            algorithms::writable_stream_finish_in_flight_write(scope, stream);
            let st = crate::streams::writable::with_ws_state(scope, stream, |s| s.state.get());
            debug_assert!(matches!(st, Some(WSState::Writable) | Some(WSState::Erroring)));
            // DequeueValue.
            with_controller_state(scope, controller, |s| {
                s.dequeue();
            });
            if !algorithms::writable_stream_close_queued_or_in_flight(scope, stream)
                && st == Some(WSState::Writable)
            {
                let bp = writable_stream_default_controller_get_backpressure(scope, controller);
                algorithms::writable_stream_update_backpressure(scope, stream, bp);
            }
            writable_stream_default_controller_advance_queue_if_needed(scope, controller);
        })),
        Some(Box::new(move |scope, reason| {
            let stream = v8::Local::new(scope, &stream_g2);
            let controller = v8::Local::new(scope, &controller_g2);
            let st = crate::streams::writable::with_ws_state(scope, stream, |s| s.state.get());
            if st == Some(WSState::Writable) {
                writable_stream_default_controller_clear_algorithms(scope, controller);
            }
            algorithms::writable_stream_finish_in_flight_write_with_error(scope, stream, reason);
        })),
    );
}

// ---------------------------------------------------------------------------
// Internal methods (§4.7.1) — [[AbortSteps]], [[ErrorSteps]]
// ---------------------------------------------------------------------------

/// `[[AbortSteps]](reason)` — §4.7.1.1.
///
/// 1. Let result = abortAlgorithm(reason).
/// 2. ClearAlgorithms.
/// 3. Return result.
pub fn abort_steps<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
    reason: v8::Local<'s, v8::Value>,
) -> v8::Local<'s, v8::Promise> {
    let controller_v = crate::streams::slots::read_slot(scope, stream, crate::streams::slots::CONTROLLER);
    let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) else {
        return algorithms::resolved_undefined_promise(scope);
    };
    let snap = with_controller_state(scope, controller, |s| algorithm_snapshot(&s.abort_algorithm))
        .flatten();
    writable_stream_default_controller_clear_algorithms(scope, controller);
    let Some(snap) = snap else {
        return algorithms::resolved_undefined_promise(scope);
    };
    snap.invoke_with_reason(scope, reason)
}

/// `[[ErrorSteps]]()` — §4.7.1.2. ResetQueue(controller).
pub fn error_steps(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) {
    let controller_v = crate::streams::slots::read_slot(scope, stream, crate::streams::slots::CONTROLLER);
    let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) else {
        return;
    };
    with_controller_state(scope, controller, |s| s.reset_queue());
}

// ---------------------------------------------------------------------------
// Algorithm snapshot — same pattern as readable_default_controller
// ---------------------------------------------------------------------------

enum AlgorithmSnapshot {
    Noop,
    Js {
        function: v8::Global<v8::Function>,
        this_obj: v8::Global<v8::Value>,
    },
    /// Native one-shot: reserved for the next dispatch when AlgorithmFn::Native
    /// becomes drivable. Closure shape preserved for type-checking; actual
    /// invocation defers to AlgorithmSnapshot::Noop until the runtime-loop
    /// driver lands (§VII.5).
    #[allow(dead_code)]
    Native(
        Rc<RefCell<Option<Box<dyn FnOnce(NativeWritableArg, v8::Global<v8::Object>) -> Pin<Box<dyn Future<Output = Result<(), v8::Global<v8::Value>>>>>>>>>,
    ),
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
            AlgorithmSnapshot::Native(_) => algorithms::resolved_undefined_promise(scope),
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

    fn invoke_zero_args<'s>(
        self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> v8::Local<'s, v8::Promise> {
        match self {
            AlgorithmSnapshot::Noop => algorithms::resolved_undefined_promise(scope),
            AlgorithmSnapshot::Js { function, this_obj } => {
                let f = v8::Local::new(scope, &function);
                let this = v8::Local::new(scope, &this_obj);
                invoke_js(scope, f, this, &[])
            }
            AlgorithmSnapshot::Native(_) => algorithms::resolved_undefined_promise(scope),
        }
    }
}

#[doc(hidden)]
pub enum NativeWritableArg {
    Empty,
    Chunk(v8::Global<v8::Value>),
    Reason(Option<v8::Global<v8::Value>>),
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

fn invoke_write_algorithm<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller: v8::Local<v8::Object>,
    chunk: v8::Local<'s, v8::Value>,
) -> v8::Local<'s, v8::Promise> {
    let snap = with_controller_state(scope, controller, |s| algorithm_snapshot(&s.write_algorithm))
        .flatten();
    let Some(snap) = snap else {
        return algorithms::resolved_undefined_promise(scope);
    };
    snap.invoke_with_chunk(scope, chunk, controller)
}

fn invoke_close_algorithm<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller: v8::Local<v8::Object>,
) -> v8::Local<'s, v8::Promise> {
    let snap = with_controller_state(scope, controller, |s| algorithm_snapshot(&s.close_algorithm))
        .flatten();
    let Some(snap) = snap else {
        return algorithms::resolved_undefined_promise(scope);
    };
    snap.invoke_zero_args(scope)
}

fn invoke_start_algorithm<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller: v8::Local<v8::Object>,
    start_algorithm: AlgorithmFn,
) -> v8::Local<'s, v8::Promise> {
    // start algorithm produced from underlyingSink.start; one-shot.
    let snap = match start_algorithm {
        AlgorithmFn::Noop => AlgorithmSnapshot::Noop,
        AlgorithmFn::Js { function, this_obj } => AlgorithmSnapshot::Js { function, this_obj },
        AlgorithmFn::Native(_) | AlgorithmFn::NativeReason(_) => AlgorithmSnapshot::Noop,
    };
    snap.invoke_with_controller(scope, controller)
}

// ---------------------------------------------------------------------------
// Setup — §4.7.14, §4.7.15
// ---------------------------------------------------------------------------

/// `SetUpWritableStreamDefaultController(stream, controller, startAlg,
///  writeAlg, closeAlg, abortAlg, hwm, sizeAlg)` — §4.7.14.
fn set_up_writable_stream_default_controller(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
    start_algorithm: AlgorithmFn,
    write_algorithm: AlgorithmFn,
    close_algorithm: AlgorithmFn,
    abort_algorithm: AlgorithmFn,
    hwm: f64,
    size_algorithm: SizeAlgorithm,
) -> Result<(), String> {
    // Build the controller wrapper.
    let tmpl = controller_class_template(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let controller_obj = inst_tmpl
        .new_instance(scope)
        .ok_or_else(|| "alloc controller instance".to_string())?;

    // Wire prototype.
    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    controller_obj.set_prototype(scope, proto_v);

    let state = WSControllerState::new(
        hwm,
        size_algorithm,
        write_algorithm,
        close_algorithm,
        abort_algorithm,
    );
    let boxed = Box::new(state);
    let raw_ptr = Box::into_raw(boxed);
    let raw_addr = raw_ptr as usize;
    let ext = v8::External::new(scope, raw_ptr as *mut std::ffi::c_void);
    controller_obj.set_internal_field(0, ext.into());

    // Brand priv-sym.
    let brand = crate::streams::slots::private_sym(scope, CTRL_BRAND);
    let true_v: v8::Local<v8::Value> = v8::Boolean::new(scope, true).into();
    controller_obj.set_private(scope, brand, true_v);

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        controller_obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut WSControllerState));
        }),
    );
    std::mem::forget(weak);

    // Wire bidirectional refs.
    crate::streams::slots::write_slot(
        scope,
        stream,
        crate::streams::slots::CONTROLLER,
        controller_obj.into(),
    );
    crate::streams::slots::write_slot(scope, controller_obj, STREAM_OBJ_SLOT, stream.into());

    // KNOWN GAP: native AbortSignal/AbortController not yet built (it's
    // part of the fetch design landing later). We store an inert
    // placeholder Object on the controller so the .signal getter has
    // something to return. When AbortSignal lands, replace with
    // `new AbortController()` and wire abort.
    let signal_placeholder = v8::Object::new(scope);
    crate::streams::slots::write_slot(
        scope,
        controller_obj,
        ABORT_CONTROLLER_SLOT,
        signal_placeholder.into(),
    );

    // Compute initial backpressure and propagate via UpdateBackpressure.
    // Per ref impl — set BEFORE invoking startAlgorithm so writer's
    // ready promise is correctly initialized when user creates a writer
    // synchronously after construction.
    let initial_bp = writable_stream_default_controller_get_backpressure(scope, controller_obj);
    algorithms::writable_stream_update_backpressure(scope, stream, initial_bp);

    // Run startAlgorithm; on its promise's fulfill set started=true and
    // call AdvanceQueueIfNeeded; on rejection deal with rejection.
    let start_promise = invoke_start_algorithm(scope, controller_obj, start_algorithm);
    let stream_g = v8::Global::new(scope, stream);
    let stream_g2 = stream_g.clone();
    let controller_g = v8::Global::new(scope, controller_obj);
    let controller_g2 = controller_g.clone();

    promise_resolve::upon_promise(
        scope,
        start_promise,
        Some(Box::new(move |scope, _v| {
            let stream = v8::Local::new(scope, &stream_g);
            let controller = v8::Local::new(scope, &controller_g);
            let st = crate::streams::writable::with_ws_state(scope, stream, |s| s.state.get());
            debug_assert!(matches!(st, Some(WSState::Writable) | Some(WSState::Erroring)));
            with_controller_state(scope, controller, |s| s.started.set(true));
            writable_stream_default_controller_advance_queue_if_needed(scope, controller);
        })),
        Some(Box::new(move |scope, reason| {
            let stream = v8::Local::new(scope, &stream_g2);
            let controller = v8::Local::new(scope, &controller_g2);
            let st = crate::streams::writable::with_ws_state(scope, stream, |s| s.state.get());
            debug_assert!(matches!(st, Some(WSState::Writable) | Some(WSState::Erroring)));
            with_controller_state(scope, controller, |s| s.started.set(true));
            algorithms::writable_stream_deal_with_rejection(scope, stream, reason);
        })),
    );
    Ok(())
}

/// `SetUpWritableStreamDefaultControllerFromUnderlyingSink(stream,
///  underlyingSink, underlyingSinkDict, hwm, sizeAlg)` — §4.7.15.
pub fn set_up_writable_stream_default_controller_from_underlying_sink_with_strategy(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
    underlying_sink: v8::Local<v8::Value>,
    hwm: f64,
    size_algo: SizeAlgorithm,
) -> Result<(), String> {
    let mut start_alg = AlgorithmFn::Noop;
    let mut write_alg = AlgorithmFn::Noop;
    let mut close_alg = AlgorithmFn::Noop;
    let mut abort_alg = AlgorithmFn::Noop;

    if let Ok(us_obj) = v8::Local::<v8::Object>::try_from(underlying_sink) {
        for (key_name, slot) in [
            ("start", &mut start_alg as *mut _),
            ("write", &mut write_alg as *mut _),
            ("close", &mut close_alg as *mut _),
            ("abort", &mut abort_alg as *mut _),
        ] {
            let key = v8::String::new(scope, key_name).unwrap();
            let v = us_obj
                .get(scope, key.into())
                .unwrap_or_else(|| v8::undefined(scope).into());
            if !v.is_undefined() {
                let Ok(fn_l) = v8::Local::<v8::Function>::try_from(v) else {
                    return Err(format!("underlyingSink.{key_name} must be a function"));
                };
                // SAFETY: `slot` points at a stack-local variable and is
                // valid for the duration of this loop iteration only.
                unsafe {
                    *slot = AlgorithmFn::Js {
                        function: v8::Global::new(scope, fn_l),
                        this_obj: {
                            let v: v8::Local<v8::Value> = us_obj.into();
                            v8::Global::new(scope, v)
                        },
                    };
                }
            }
        }
    }

    set_up_writable_stream_default_controller(
        scope,
        stream,
        start_alg,
        write_alg,
        close_alg,
        abort_alg,
        hwm,
        size_algo,
    )
}

/// Native variant — used by `from_native_sink`.
pub fn set_up_writable_stream_default_controller_native<S: NativeSink + 'static>(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
    sink: S,
    hwm: f64,
) {
    let _sink_rc = Rc::new(RefCell::new(sink));
    // For this dispatch the native sink's pull/cancel/write futures aren't
    // driven by the runtime loop yet. The trait surface exists so the next dispatch
    // can attach a runtime-loop driver without churning the API. For now,
    // pull/close/abort are no-ops — same shape as
    // set_up_readable_stream_default_controller_native.
    let _ = set_up_writable_stream_default_controller(
        scope,
        stream,
        AlgorithmFn::Noop,
        AlgorithmFn::Noop,
        AlgorithmFn::Noop,
        AlgorithmFn::Noop,
        hwm,
        SizeAlgorithm::DefaultCount,
    );
    // Suppress unused-type lint when sink trait methods aren't driven.
    let _ = std::mem::size_of_val(&_sink_rc);
}

// ---------------------------------------------------------------------------
// Public install
// ---------------------------------------------------------------------------

pub fn install(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
    let tmpl = controller_class_template(scope);
    let class_fn = tmpl.get_function(scope).unwrap();
    let key = v8::String::new(scope, "WritableStreamDefaultController").unwrap();
    global.set(scope, key.into(), class_fn.into());
}

// ---------------------------------------------------------------------------
// Suppress unused-variable lint for ValueQueueEntry (re-exported as
// part of the queue module API). The WS controller uses its own bespoke
// queue entry shape (with close-sentinel marker) instead.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn _unused_value_queue_entry(_e: &ValueQueueEntry) {}
#[allow(dead_code)]
fn _unused_value_queue(_q: &ValueQueue) {}
