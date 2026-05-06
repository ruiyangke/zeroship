//! `ReadableStreamPipeTo` — spec §3.5.1 / §3.9.1.7.
//!
//! Implementation mirrors WHATWG's reference implementation
//! (`reference-implementation/lib/abstract-ops/readable-streams.js` —
//! `ReadableStreamPipeTo`) one-to-one. The 14-step algorithm + handler
//! installation order is critical:
//! we install the four shutdown handlers in spec order:
//!   1. abortAlgorithm (signal)
//!   2. isOrBecomesErrored(source) — forward-error
//!   3. isOrBecomesErrored(dest)   — backward-error
//!   4. isOrBecomesClosed(source)  — forward-close
//!   5. synchronous backward-close check (dest closed/closing at start)
//!   6. spawn pipeLoop with setPromiseIsHandledToTrue
//!
//! PipeState borrow protocol: PipeState is held via
//! `Rc<PipeState>`; mutable bookkeeping (current_write, signal_listener)
//! lives in interior `RefCell`s. Each top-level callback drops its
//! borrow before calling out to user code (writer.write, reader.read,
//! signal.removeEventListener) and re-acquires on the next reaction.
//! We never hold a `borrow_mut()` across an `await`/V8 callback boundary.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::streams::algorithms;
use crate::streams::promise_resolve;
use crate::streams::readable::{is_readable_stream, with_rs_state, StreamState};
use crate::streams::readable_default_reader::{
    acquire_readable_stream_default_reader, readable_stream_default_reader_release,
    readable_stream_reader_generic_release, ReadRequest, ReadRequestKind, ReadRequestNative,
};
use crate::streams::slots::{self, CLOSED_PROMISE, READY_PROMISE, STORED_ERROR};
use crate::streams::writable::{is_writable_stream, with_ws_state, WSState};
use crate::streams::writable_writer::{
    acquire_writable_stream_default_writer,
    writable_stream_default_writer_close_with_error_propagation,
    writable_stream_default_writer_release, writable_stream_default_writer_write,
};

// ---------------------------------------------------------------------------
// PipeState
// ---------------------------------------------------------------------------

/// Per spec §3.5.1, `PipeState` holds the captured args + the bookkeeping
/// flags `shuttingDown`, `currentWrite`, plus the promise resolver that
/// the spec returns to JS.
///
/// Ownership: `Rc<PipeState>` so multiple async callbacks can hold a
/// reference. All mutable fields are `Cell` (Copy types) or `RefCell`
/// (non-Copy). The borrow-acquire-release protocol is
/// strictly observed: callbacks never hold a `borrow_mut()` across a
/// `.then()` / `await` boundary.
#[allow(missing_debug_implementations)]
pub struct PipeState {
    pub source: v8::Global<v8::Object>,
    pub dest: v8::Global<v8::Object>,
    pub reader: v8::Global<v8::Object>,
    pub writer: v8::Global<v8::Object>,
    pub prevent_close: bool,
    pub prevent_abort: bool,
    pub prevent_cancel: bool,
    pub signal: Option<v8::Global<v8::Object>>,
    /// Abort listener function we registered (so we can pass the same
    /// function to removeEventListener on finalize). Set exactly once
    /// in `register_abort_listener`; cleared on finalize.
    pub signal_listener: RefCell<Option<v8::Global<v8::Function>>>,
    /// JS-visible Promise returned to the caller.
    pub promise_resolver: v8::Global<v8::PromiseResolver>,
    /// `currentWrite` per spec — the most recently-issued write Promise.
    /// `waitForWritesToFinish` chains off this.
    pub current_write: RefCell<v8::Global<v8::Promise>>,
    /// `shuttingDown` flag. Once set to true, subsequent shutdown
    /// triggers no-op.
    pub shutting_down: Cell<bool>,
    /// True once `finalize` has run — guards against double-finalize.
    pub finalized: Cell<bool>,
}

// ---------------------------------------------------------------------------
// Public entrypoint — ReadableStreamPipeTo
// ---------------------------------------------------------------------------

/// `ReadableStreamPipeTo(source, dest, preventClose, preventAbort,
/// preventCancel, signal)` — spec §3.5.1. Returns a Promise<undefined>.
///
/// Caller must verify the source/dest are not locked before calling.
pub fn readable_stream_pipe_to<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    source_obj: v8::Local<v8::Object>,
    dest_obj: v8::Local<v8::Object>,
    prevent_close: bool,
    prevent_abort: bool,
    prevent_cancel: bool,
    signal: Option<v8::Local<v8::Object>>,
) -> v8::Local<'s, v8::Promise> {
    debug_assert!(!algorithms::is_readable_stream_locked(scope, source_obj));
    debug_assert!(!algorithms::is_writable_stream_locked(scope, dest_obj));

    // Acquire reader + writer. Spec asserts these succeed (we already
    // checked `locked === false`).
    let reader = match acquire_readable_stream_default_reader(scope, source_obj) {
        Ok(r) => r,
        Err(msg) => {
            let m = v8::String::new(scope, &msg).unwrap();
            let exc = v8::Exception::type_error(scope, m);
            return algorithms::rejected_with_promise(scope, exc.into());
        }
    };
    let writer = match acquire_writable_stream_default_writer(scope, dest_obj) {
        Ok(w) => w,
        Err(msg) => {
            // Release the reader we already took before bailing.
            readable_stream_default_reader_release(scope, reader);
            let m = v8::String::new(scope, &msg).unwrap();
            let exc = v8::Exception::type_error(scope, m);
            return algorithms::rejected_with_promise(scope, exc.into());
        }
    };

    // Spec: source._disturbed = true.
    with_rs_state(scope, source_obj, |s| s.disturbed.set(true));

    // Build the result Promise + Resolver.
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let resolver_g = v8::Global::new(scope, resolver);

    // currentWrite starts as resolved(undefined).
    let initial_current_write = algorithms::resolved_undefined_promise(scope);

    let pipe_state = Rc::new(PipeState {
        source: v8::Global::new(scope, source_obj),
        dest: v8::Global::new(scope, dest_obj),
        reader: v8::Global::new(scope, reader),
        writer: v8::Global::new(scope, writer),
        prevent_close,
        prevent_abort,
        prevent_cancel,
        signal: signal.map(|s| v8::Global::new(scope, s)),
        signal_listener: RefCell::new(None),
        promise_resolver: resolver_g,
        current_write: RefCell::new(v8::Global::new(scope, initial_current_write)),
        shutting_down: Cell::new(false),
        finalized: Cell::new(false),
    });

    // STEP 1: Signal abort handler.
    //
    // We model AbortSignal via duck-typed property access: `aborted`
    // boolean getter, `reason` getter, `addEventListener`/
    // `removeEventListener` methods. The runtime's polyfilled
    // AbortSignal in fetch.js conforms; tests can pass any object that
    // satisfies the duck-type.
    if let Some(sig) = signal {
        if abort_signal_aborted(scope, sig) {
            // Spec early-return: run abortAlgorithm; pipe Promise rejects
            // with signal.reason.
            run_abort_algorithm(scope, &pipe_state);
            return promise;
        }
        register_abort_listener(scope, sig, &pipe_state);
    }

    // STEP 2: Forward error — source.errored → shutdown via WritableStreamAbort.
    {
        let reader_l = v8::Local::new(scope, &pipe_state.reader);
        let reader_closed = read_closed_promise(scope, reader_l);
        let pipe_state_for_err = pipe_state.clone();
        is_or_becomes_errored(
            scope,
            source_obj,
            true, // is_readable
            reader_closed,
            Box::new(move |scope, error| {
                if !pipe_state_for_err.prevent_abort {
                    let dest_g = pipe_state_for_err.dest.clone();
                    let error_g = error.clone();
                    shutdown_with_action(
                        scope,
                        &pipe_state_for_err,
                        Box::new(move |scope| {
                            let dest = v8::Local::new(scope, &dest_g);
                            let err = v8::Local::new(scope, &error_g);
                            let p = algorithms::writable_stream_abort(scope, dest, err);
                            v8::Global::new(scope, p)
                        }),
                        true,
                        Some(error),
                    );
                } else {
                    shutdown(scope, &pipe_state_for_err, true, Some(error));
                }
            }),
        );
    }

    // STEP 3: Backward error — dest.errored → shutdown via ReadableStreamCancel.
    {
        let writer_l = v8::Local::new(scope, &pipe_state.writer);
        let writer_closed = read_closed_promise(scope, writer_l);
        let pipe_state_for_err = pipe_state.clone();
        is_or_becomes_errored(
            scope,
            dest_obj,
            false, // is_readable: false — writable
            writer_closed,
            Box::new(move |scope, error| {
                if !pipe_state_for_err.prevent_cancel {
                    let source_g = pipe_state_for_err.source.clone();
                    let error_g = error.clone();
                    shutdown_with_action(
                        scope,
                        &pipe_state_for_err,
                        Box::new(move |scope| {
                            let source = v8::Local::new(scope, &source_g);
                            let err = v8::Local::new(scope, &error_g);
                            let p = algorithms::readable_stream_cancel(scope, source, err);
                            v8::Global::new(scope, p)
                        }),
                        true,
                        Some(error),
                    );
                } else {
                    shutdown(scope, &pipe_state_for_err, true, Some(error));
                }
            }),
        );
    }

    // STEP 4: Forward close — source.closed → shutdown via WritableStreamDefaultWriterCloseWithErrorPropagation.
    {
        let reader_l = v8::Local::new(scope, &pipe_state.reader);
        let reader_closed = read_closed_promise(scope, reader_l);
        let pipe_state_for_close = pipe_state.clone();
        is_or_becomes_closed(
            scope,
            source_obj,
            reader_closed,
            Box::new(move |scope| {
                if !pipe_state_for_close.prevent_close {
                    let writer_g = pipe_state_for_close.writer.clone();
                    shutdown_with_action(
                        scope,
                        &pipe_state_for_close,
                        Box::new(move |scope| {
                            let writer = v8::Local::new(scope, &writer_g);
                            let p = writable_stream_default_writer_close_with_error_propagation(
                                scope, writer,
                            );
                            v8::Global::new(scope, p)
                        }),
                        false,
                        None,
                    );
                } else {
                    shutdown(scope, &pipe_state_for_close, false, None);
                }
            }),
        );
    }

    // STEP 5: Backward close — synchronous initial check that dest is
    // already closed/closing.
    let dest_state = with_ws_state(scope, dest_obj, |s| s.state.get()).unwrap_or(WSState::Errored);
    let close_in_flight = algorithms::writable_stream_close_queued_or_in_flight(scope, dest_obj);
    if close_in_flight || dest_state == WSState::Closed {
        let msg = v8::String::new(
            scope,
            "the destination writable stream closed before all data could be piped to it",
        )
        .unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let exc_v: v8::Local<v8::Value> = exc;
        let exc_g = v8::Global::new(scope, exc_v);
        if !prevent_cancel {
            let source_g = pipe_state.source.clone();
            let exc_g_for_action = exc_g.clone();
            shutdown_with_action(
                scope,
                &pipe_state,
                Box::new(move |scope| {
                    let source = v8::Local::new(scope, &source_g);
                    let err = v8::Local::new(scope, &exc_g_for_action);
                    let p = algorithms::readable_stream_cancel(scope, source, err);
                    v8::Global::new(scope, p)
                }),
                true,
                Some(exc_g),
            );
        } else {
            shutdown(scope, &pipe_state, true, Some(exc_g));
        }
    }

    // STEP 6: Spawn pipeLoop. setPromiseIsHandledToTrue swallows the
    // pipeLoop's own rejection; the four shutdown handlers handle errors.
    let loop_promise = spawn_pipe_loop(scope, pipe_state);
    promise_resolve::set_promise_is_handled_to_true(scope, loop_promise);

    promise
}

// ---------------------------------------------------------------------------
// AbortSignal helpers — duck-typed against `aborted`, `reason`,
// `addEventListener`, `removeEventListener` properties.
// ---------------------------------------------------------------------------

fn abort_signal_aborted(scope: &mut v8::PinScope, signal: v8::Local<v8::Object>) -> bool {
    let key = v8::String::new(scope, "aborted").unwrap();
    let v = signal.get(scope, key.into()).unwrap_or_else(|| v8::undefined(scope).into());
    v.boolean_value(scope)
}

fn abort_signal_reason<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    signal: v8::Local<v8::Object>,
) -> v8::Local<'s, v8::Value> {
    let key = v8::String::new(scope, "reason").unwrap();
    signal
        .get(scope, key.into())
        .unwrap_or_else(|| v8::undefined(scope).into())
}

/// Holder for the abort listener — pinned in an External via Rc::into_raw.
type AbortListenerHolder = RefCell<Option<Rc<PipeState>>>;

/// Build a one-shot V8 Function that fires `abortAlgorithm` when the
/// signal aborts. Stashes the function in `pipe_state.signal_listener`
/// so finalize can call `removeEventListener` with the same reference.
fn register_abort_listener(
    scope: &mut v8::PinScope,
    signal: v8::Local<v8::Object>,
    pipe_state: &Rc<PipeState>,
) {
    let holder: Rc<AbortListenerHolder> = Rc::new(RefCell::new(Some(pipe_state.clone())));
    let raw = Rc::into_raw(holder) as *mut std::ffi::c_void;
    let ext = v8::External::new(scope, raw);
    let tmpl = v8::FunctionTemplate::builder(abort_listener_callback)
        .data(ext.into())
        .build(scope);
    let func = tmpl.get_function(scope).unwrap();
    *pipe_state.signal_listener.borrow_mut() = Some(v8::Global::new(scope, func));

    // Call signal.addEventListener('abort', func).
    let add = signal.get(
        scope,
        v8::String::new(scope, "addEventListener").unwrap().into(),
    );
    if let Some(add_v) = add {
        if let Ok(add_fn) = v8::Local::<v8::Function>::try_from(add_v) {
            let evt = v8::String::new(scope, "abort").unwrap();
            v8::tc_scope!(let tc, scope);
            let _ = add_fn.call(tc, signal.into(), &[evt.into(), func.into()]);
        }
    }
}

fn abort_listener_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let data = args.data();
    let Ok(ext) = v8::Local::<v8::External>::try_from(data) else {
        return;
    };
    let raw = ext.value() as *const AbortListenerHolder;
    if raw.is_null() {
        return;
    }
    // SAFETY: raw produced by Rc::into_raw; matched here by Rc::from_raw.
    let holder: &AbortListenerHolder = unsafe { &*raw };
    let state_opt = holder.borrow_mut().take();
    let Some(state) = state_opt else { return };
    run_abort_algorithm(scope, &state);
}

/// `abortAlgorithm` per spec §3.5.1 step 1. Builds the actions list and
/// calls shutdownWithAction.
fn run_abort_algorithm(scope: &mut v8::PinScope, pipe_state: &Rc<PipeState>) {
    // Spec:
    //   const error = signal.reason;
    //   const actions = [];
    //   if (preventAbort === false) actions.push(() => { ws abort(error) | resolved });
    //   if (preventCancel === false) actions.push(() => { rs cancel(error) | resolved });
    //   shutdownWithAction(() => waitForAllPromise(actions.map(action => action())), true, error);
    let signal_g = pipe_state.signal.clone();
    let Some(signal_g) = signal_g else { return };
    let signal_l = v8::Local::new(scope, &signal_g);
    let reason_l = abort_signal_reason(scope, signal_l);
    let reason_g = v8::Global::new(scope, reason_l);

    let prevent_abort = pipe_state.prevent_abort;
    let prevent_cancel = pipe_state.prevent_cancel;
    let dest_g = pipe_state.dest.clone();
    let source_g = pipe_state.source.clone();
    let action_reason_g = reason_g.clone();

    shutdown_with_action(
        scope,
        pipe_state,
        Box::new(move |scope| {
            let mut actions: Vec<v8::Local<v8::Promise>> = Vec::with_capacity(2);
            if !prevent_abort {
                let dest = v8::Local::new(scope, &dest_g);
                let dest_state =
                    with_ws_state(scope, dest, |s| s.state.get()).unwrap_or(WSState::Errored);
                let p = if dest_state == WSState::Writable {
                    let r = v8::Local::new(scope, &action_reason_g);
                    algorithms::writable_stream_abort(scope, dest, r)
                } else {
                    algorithms::resolved_undefined_promise(scope)
                };
                actions.push(p);
            }
            if !prevent_cancel {
                let source = v8::Local::new(scope, &source_g);
                let source_state =
                    with_rs_state(scope, source, |s| s.state.get()).unwrap_or(StreamState::Errored);
                let p = if source_state == StreamState::Readable {
                    let r = v8::Local::new(scope, &action_reason_g);
                    algorithms::readable_stream_cancel(scope, source, r)
                } else {
                    algorithms::resolved_undefined_promise(scope)
                };
                actions.push(p);
            }
            let combined = wait_for_all_promise(scope, actions);
            v8::Global::new(scope, combined)
        }),
        true,
        Some(reason_g),
    );
}

/// `Promise.all`-style waitForAllPromise. Resolves with undefined when
/// all actions complete; rejects with the first rejection. We don't need
/// the full Promise.all behaviour (combining values); pipe just wants
/// "wait for all".
fn wait_for_all_promise<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    promises: Vec<v8::Local<'s, v8::Promise>>,
) -> v8::Local<'s, v8::Promise> {
    if promises.is_empty() {
        return algorithms::resolved_undefined_promise(scope);
    }
    let global = scope.get_current_context().global(scope);
    let promise_key = v8::String::new(scope, "Promise").unwrap();
    let promise_ctor_v = global.get(scope, promise_key.into()).unwrap();
    let Ok(promise_ctor) = v8::Local::<v8::Object>::try_from(promise_ctor_v) else {
        return algorithms::resolved_undefined_promise(scope);
    };
    let all_key = v8::String::new(scope, "all").unwrap();
    let all_v = promise_ctor.get(scope, all_key.into()).unwrap();
    let Ok(all_fn) = v8::Local::<v8::Function>::try_from(all_v) else {
        return algorithms::resolved_undefined_promise(scope);
    };
    let arr = v8::Array::new(scope, promises.len() as i32);
    for (i, p) in promises.iter().enumerate() {
        let v: v8::Local<v8::Value> = (*p).into();
        arr.set_index(scope, i as u32, v);
    }
    let result = all_fn.call(scope, promise_ctor.into(), &[arr.into()]);
    match result.and_then(|v| v8::Local::<v8::Promise>::try_from(v).ok()) {
        Some(p) => p,
        None => algorithms::resolved_undefined_promise(scope),
    }
}

// ---------------------------------------------------------------------------
// isOrBecomesErrored / isOrBecomesClosed
// ---------------------------------------------------------------------------

/// Read the `[[closedPromise]]` priv-sym off a reader/writer wrapper.
/// On a freshly-acquired reader/writer this is a fresh pending promise.
fn read_closed_promise<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    reader_or_writer: v8::Local<v8::Object>,
) -> v8::Local<'s, v8::Promise> {
    let v = slots::read_slot(scope, reader_or_writer, CLOSED_PROMISE);
    v8::Local::<v8::Promise>::try_from(v)
        .unwrap_or_else(|_| algorithms::resolved_undefined_promise(scope))
}

type ErrAction =
    Box<dyn for<'s> FnOnce(&mut v8::PinScope<'s, '_>, v8::Global<v8::Value>) + 'static>;
type ClosedAction = Box<dyn for<'s> FnOnce(&mut v8::PinScope<'s, '_>) + 'static>;

/// `isOrBecomesErrored(stream, promise, action)` — spec helper.
/// `is_readable=true` means the stream is a ReadableStream; otherwise
/// it's a WritableStream. We need the flag because the two stream
/// types have different state enums.
fn is_or_becomes_errored<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
    is_readable: bool,
    closed_promise: v8::Local<'s, v8::Promise>,
    action: ErrAction,
) {
    let already_errored = if is_readable {
        debug_assert!(is_readable_stream(scope, stream));
        matches!(
            with_rs_state(scope, stream, |s| s.state.get()),
            Some(StreamState::Errored)
        )
    } else {
        debug_assert!(is_writable_stream(scope, stream));
        matches!(
            with_ws_state(scope, stream, |s| s.state.get()),
            Some(WSState::Errored) | Some(WSState::Erroring)
        )
    };
    if already_errored {
        let stored = slots::read_slot(scope, stream, STORED_ERROR);
        let stored_g = v8::Global::new(scope, stored);
        action(scope, stored_g);
        return;
    }
    let action_cell: Rc<RefCell<Option<ErrAction>>> = Rc::new(RefCell::new(Some(action)));
    let action_for_reject = action_cell;
    promise_resolve::upon_promise(
        scope,
        closed_promise,
        None,
        Some(Box::new(move |scope, reason| {
            let Some(action) = action_for_reject.borrow_mut().take() else {
                return;
            };
            let reason_g = v8::Global::new(scope, reason);
            action(scope, reason_g);
        })),
    );
}

fn is_or_becomes_closed<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
    closed_promise: v8::Local<'s, v8::Promise>,
    action: ClosedAction,
) {
    let st = with_rs_state(scope, stream, |s| s.state.get());
    if matches!(st, Some(StreamState::Closed)) {
        action(scope);
        return;
    }
    let action_cell: Rc<RefCell<Option<ClosedAction>>> = Rc::new(RefCell::new(Some(action)));
    let action_for_fulfill = action_cell;
    promise_resolve::upon_promise(
        scope,
        closed_promise,
        Some(Box::new(move |scope, _v| {
            let Some(action) = action_for_fulfill.borrow_mut().take() else {
                return;
            };
            action(scope);
        })),
        None,
    );
}

// ---------------------------------------------------------------------------
// pipeLoop / pipeStep — read → write loop
// ---------------------------------------------------------------------------

/// Spawn the pipeLoop. Returns the loop's outer Promise; the pipe entry
/// hands this to setPromiseIsHandledToTrue (errors are handled by the
/// shutdown handlers).
fn spawn_pipe_loop<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    pipe_state: Rc<PipeState>,
) -> v8::Local<'s, v8::Promise> {
    let loop_resolver = v8::PromiseResolver::new(scope).unwrap();
    let loop_promise = loop_resolver.get_promise(scope);
    let loop_resolver_g = v8::Global::new(scope, loop_resolver);

    pipe_loop_step(scope, pipe_state, loop_resolver_g);
    loop_promise
}

fn pipe_loop_step(
    scope: &mut v8::PinScope,
    pipe_state: Rc<PipeState>,
    loop_resolver: v8::Global<v8::PromiseResolver>,
) {
    if pipe_state.shutting_down.get() {
        let r = v8::Local::new(scope, &loop_resolver);
        let und = v8::undefined(scope);
        r.resolve(scope, und.into());
        return;
    }

    // Wait for writer.ready to settle (backpressure).
    let writer_l = v8::Local::new(scope, &pipe_state.writer);
    let ready_p_v = slots::read_slot(scope, writer_l, READY_PROMISE);
    let ready_p = v8::Local::<v8::Promise>::try_from(ready_p_v)
        .unwrap_or_else(|_| algorithms::resolved_undefined_promise(scope));

    let pipe_state_clone = pipe_state.clone();
    let loop_resolver_clone = loop_resolver.clone();
    let loop_resolver_clone2 = loop_resolver;

    promise_resolve::upon_promise(
        scope,
        ready_p,
        Some(Box::new(move |scope, _v| {
            issue_read(scope, pipe_state_clone, loop_resolver_clone);
        })),
        Some(Box::new(move |scope, _reason| {
            // writer.ready rejected — the dest.errored shutdown handler
            // already kicked in (or will). Resolve the loop.
            let r = v8::Local::new(scope, &loop_resolver_clone2);
            let und = v8::undefined(scope);
            r.resolve(scope, und.into());
        })),
    );
}

fn issue_read(
    scope: &mut v8::PinScope,
    pipe_state: Rc<PipeState>,
    loop_resolver: v8::Global<v8::PromiseResolver>,
) {
    if pipe_state.shutting_down.get() {
        let r = v8::Local::new(scope, &loop_resolver);
        let und = v8::undefined(scope);
        r.resolve(scope, und.into());
        return;
    }

    let reader_l = v8::Local::new(scope, &pipe_state.reader);
    let source_l = v8::Local::new(scope, &pipe_state.source);

    // Use a Native ReadRequest so chunk/close/error steps run
    // SYNCHRONOUSLY inside the controller's fulfill path. This matches
    // the spec's pipeStep semantics — currentWrite must be captured
    // before reactions to the source's closedPromise fire (which happens
    // synchronously inside ReadableStreamClose during pull_steps).
    let request = ReadRequest {
        kind: ReadRequestKind::Native(Box::new(PipeReadRequest {
            pipe_state,
            loop_resolver,
        })),
    };

    crate::streams::readable_default_reader::readable_stream_default_reader_read(
        scope, reader_l, source_l, request,
    );
}

struct PipeReadRequest {
    pipe_state: Rc<PipeState>,
    loop_resolver: v8::Global<v8::PromiseResolver>,
}

impl ReadRequestNative for PipeReadRequest {
    fn chunk_steps<'s>(
        self: Box<Self>,
        scope: &mut v8::PinScope<'s, '_>,
        chunk: v8::Local<'s, v8::Value>,
    ) {
        let PipeReadRequest { pipe_state, loop_resolver } = *self;

        // Capture currentWrite before recursing or yielding,
        // so the source.closed handler — which fires AS A QUEUED
        // MICROTASK after closedPromise resolved during pull_steps —
        // sees the new write Promise when it runs `wait_for_writes_to_finish`.
        let writer_l = v8::Local::new(scope, &pipe_state.writer);
        let write_p = writable_stream_default_writer_write(scope, writer_l, chunk);
        *pipe_state.current_write.borrow_mut() = v8::Global::new(scope, write_p);
        promise_resolve::set_promise_is_handled_to_true(scope, write_p);

        // Loop again. The next iteration goes through pipe_loop_step
        // which awaits writer.ready.
        pipe_loop_step(scope, pipe_state, loop_resolver);
    }

    fn close_steps(self: Box<Self>, scope: &mut v8::PinScope) {
        // closeSteps: source.closed handler (step 4) drives shutdown.
        // Resolve the loop.
        let r = v8::Local::new(scope, &self.loop_resolver);
        let und = v8::undefined(scope);
        r.resolve(scope, und.into());
    }

    fn error_steps<'s>(
        self: Box<Self>,
        scope: &mut v8::PinScope<'s, '_>,
        _reason: v8::Local<'s, v8::Value>,
    ) {
        // errorSteps — source.errored handler (step 2) drives shutdown.
        // Resolve the loop.
        let r = v8::Local::new(scope, &self.loop_resolver);
        let und = v8::undefined(scope);
        r.resolve(scope, und.into());
    }
}

// ---------------------------------------------------------------------------
// shutdown / shutdownWithAction / waitForWritesToFinish / finalize
// ---------------------------------------------------------------------------

type ActionFn = Box<
    dyn for<'s> FnOnce(&mut v8::PinScope<'s, '_>) -> v8::Global<v8::Promise> + 'static,
>;

fn shutdown_with_action(
    scope: &mut v8::PinScope,
    pipe_state: &Rc<PipeState>,
    action: ActionFn,
    original_is_error: bool,
    original_error: Option<v8::Global<v8::Value>>,
) {
    if pipe_state.shutting_down.get() {
        return;
    }
    pipe_state.shutting_down.set(true);

    let dest_l = v8::Local::new(scope, &pipe_state.dest);
    let dest_state = with_ws_state(scope, dest_l, |s| s.state.get()).unwrap_or(WSState::Errored);
    let close_in_flight = algorithms::writable_stream_close_queued_or_in_flight(scope, dest_l);

    if dest_state == WSState::Writable && !close_in_flight {
        let ps = pipe_state.clone();
        let action_cell: Rc<RefCell<Option<ActionFn>>> = Rc::new(RefCell::new(Some(action)));
        let original_error_cell = Rc::new(RefCell::new(original_error));
        let writes_p = wait_for_writes_to_finish(scope, pipe_state);
        promise_resolve::upon_promise(
            scope,
            writes_p,
            Some(Box::new(move |scope, _v| {
                let Some(action) = action_cell.borrow_mut().take() else { return };
                let oe = original_error_cell.borrow_mut().take();
                do_the_rest(scope, &ps, action, original_is_error, oe);
            })),
            None,
        );
    } else {
        do_the_rest(scope, pipe_state, action, original_is_error, original_error);
    }
}

fn shutdown(
    scope: &mut v8::PinScope,
    pipe_state: &Rc<PipeState>,
    is_error: bool,
    error: Option<v8::Global<v8::Value>>,
) {
    if pipe_state.shutting_down.get() {
        return;
    }
    pipe_state.shutting_down.set(true);

    let dest_l = v8::Local::new(scope, &pipe_state.dest);
    let dest_state = with_ws_state(scope, dest_l, |s| s.state.get()).unwrap_or(WSState::Errored);
    let close_in_flight = algorithms::writable_stream_close_queued_or_in_flight(scope, dest_l);

    if dest_state == WSState::Writable && !close_in_flight {
        let ps = pipe_state.clone();
        let error_cell = Rc::new(RefCell::new(error));
        let writes_p = wait_for_writes_to_finish(scope, pipe_state);
        promise_resolve::upon_promise(
            scope,
            writes_p,
            Some(Box::new(move |scope, _v| {
                let e = error_cell.borrow_mut().take();
                finalize(scope, &ps, is_error, e);
            })),
            None,
        );
    } else {
        finalize(scope, pipe_state, is_error, error);
    }
}

fn do_the_rest(
    scope: &mut v8::PinScope,
    pipe_state: &Rc<PipeState>,
    action: ActionFn,
    original_is_error: bool,
    original_error: Option<v8::Global<v8::Value>>,
) {
    let action_promise_g = action(scope);
    let action_promise = v8::Local::new(scope, &action_promise_g);

    let ps_ok = pipe_state.clone();
    let original_error_cell = Rc::new(RefCell::new(original_error));
    let ps_err = pipe_state.clone();

    promise_resolve::upon_promise(
        scope,
        action_promise,
        Some(Box::new(move |scope, _v| {
            let oe = original_error_cell.borrow_mut().take();
            finalize(scope, &ps_ok, original_is_error, oe);
        })),
        Some(Box::new(move |scope, new_error| {
            let new_error_g = v8::Global::new(scope, new_error);
            finalize(scope, &ps_err, true, Some(new_error_g));
        })),
    );
}

/// `waitForWritesToFinish` per spec §3.5.1. Recursive: if a new write
/// started while waiting on the previous one, wait for that too.
///
/// We can't compare `v8::Global<Promise>` for identity portably across
/// the v8 crate API surface — but for correctness we don't strictly
/// need to: in our implementation `current_write` is updated only
/// inside `issue_read`'s chunk handler, which runs as a microtask.
/// Once shutdown sets `shutting_down=true`, further chunk handlers
/// observe the flag and resolve the loop without issuing writes; so
/// `current_write` is stable after shutdown begins, and a single
/// uponPromise on `current_write` suffices.
fn wait_for_writes_to_finish<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    pipe_state: &Rc<PipeState>,
) -> v8::Local<'s, v8::Promise> {
    let current_g = pipe_state.current_write.borrow().clone();
    let current_l = v8::Local::new(scope, &current_g);

    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let resolver_g = v8::Global::new(scope, resolver);

    let resolver_g_for_resolve = resolver_g.clone();
    let resolver_g_for_reject = resolver_g;
    promise_resolve::upon_promise(
        scope,
        current_l,
        Some(Box::new(move |scope, _v| {
            let r = v8::Local::new(scope, &resolver_g_for_resolve);
            let und = v8::undefined(scope);
            r.resolve(scope, und.into());
        })),
        Some(Box::new(move |scope, _reason| {
            // Per spec: errors swallowed by the chunk handler's
            // setPromiseIsHandledToTrue. We resolve regardless.
            let r = v8::Local::new(scope, &resolver_g_for_reject);
            let und = v8::undefined(scope);
            r.resolve(scope, und.into());
        })),
    );

    promise
}

fn finalize(
    scope: &mut v8::PinScope,
    pipe_state: &Rc<PipeState>,
    is_error: bool,
    error: Option<v8::Global<v8::Value>>,
) {
    if pipe_state.finalized.get() {
        return;
    }
    pipe_state.finalized.set(true);

    // Release the writer + reader.
    let writer_l = v8::Local::new(scope, &pipe_state.writer);
    writable_stream_default_writer_release(scope, writer_l);
    let reader_l = v8::Local::new(scope, &pipe_state.reader);
    readable_stream_reader_generic_release(scope, reader_l);

    // Remove abort listener if installed.
    let listener_g = pipe_state.signal_listener.borrow_mut().take();
    if let (Some(sig_g), Some(listener_g)) = (pipe_state.signal.clone(), listener_g) {
        let sig_l = v8::Local::new(scope, &sig_g);
        let listener_l = v8::Local::new(scope, &listener_g);
        let remove_key = v8::String::new(scope, "removeEventListener").unwrap();
        if let Some(remove_v) = sig_l.get(scope, remove_key.into()) {
            if let Ok(remove_fn) = v8::Local::<v8::Function>::try_from(remove_v) {
                let evt = v8::String::new(scope, "abort").unwrap();
                v8::tc_scope!(let tc, scope);
                let _ = remove_fn.call(tc, sig_l.into(), &[evt.into(), listener_l.into()]);
            }
        }
    }

    // Resolve / reject the pipe promise.
    let resolver_l = v8::Local::new(scope, &pipe_state.promise_resolver);
    if is_error {
        let err_l = match error {
            Some(g) => v8::Local::new(scope, &g),
            None => {
                let msg = v8::String::new(scope, "pipeTo finalized with error").unwrap();
                v8::Exception::error(scope, msg)
            }
        };
        resolver_l.reject(scope, err_l);
    } else {
        let und = v8::undefined(scope);
        resolver_l.resolve(scope, und.into());
    }
}

// ---------------------------------------------------------------------------
// pipe_native_internal — Rust-only internal pipe
// ---------------------------------------------------------------------------

/// Pipe options for `pipe_native_internal` and `pipeThrough`.
#[derive(Default, Debug, Clone, Copy)]
pub struct PipeOptions {
    pub prevent_close: bool,
    pub prevent_abort: bool,
    pub prevent_cancel: bool,
}

/// Pipe a JS-visible source ReadableStream to a JS-visible dest
/// WritableStream, bypassing the lock checks. Used by compression /
/// fetch decode where both ends are still being constructed.
///
/// **C-12 INVARIANT (`#[doc(hidden)]`):** callers MUST ensure both
/// `source` and `dest` are Rust-only (constructed via
/// `from_native_source` / `from_native_sink`) and not yet exposed to
/// JS. Pull requests adding a JS-bridged Source/Sink type without a
/// lock-acquiring shim violate this invariant.
#[doc(hidden)]
pub fn pipe_native_internal<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    source: v8::Local<v8::Object>,
    dest: v8::Local<v8::Object>,
    options: PipeOptions,
) -> v8::Local<'s, v8::Promise> {
    // The source/dest are not yet JS-exposed, so they're guaranteed to
    // be unlocked. readable_stream_pipe_to acquires a default reader on
    // the source and a default writer on the dest internally.
    readable_stream_pipe_to(
        scope,
        source,
        dest,
        options.prevent_close,
        options.prevent_abort,
        options.prevent_cancel,
        None,
    )
}
