//! Cross-class abstract operations for ReadableStream (spec §3.9.1, §3.9.2)
//! and WritableStream (spec §4.5).
//!
//! Per D-20: spec abstract operations whose name doesn't sit on a single
//! class (e.g. `ReadableStreamCancel`, `WritableStreamAbort`) live here.
//! Class-local operations (`ReadableStreamDefaultControllerEnqueue`) live
//! in their respective class file.
//!
//! Style: each function is named with the spec name in `snake_case`. Spec
//! sections are cited at every function so reviewers can grep for the WPT
//! spec text.

use crate::streams::readable::StreamState;
use crate::streams::readable_default_reader::{ReadRequest, ReadRequestKind};
use crate::streams::slots::{self, READER, STORED_ERROR, WRITER};
use crate::streams::writable::{
    PendingAbortRequest, PromisePair, WSState, WSStreamState,
};

// ---------------------------------------------------------------------------
// IsReadableStreamLocked — §3.9.1.4
// ---------------------------------------------------------------------------

/// `IsReadableStreamLocked(stream)`: spec §3.9.1.4. Returns true iff the
/// stream's `[[reader]]` slot is set.
pub fn is_readable_stream_locked(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) -> bool {
    !slots::slot_is_empty(scope, stream, READER)
}

// ---------------------------------------------------------------------------
// ReadableStreamHasDefaultReader / HasBYOBReader — §3.9.1.13 / §3.9.1.14
// ---------------------------------------------------------------------------

/// `ReadableStreamHasDefaultReader(stream)` — §3.9.1.13. True iff
/// `[[reader]]` is a default reader. The default-reader test reads the
/// reader wrapper's internal field 0 and confirms its boxed state is
/// `DefaultReaderState`. We use a sentinel embed (the wrapper is the only
/// JS class with this internal-field shape) — any reader installed via
/// `ReadableStreamDefaultReader::install` matches.
pub fn readable_stream_has_default_reader(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) -> bool {
    let reader_v = slots::read_slot(scope, stream, READER);
    let Ok(reader_obj) = v8::Local::<v8::Object>::try_from(reader_v) else {
        return false;
    };
    crate::streams::readable_default_reader::is_default_reader(scope, reader_obj)
}

/// `ReadableStreamHasBYOBReader(stream)` — §3.9.1.14. BYOB readers are
/// not implemented in this dispatch; always returns false. (When the byte
/// path lands, this branches on the reader's class.)
pub fn readable_stream_has_byob_reader(
    _scope: &mut v8::PinScope,
    _stream: v8::Local<v8::Object>,
) -> bool {
    false
}

// ---------------------------------------------------------------------------
// ReadableStreamGetNumReadRequests — §3.9.1.15
// ---------------------------------------------------------------------------

/// `ReadableStreamGetNumReadRequests(stream)`. Asserts a default reader
/// is attached; returns its `[[readRequests]]` length. The list is stored
/// on the stream's RSState (per spec §3.10 it sits on the reader, but
/// since reads route through the stream's controller's PullSteps and
/// then back to the reader, the spec's "shift from the reader's queue"
/// is implementable at either end. We keep it on the reader's state, in
/// which the reader is held via the stream's `[[reader]]` slot.)
pub fn readable_stream_get_num_read_requests(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) -> usize {
    let reader_v = slots::read_slot(scope, stream, READER);
    let Ok(reader_obj) = v8::Local::<v8::Object>::try_from(reader_v) else {
        return 0;
    };
    crate::streams::readable_default_reader::with_state(scope, reader_obj, |state| {
        state.read_requests.borrow().len()
    })
    .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// ReadableStreamFulfillReadRequest — §3.9.1.16
// ---------------------------------------------------------------------------

/// `ReadableStreamFulfillReadRequest(stream, chunk, done)` — §3.9.1.16.
///
/// Pop the head read request from the reader's queue and dispatch one of
/// the three steps:
///   - `done == true`  → `closeSteps()`
///   - `done == false` → `chunkSteps(chunk)`
/// The `errorSteps` path is fired separately via the
/// `ReadableStreamDefaultReaderErrorReadRequests` algorithm.
pub fn readable_stream_fulfill_read_request<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
    chunk: v8::Local<'s, v8::Value>,
    done: bool,
) {
    let reader_v = slots::read_slot(scope, stream, READER);
    let Ok(reader_obj) = v8::Local::<v8::Object>::try_from(reader_v) else {
        return;
    };
    let req = crate::streams::readable_default_reader::with_state(scope, reader_obj, |state| {
        state.read_requests.borrow_mut().pop_front()
    })
    .flatten();
    let Some(req) = req else {
        return;
    };
    if done {
        invoke_close_steps(scope, req);
    } else {
        invoke_chunk_steps(scope, req, chunk);
    }
}

fn invoke_chunk_steps<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    req: ReadRequest,
    chunk: v8::Local<'s, v8::Value>,
) {
    match req.kind {
        ReadRequestKind::Js { resolver } => {
            let resolver_l = v8::Local::new(scope, &resolver);
            // Build { value: chunk, done: false } per WebIDL ReadableStreamReadResult.
            let result = v8::Object::new(scope);
            let value_key = v8::String::new(scope, "value").unwrap();
            let done_key = v8::String::new(scope, "done").unwrap();
            result.set(scope, value_key.into(), chunk);
            result.set(scope, done_key.into(), v8::Boolean::new(scope, false).into());
            resolver_l.resolve(scope, result.into());
        }
    }
}

fn invoke_close_steps(scope: &mut v8::PinScope, req: ReadRequest) {
    match req.kind {
        ReadRequestKind::Js { resolver } => {
            let resolver_l = v8::Local::new(scope, &resolver);
            let result = v8::Object::new(scope);
            let value_key = v8::String::new(scope, "value").unwrap();
            let done_key = v8::String::new(scope, "done").unwrap();
            result.set(scope, value_key.into(), v8::undefined(scope).into());
            result.set(scope, done_key.into(), v8::Boolean::new(scope, true).into());
            resolver_l.resolve(scope, result.into());
        }
    }
}

fn invoke_error_steps<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    req: ReadRequest,
    reason: v8::Local<'s, v8::Value>,
) {
    match req.kind {
        ReadRequestKind::Js { resolver } => {
            let resolver_l = v8::Local::new(scope, &resolver);
            resolver_l.reject(scope, reason);
        }
    }
}

// ---------------------------------------------------------------------------
// ReadableStreamCloseInternal — §3.9.1 ReadableStreamClose
// ---------------------------------------------------------------------------

/// `ReadableStreamClose(stream)` — §3.9.1.5.
///
/// 1. Assert state is "readable".
/// 2. Set state to "closed".
/// 3. Fulfill the reader's `[[closedPromise]]` with undefined.
/// 4. If the reader is a default reader, close all its outstanding
///    read requests with `{value: undefined, done: true}`.
pub fn readable_stream_close(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) {
    let rs_state = match crate::streams::readable::with_rs_state(scope, stream, |s| s.state.get()) {
        Some(s) => s,
        None => return,
    };
    debug_assert_eq!(rs_state, StreamState::Readable);
    crate::streams::readable::with_rs_state(scope, stream, |s| s.state.set(StreamState::Closed));

    // Reader:
    //  - resolve closedPromise with undefined
    //  - drain pending read requests with closeSteps each
    let reader_v = slots::read_slot(scope, stream, READER);
    let Ok(reader_obj) = v8::Local::<v8::Object>::try_from(reader_v) else {
        return;
    };
    crate::streams::readable_default_reader::resolve_closed_promise(scope, reader_obj);
    let drained: Vec<ReadRequest> =
        crate::streams::readable_default_reader::with_state(scope, reader_obj, |state| {
            state.read_requests.borrow_mut().drain(..).collect()
        })
        .unwrap_or_default();
    for req in drained {
        invoke_close_steps(scope, req);
    }
}

// ---------------------------------------------------------------------------
// ReadableStreamError — §3.9.1.6
// ---------------------------------------------------------------------------

/// `ReadableStreamError(stream, e)` — §3.9.1.6.
///
/// 1. Assert state is "readable".
/// 2. Set state to "errored".
/// 3. Set storedError to e.
/// 4. Reject reader's closedPromise with e (and set `[[PromiseIsHandled]]`).
/// 5. Reject all outstanding read requests with e.
pub fn readable_stream_error<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
    error: v8::Local<'s, v8::Value>,
) {
    let rs_state = match crate::streams::readable::with_rs_state(scope, stream, |s| s.state.get()) {
        Some(s) => s,
        None => return,
    };
    debug_assert_eq!(rs_state, StreamState::Readable);
    crate::streams::readable::with_rs_state(scope, stream, |s| s.state.set(StreamState::Errored));
    slots::write_slot(scope, stream, STORED_ERROR, error);

    let reader_v = slots::read_slot(scope, stream, READER);
    if let Ok(reader_obj) = v8::Local::<v8::Object>::try_from(reader_v) {
        crate::streams::readable_default_reader::reject_closed_promise(scope, reader_obj, error);
        let drained: Vec<ReadRequest> =
            crate::streams::readable_default_reader::with_state(scope, reader_obj, |state| {
                state.read_requests.borrow_mut().drain(..).collect()
            })
            .unwrap_or_default();
        for req in drained {
            invoke_error_steps(scope, req, error);
        }
    }
}

// ---------------------------------------------------------------------------
// ReadableStreamCancel — §3.9.1.3
// ---------------------------------------------------------------------------

/// `ReadableStreamCancel(stream, reason)` — §3.9.1.3. Returns a Promise.
///
/// 1. Set `[[disturbed]]` to true.
/// 2. If state == closed, return promiseResolvedWith(undefined).
/// 3. If state == errored, return promiseRejectedWith(storedError).
/// 4. ReadableStreamClose(stream).
/// 5. Run the controller's `[[CancelSteps]](reason)` → `sourceCancelPromise`.
/// 6. Return `sourceCancelPromise.then(_ => undefined)` (mapped via
///    react_to_promise_with).
pub fn readable_stream_cancel<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
    reason: v8::Local<'s, v8::Value>,
) -> v8::Local<'s, v8::Promise> {
    crate::streams::readable::with_rs_state(scope, stream, |s| s.disturbed.set(true));

    let rs_state = match crate::streams::readable::with_rs_state(scope, stream, |s| s.state.get()) {
        Some(s) => s,
        None => return resolved_undefined_promise(scope),
    };
    match rs_state {
        StreamState::Closed => {
            return resolved_undefined_promise(scope);
        }
        StreamState::Errored => {
            let stored = slots::read_slot(scope, stream, STORED_ERROR);
            let resolver = v8::PromiseResolver::new(scope).unwrap();
            let promise = resolver.get_promise(scope);
            resolver.reject(scope, stored);
            return promise;
        }
        StreamState::Readable => {}
    }

    readable_stream_close(scope, stream);

    // controller [[CancelSteps]] returns a Promise<undefined>.
    let source_cancel = crate::streams::readable_default_controller::cancel_steps(scope, stream, reason);

    crate::streams::promise_resolve::react_to_promise_with(
        scope,
        source_cancel,
        Some(Box::new(|scope, _value| {
            let undef: v8::Local<v8::Value> = v8::undefined(scope).into();
            v8::Global::new(scope, undef)
        })),
        None,
    )
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn resolved_undefined_promise<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::Promise> {
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let undef = v8::undefined(scope);
    resolver.resolve(scope, undef.into());
    promise
}

pub fn rejected_with_promise<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    reason: v8::Local<'s, v8::Value>,
) -> v8::Local<'s, v8::Promise> {
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    resolver.reject(scope, reason);
    promise
}

// ===========================================================================
// WritableStream cross-class abstract operations (§4.5)
// ===========================================================================

// ---------------------------------------------------------------------------
// IsWritableStreamLocked — §4.5.2
// ---------------------------------------------------------------------------

/// `IsWritableStreamLocked(stream)` — §4.5.2. Returns true iff `[[writer]]`
/// is set.
pub fn is_writable_stream_locked(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) -> bool {
    !slots::slot_is_empty(scope, stream, WRITER)
}

// ---------------------------------------------------------------------------
// WritableStreamAddWriteRequest — §4.5.3
// ---------------------------------------------------------------------------

/// `WritableStreamAddWriteRequest(stream)` — §4.5.3.
///
/// Allocates a new pending Promise+Resolver pair, pushes the Promise onto
/// `[[writeRequests]]` and the Resolver onto `write_request_resolvers`,
/// and returns the Promise (which writer.write() returns to JS).
pub fn writable_stream_add_write_request<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
) -> v8::Local<'s, v8::Promise> {
    debug_assert!(is_writable_stream_locked(scope, stream));
    let st = crate::streams::writable::with_ws_state(scope, stream, |s| s.state.get());
    debug_assert_eq!(st, Some(WSState::Writable));

    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let promise_g = v8::Global::new(scope, promise);
    let resolver_g = v8::Global::new(scope, resolver);
    crate::streams::writable::with_ws_state(scope, stream, |s| {
        s.write_requests.borrow_mut().push_back(promise_g);
        s.write_request_resolvers.borrow_mut().push_back(resolver_g);
    });
    promise
}

// ---------------------------------------------------------------------------
// WritableStreamCloseQueuedOrInFlight — §4.5.7
// ---------------------------------------------------------------------------

/// `WritableStreamCloseQueuedOrInFlight(stream)` — §4.5.7. True iff
/// `[[closeRequest]]` or `[[inFlightCloseRequest]]` is set.
pub fn writable_stream_close_queued_or_in_flight(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) -> bool {
    crate::streams::writable::with_ws_state(scope, stream, |s| {
        s.close_request.borrow().is_some() || s.in_flight_close_request.borrow().is_some()
    })
    .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// WritableStreamHasOperationMarkedInFlight — §4.5.8
// ---------------------------------------------------------------------------

pub fn writable_stream_has_operation_marked_in_flight(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) -> bool {
    crate::streams::writable::with_ws_state(scope, stream, |s| {
        s.in_flight_write_request.borrow().is_some() || s.in_flight_close_request.borrow().is_some()
    })
    .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// WritableStreamMarkCloseRequestInFlight — §4.5.9
// ---------------------------------------------------------------------------

pub fn writable_stream_mark_close_request_in_flight(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) {
    crate::streams::writable::with_ws_state(scope, stream, |s| {
        debug_assert!(s.in_flight_close_request.borrow().is_none());
        debug_assert!(s.close_request.borrow().is_some());
        // Move the resolver from close_request to in_flight_close_request.
        // The promise stays in close_request's record (not needed for resolution
        // — the resolver is what we drive). For our paired storage, we move
        // the whole pair: `close_request.take()` yields the PromisePair; we
        // store its resolver in `in_flight_close_request`.
        if let Some(pair) = s.close_request.borrow_mut().take() {
            *s.in_flight_close_request.borrow_mut() = Some(pair.resolver);
        }
    });
}

// ---------------------------------------------------------------------------
// WritableStreamMarkFirstWriteRequestInFlight — §4.5.10
// ---------------------------------------------------------------------------

pub fn writable_stream_mark_first_write_request_in_flight(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) {
    crate::streams::writable::with_ws_state(scope, stream, |s| {
        debug_assert!(s.in_flight_write_request.borrow().is_none());
        debug_assert!(!s.write_requests.borrow().is_empty());
        // Pop the head Promise (kept for FinishErroring's reject loop) and
        // its paired Resolver. The Resolver becomes [[inFlightWriteRequest]];
        // the Promise is "consumed" by the spec at this point (we drop it,
        // since the resolver is what we drive).
        s.write_requests.borrow_mut().pop_front();
        let resolver = s.write_request_resolvers.borrow_mut().pop_front();
        *s.in_flight_write_request.borrow_mut() = resolver;
    });
}

// ---------------------------------------------------------------------------
// WritableStreamUpdateBackpressure — §4.5.18
// ---------------------------------------------------------------------------

pub fn writable_stream_update_backpressure(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
    backpressure: bool,
) {
    let st = crate::streams::writable::with_ws_state(scope, stream, |s| s.state.get());
    debug_assert_eq!(st, Some(WSState::Writable));
    debug_assert!(!writable_stream_close_queued_or_in_flight(scope, stream));

    let prev = crate::streams::writable::with_ws_state(scope, stream, |s| s.backpressure.get())
        .unwrap_or(false);
    let writer_v = slots::read_slot(scope, stream, WRITER);
    if let Ok(writer) = v8::Local::<v8::Object>::try_from(writer_v) {
        if backpressure != prev {
            if backpressure {
                crate::streams::writable_writer::writable_stream_default_writer_reset_ready_promise(
                    scope, writer,
                );
            } else {
                crate::streams::writable_writer::writable_stream_default_writer_resolve_ready_promise(
                    scope, writer,
                );
            }
        }
    }
    crate::streams::writable::with_ws_state(scope, stream, |s| s.backpressure.set(backpressure));
}

// ---------------------------------------------------------------------------
// WritableStreamStartErroring — §4.5.17
// ---------------------------------------------------------------------------

pub fn writable_stream_start_erroring<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
    reason: v8::Local<'s, v8::Value>,
) {
    debug_assert!(slots::slot_is_empty(scope, stream, STORED_ERROR));
    let st = crate::streams::writable::with_ws_state(scope, stream, |s| s.state.get());
    debug_assert_eq!(st, Some(WSState::Writable));

    crate::streams::writable::with_ws_state(scope, stream, |s| {
        s.state.set(WSState::Erroring);
    });
    slots::write_slot(scope, stream, STORED_ERROR, reason);

    let writer_v = slots::read_slot(scope, stream, WRITER);
    if let Ok(writer) = v8::Local::<v8::Object>::try_from(writer_v) {
        crate::streams::writable_writer::writable_stream_default_writer_ensure_ready_promise_rejected(
            scope, writer, reason,
        );
    }

    let started = {
        let controller_v = slots::read_slot(scope, stream, slots::CONTROLLER);
        v8::Local::<v8::Object>::try_from(controller_v)
            .ok()
            .and_then(|c| {
                crate::streams::writable_controller::with_controller_state(scope, c, |s| {
                    s.started.get()
                })
            })
            .unwrap_or(false)
    };

    if !writable_stream_has_operation_marked_in_flight(scope, stream) && started {
        writable_stream_finish_erroring(scope, stream);
    }
}

// ---------------------------------------------------------------------------
// WritableStreamDealWithRejection — §4.5.4
// ---------------------------------------------------------------------------

pub fn writable_stream_deal_with_rejection<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
    error: v8::Local<'s, v8::Value>,
) {
    let st = crate::streams::writable::with_ws_state(scope, stream, |s| s.state.get())
        .unwrap_or(WSState::Errored);
    if st == WSState::Writable {
        writable_stream_start_erroring(scope, stream, error);
        return;
    }
    debug_assert_eq!(st, WSState::Erroring);
    writable_stream_finish_erroring(scope, stream);
}

// ---------------------------------------------------------------------------
// WritableStreamFinishErroring — §4.5.5
// ---------------------------------------------------------------------------

pub fn writable_stream_finish_erroring(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) {
    let st = crate::streams::writable::with_ws_state(scope, stream, |s| s.state.get());
    debug_assert_eq!(st, Some(WSState::Erroring));
    debug_assert!(!writable_stream_has_operation_marked_in_flight(scope, stream));

    crate::streams::writable::with_ws_state(scope, stream, |s| s.state.set(WSState::Errored));

    // controller [[ErrorSteps]]: ResetQueue.
    crate::streams::writable_controller::error_steps(scope, stream);

    // Reject all outstanding write requests with storedError.
    let stored = slots::read_slot(scope, stream, STORED_ERROR);
    let resolvers: Vec<v8::Global<v8::PromiseResolver>> =
        crate::streams::writable::with_ws_state(scope, stream, |s| {
            s.write_requests.borrow_mut().clear();
            s.write_request_resolvers.borrow_mut().drain(..).collect()
        })
        .unwrap_or_default();
    for r in resolvers {
        let r_l = v8::Local::new(scope, &r);
        r_l.reject(scope, stored);
    }

    // Pending abort request handling.
    let pending_abort: Option<PendingAbortRequest> =
        crate::streams::writable::with_ws_state(scope, stream, |s| {
            s.pending_abort_request.borrow_mut().take()
        })
        .flatten();

    let Some(abort_req) = pending_abort else {
        writable_stream_reject_close_and_closed_promise_if_needed(scope, stream);
        return;
    };

    if abort_req.was_already_erroring {
        let resolver_l = v8::Local::new(scope, &abort_req.resolver);
        resolver_l.reject(scope, stored);
        writable_stream_reject_close_and_closed_promise_if_needed(scope, stream);
        return;
    }

    // Run controller [[AbortSteps]](abortRequest.reason).
    let reason_l = v8::Local::new(scope, &abort_req.reason);
    let promise = crate::streams::writable_controller::abort_steps(scope, stream, reason_l);

    let stream_g = v8::Global::new(scope, stream);
    let stream_g2 = stream_g.clone();
    let abort_resolver = abort_req.resolver.clone();
    let abort_resolver2 = abort_req.resolver;
    crate::streams::promise_resolve::upon_promise(
        scope,
        promise,
        Some(Box::new(move |scope, _v| {
            let stream = v8::Local::new(scope, &stream_g);
            let resolver_l = v8::Local::new(scope, &abort_resolver);
            let undef = v8::undefined(scope);
            resolver_l.resolve(scope, undef.into());
            writable_stream_reject_close_and_closed_promise_if_needed(scope, stream);
        })),
        Some(Box::new(move |scope, reason| {
            let stream = v8::Local::new(scope, &stream_g2);
            let resolver_l = v8::Local::new(scope, &abort_resolver2);
            resolver_l.reject(scope, reason);
            writable_stream_reject_close_and_closed_promise_if_needed(scope, stream);
        })),
    );
}

// ---------------------------------------------------------------------------
// WritableStreamRejectCloseAndClosedPromiseIfNeeded — §4.5.16
// ---------------------------------------------------------------------------

pub fn writable_stream_reject_close_and_closed_promise_if_needed(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) {
    let st = crate::streams::writable::with_ws_state(scope, stream, |s| s.state.get());
    debug_assert_eq!(st, Some(WSState::Errored));

    let stored = slots::read_slot(scope, stream, STORED_ERROR);
    // Reject closeRequest's resolver, if present.
    let close_pair = crate::streams::writable::with_ws_state(scope, stream, |s| {
        s.close_request.borrow_mut().take()
    })
    .flatten();
    if let Some(pair) = close_pair {
        let resolver_l = v8::Local::new(scope, &pair.resolver);
        resolver_l.reject(scope, stored);
    }
    // Reject writer.closedPromise.
    let writer_v = slots::read_slot(scope, stream, WRITER);
    if let Ok(writer) = v8::Local::<v8::Object>::try_from(writer_v) {
        crate::streams::writable_writer::writable_stream_default_writer_ensure_closed_promise_rejected(
            scope, writer, stored,
        );
    }
}

// ---------------------------------------------------------------------------
// WritableStreamFinishInFlightWrite — §4.5.11
// ---------------------------------------------------------------------------

pub fn writable_stream_finish_in_flight_write(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) {
    let resolver = crate::streams::writable::with_ws_state(scope, stream, |s| {
        s.in_flight_write_request.borrow_mut().take()
    })
    .flatten();
    debug_assert!(resolver.is_some());
    if let Some(r) = resolver {
        let r_l = v8::Local::new(scope, &r);
        let undef = v8::undefined(scope);
        r_l.resolve(scope, undef.into());
    }
}

// ---------------------------------------------------------------------------
// WritableStreamFinishInFlightWriteWithError — §4.5.12
// ---------------------------------------------------------------------------

pub fn writable_stream_finish_in_flight_write_with_error<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
    error: v8::Local<'s, v8::Value>,
) {
    let resolver = crate::streams::writable::with_ws_state(scope, stream, |s| {
        s.in_flight_write_request.borrow_mut().take()
    })
    .flatten();
    debug_assert!(resolver.is_some());
    if let Some(r) = resolver {
        let r_l = v8::Local::new(scope, &r);
        r_l.reject(scope, error);
    }
    let st = crate::streams::writable::with_ws_state(scope, stream, |s| s.state.get())
        .unwrap_or(WSState::Errored);
    debug_assert!(st == WSState::Writable || st == WSState::Erroring);

    writable_stream_deal_with_rejection(scope, stream, error);
}

// ---------------------------------------------------------------------------
// WritableStreamFinishInFlightClose — §4.5.13
// ---------------------------------------------------------------------------

pub fn writable_stream_finish_in_flight_close(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) {
    let resolver = crate::streams::writable::with_ws_state(scope, stream, |s| {
        s.in_flight_close_request.borrow_mut().take()
    })
    .flatten();
    debug_assert!(resolver.is_some());
    if let Some(r) = resolver {
        let r_l = v8::Local::new(scope, &r);
        let undef = v8::undefined(scope);
        r_l.resolve(scope, undef.into());
    }
    let st = crate::streams::writable::with_ws_state(scope, stream, |s| s.state.get())
        .unwrap_or(WSState::Errored);
    debug_assert!(st == WSState::Writable || st == WSState::Erroring);

    if st == WSState::Erroring {
        // The error was too late; ignore. Clear storedError + pendingAbort.
        slots::delete_slot(scope, stream, STORED_ERROR);
        let pending = crate::streams::writable::with_ws_state(scope, stream, |s| {
            s.pending_abort_request.borrow_mut().take()
        })
        .flatten();
        if let Some(p) = pending {
            let r_l = v8::Local::new(scope, &p.resolver);
            let undef = v8::undefined(scope);
            r_l.resolve(scope, undef.into());
        }
    }
    crate::streams::writable::with_ws_state(scope, stream, |s| s.state.set(WSState::Closed));

    let writer_v = slots::read_slot(scope, stream, WRITER);
    if let Ok(writer) = v8::Local::<v8::Object>::try_from(writer_v) {
        crate::streams::writable_writer::writable_stream_default_writer_resolve_closed_promise(
            scope, writer,
        );
    }

    debug_assert!(crate::streams::writable::with_ws_state(scope, stream, |s| {
        s.pending_abort_request.borrow().is_none()
    })
    .unwrap_or(true));
    debug_assert!(slots::slot_is_empty(scope, stream, STORED_ERROR));
}

// ---------------------------------------------------------------------------
// WritableStreamFinishInFlightCloseWithError — §4.5.14
// ---------------------------------------------------------------------------

pub fn writable_stream_finish_in_flight_close_with_error<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
    error: v8::Local<'s, v8::Value>,
) {
    let resolver = crate::streams::writable::with_ws_state(scope, stream, |s| {
        s.in_flight_close_request.borrow_mut().take()
    })
    .flatten();
    debug_assert!(resolver.is_some());
    if let Some(r) = resolver {
        let r_l = v8::Local::new(scope, &r);
        r_l.reject(scope, error);
    }
    let st = crate::streams::writable::with_ws_state(scope, stream, |s| s.state.get())
        .unwrap_or(WSState::Errored);
    debug_assert!(st == WSState::Writable || st == WSState::Erroring);

    // Never execute sink abort() after sink close().
    let pending = crate::streams::writable::with_ws_state(scope, stream, |s| {
        s.pending_abort_request.borrow_mut().take()
    })
    .flatten();
    if let Some(p) = pending {
        let r_l = v8::Local::new(scope, &p.resolver);
        r_l.reject(scope, error);
    }
    writable_stream_deal_with_rejection(scope, stream, error);
}

// ---------------------------------------------------------------------------
// WritableStreamAbort — §4.5.6
// ---------------------------------------------------------------------------

pub fn writable_stream_abort<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
    mut reason: v8::Local<'s, v8::Value>,
) -> v8::Local<'s, v8::Promise> {
    let st = crate::streams::writable::with_ws_state(scope, stream, |s| s.state.get())
        .unwrap_or(WSState::Errored);
    if st == WSState::Closed || st == WSState::Errored {
        return resolved_undefined_promise(scope);
    }

    // KNOWN GAP: would call controller._abortController.abort(reason) here
    // once native AbortController/AbortSignal lands. The state-check below
    // remains identical regardless.

    let st = crate::streams::writable::with_ws_state(scope, stream, |s| s.state.get())
        .unwrap_or(WSState::Errored);
    if st == WSState::Closed || st == WSState::Errored {
        return resolved_undefined_promise(scope);
    }

    // If pendingAbortRequest is set, return its existing promise. We keep
    // the Promise mirrored on a priv-sym so subsequent abort() callers see
    // the same Promise (the spec says abort() returns the same Promise as
    // the original pending abort).
    let abort_promise_v = slots::read_slot(scope, stream, "[[ws.pendingAbortPromise]]");
    if !abort_promise_v.is_undefined() {
        if let Ok(p) = v8::Local::<v8::Promise>::try_from(abort_promise_v) {
            return p;
        }
    }

    debug_assert!(st == WSState::Writable || st == WSState::Erroring);

    let mut was_already_erroring = false;
    if st == WSState::Erroring {
        was_already_erroring = true;
        // reason will not be used.
        reason = v8::undefined(scope).into();
    }

    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let resolver_g = v8::Global::new(scope, resolver);
    let reason_g = v8::Global::new(scope, reason);

    crate::streams::writable::with_ws_state(scope, stream, |s| {
        *s.pending_abort_request.borrow_mut() = Some(PendingAbortRequest {
            resolver: resolver_g,
            reason: reason_g,
            was_already_erroring,
        });
    });

    // Mirror promise into priv sym so subsequent abort() calls return same.
    slots::write_slot(scope, stream, "[[ws.pendingAbortPromise]]", promise.into());

    if !was_already_erroring {
        writable_stream_start_erroring(scope, stream, reason);
    }

    promise
}

// ---------------------------------------------------------------------------
// WritableStreamClose — §4.5.7
// ---------------------------------------------------------------------------

pub fn writable_stream_close<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
) -> v8::Local<'s, v8::Promise> {
    let st = crate::streams::writable::with_ws_state(scope, stream, |s| s.state.get())
        .unwrap_or(WSState::Errored);
    if st == WSState::Closed || st == WSState::Errored {
        let msg = format!(
            "The stream (in {} state) is not in the writable state and cannot be closed",
            match st {
                WSState::Closed => "closed",
                _ => "errored",
            }
        );
        let v8_msg = v8::String::new(scope, &msg).unwrap();
        let exc = v8::Exception::type_error(scope, v8_msg);
        return rejected_with_promise(scope, exc.into());
    }

    debug_assert!(st == WSState::Writable || st == WSState::Erroring);
    debug_assert!(!writable_stream_close_queued_or_in_flight(scope, stream));

    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let promise_g = v8::Global::new(scope, promise);
    let resolver_g = v8::Global::new(scope, resolver);

    crate::streams::writable::with_ws_state(scope, stream, |s| {
        *s.close_request.borrow_mut() = Some(PromisePair {
            promise: promise_g,
            resolver: resolver_g,
        });
    });

    // If writer is set AND stream backpressure AND state == Writable: resolve
    // the writer's readyPromise (so any pending write() can settle the queue).
    let writer_v = slots::read_slot(scope, stream, WRITER);
    if let Ok(writer) = v8::Local::<v8::Object>::try_from(writer_v) {
        let bp = crate::streams::writable::with_ws_state(scope, stream, |s| s.backpressure.get())
            .unwrap_or(false);
        if bp && st == WSState::Writable {
            crate::streams::writable_writer::writable_stream_default_writer_resolve_ready_promise(
                scope, writer,
            );
        }
    }

    // Enqueue the close sentinel in the controller.
    let controller_v = slots::read_slot(scope, stream, slots::CONTROLLER);
    if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
        crate::streams::writable_controller::writable_stream_default_controller_close(
            scope, controller,
        );
    }

    promise
}

// Suppress unused warnings (the WSStreamState is used via with_ws_state).
#[allow(dead_code)]
fn _unused_ws_state(_s: &WSStreamState) {}

// ===========================================================================
// Pipe + Tee — re-exports of the cross-class algorithms (§3.5)
// ===========================================================================

/// `ReadableStreamPipeTo(source, dest, preventClose, preventAbort,
/// preventCancel, signal)` — spec §3.5.1. Re-exported here for the
/// "abstract operation lives at top-level" naming convention (D-20).
/// The implementation lives in `crate::streams::pipe`.
pub fn readable_stream_pipe_to<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    source: v8::Local<v8::Object>,
    dest: v8::Local<v8::Object>,
    prevent_close: bool,
    prevent_abort: bool,
    prevent_cancel: bool,
    signal: Option<v8::Local<v8::Object>>,
) -> v8::Local<'s, v8::Promise> {
    crate::streams::pipe::readable_stream_pipe_to(
        scope,
        source,
        dest,
        prevent_close,
        prevent_abort,
        prevent_cancel,
        signal,
    )
}

/// `ReadableStreamTee(stream, cloneForBranch2)` — spec §3.5.2. Public
/// `tee()` always passes `cloneForBranch2 = false` (the default-tee
/// path). The byte-tee path (§3.5.3) lands with BYOB byte streams.
pub fn readable_stream_tee<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
    clone_for_branch2: bool,
) -> Result<[v8::Local<'s, v8::Object>; 2], String> {
    crate::streams::tee::readable_stream_default_tee(scope, stream, clone_for_branch2)
}

// ===========================================================================
// TransformStream cross-class abstract operations (§5.4)
// ===========================================================================
//
// Per design §III.8 / §II.11, these operations span the TransformStream
// container and its `[[readable]]` / `[[writable]]` halves. They live in
// `algorithms.rs` because they coordinate state across both halves.

// ---------------------------------------------------------------------------
// TransformStreamSetBackpressure — §5.4.7
// ---------------------------------------------------------------------------
//
// Per spec:
// 1. Assert: stream.[[backpressure]] !== backpressure.
// 2. If stream.[[backpressureChangePromise]] is not undefined, resolve it.
// 3. Set stream.[[backpressureChangePromise]] to a new pending promise.
// 4. Set stream.[[backpressure]] to backpressure.
//
// Per design §II.11 paired-storage rule + critic #52 fix: we resolve the
// existing change-promise then create a fresh pre-pending one (Promise +
// paired Resolver).

pub fn transform_stream_set_backpressure(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
    backpressure: bool,
) {
    // Resolve the existing resolver, if any. (On the very first call from
    // InitializeTransformStream, the resolver is freshly-created and we
    // resolve it once below. The spec's Assert(prev !== backpressure) is
    // upheld at every other callsite.)
    let prev_resolver = crate::streams::transform::with_ts_state(scope, stream, |s| {
        s.bp_change_resolver.borrow_mut().take()
    })
    .flatten();
    if let Some(resolver_g) = prev_resolver {
        let r_l = v8::Local::new(scope, &resolver_g);
        let undef = v8::undefined(scope);
        r_l.resolve(scope, undef.into());
    }
    // Create a fresh pending promise+resolver pair and store them.
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let resolver_g = v8::Global::new(scope, resolver);
    let promise_g = v8::Global::new(scope, promise);
    crate::streams::transform::with_ts_state(scope, stream, |s| {
        *s.bp_change_promise.borrow_mut() = Some(promise_g);
        *s.bp_change_resolver.borrow_mut() = Some(resolver_g);
        s.backpressure.set(backpressure);
    });
}

// ---------------------------------------------------------------------------
// TransformStreamUnblockWrite — used by Erroring path
// ---------------------------------------------------------------------------
//
// Per spec §5.4.4: TransformStreamErrorWritableAndUnblockWrite calls
// "TransformStreamSetBackpressure(stream, false)" only if backpressure was
// true. We expose this as a helper so transform_stream_error_writable_…
// can dispatch it cleanly.

pub fn transform_stream_unblock_write(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) {
    let bp = crate::streams::transform::with_ts_state(scope, stream, |s| s.backpressure.get())
        .unwrap_or(false);
    if bp {
        transform_stream_set_backpressure(scope, stream, false);
    }
}

// ---------------------------------------------------------------------------
// TransformStreamError — §5.4.5
// ---------------------------------------------------------------------------
//
// Per spec:
// 1. ReadableStreamDefaultControllerError(stream.[[readable]].[[controller]], e).
// 2. TransformStreamErrorWritableAndUnblockWrite(stream, e).

pub fn transform_stream_error<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
    error: v8::Local<'s, v8::Value>,
) {
    let readable = crate::streams::transform::readable_slot_obj(scope, stream);
    if let Some(readable) = readable {
        let controller_v = slots::read_slot(scope, readable, slots::CONTROLLER);
        if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
            crate::streams::readable_default_controller::readable_stream_default_controller_error(
                scope, controller, error,
            );
        }
    }
    transform_stream_error_writable_and_unblock_write(scope, stream, error);
}

// ---------------------------------------------------------------------------
// TransformStreamErrorWritableAndUnblockWrite — §5.4.4
// ---------------------------------------------------------------------------
//
// Per spec:
// 1. TransformStreamDefaultControllerClearAlgorithms(stream.[[controller]]).
// 2. WritableStreamDefaultControllerErrorIfNeeded(stream.[[writable]].[[controller]], e).
// 3. TransformStreamUnblockWrite(stream).

pub fn transform_stream_error_writable_and_unblock_write<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
    error: v8::Local<'s, v8::Value>,
) {
    // Clear TS controller algorithms.
    let ts_ctrl_v = crate::streams::transform::ts_controller_slot(scope, stream);
    if let Ok(ts_ctrl) = v8::Local::<v8::Object>::try_from(ts_ctrl_v) {
        crate::streams::transform_controller::transform_stream_default_controller_clear_algorithms(
            scope, ts_ctrl,
        );
    }

    // Error WS controller (which errors the writable side).
    let writable = crate::streams::transform::writable_slot_obj(scope, stream);
    if let Some(writable) = writable {
        let controller_v = slots::read_slot(scope, writable, slots::CONTROLLER);
        if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
            crate::streams::writable_controller::writable_stream_default_controller_error_if_needed(
                scope, controller, error,
            );
        }
    }

    transform_stream_unblock_write(scope, stream);
}

// ---------------------------------------------------------------------------
// InitializeTransformStream — §5.4.1
// ---------------------------------------------------------------------------
//
// Per spec InitializeTransformStream(stream, startPromise, writableHWM,
// writableSizeAlgorithm, readableHWM, readableSizeAlgorithm):
//
//  1. Build the writable side wrapping a "TransformStreamDefaultSink" that
//     forwards write→transform, close→flush, abort→error.
//  2. Build the readable side wrapping a "TransformStreamDefaultSource"
//     that pulls via backpressureChangePromise.
//  3. Set [[backpressure]] = undefined and [[backpressureChangePromise]]
//     = undefined; then call TransformStreamSetBackpressure(stream, true)
//     to initialize the pair.
//  4. Wire the cross-class start so both sides observe `startPromise`.
//
// This function does NOT install the controller. SetUpTransformStreamDefault
// ControllerFromTransformer (lives in transform_controller.rs) installs the
// TS controller and then calls Initialize+SetBackpressure(true) per spec
// ordering.
//
// Because the readable + writable halves are constructed via existing
// `set_up_…_from_…` helpers that consume `AlgorithmFn`, we route the
// sink's write/close/abort and the source's pull/cancel through the
// "forwarder" closures registered on the TS state.

pub fn initialize_transform_stream<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
    start_promise: v8::Local<'s, v8::Promise>,
    writable_hwm: f64,
    writable_size: crate::streams::readable_default_controller::SizeAlgorithm,
    readable_hwm: f64,
    readable_size: crate::streams::readable_default_controller::SizeAlgorithm,
) {
    use crate::streams::readable_default_controller::AlgorithmFn;

    // Build the readable (read side of the TS) — pull = waits on
    // backpressureChangePromise; cancel = TransformStreamSourceCancel.
    let readable = crate::streams::transform::build_readable_for_ts(
        scope,
        stream,
        start_promise,
        readable_hwm,
        readable_size,
    );

    // Build the writable (write side of the TS) — write = perform_transform;
    // close = perform flush + close readable; abort = error TS.
    let writable = crate::streams::transform::build_writable_for_ts(
        scope,
        stream,
        start_promise,
        writable_hwm,
        writable_size,
    );

    // Stash both halves in priv-syms on the TS wrapper.
    crate::streams::slots::write_slot(scope, stream, slots::READABLE, readable.into());
    crate::streams::slots::write_slot(scope, stream, slots::WRITABLE, writable.into());

    // Initialize backpressure pair — None initially; SetBackpressure(true)
    // creates the first pair.
    transform_stream_set_backpressure(scope, stream, true);

    // Suppress unused warnings on the AlgorithmFn import — it's referenced
    // by the helpers above.
    #[allow(dead_code)]
    fn _touch(_a: AlgorithmFn) {}
}
