//! Cross-class abstract operations for ReadableStream (spec §3.9.1, §3.9.2).
//!
//! Per D-20: spec abstract operations whose name doesn't sit on a single
//! class (e.g. `ReadableStreamCancel`, `ReadableStreamFulfillReadRequest`)
//! live here. Class-local operations (`ReadableStreamDefaultControllerEnqueue`)
//! live in their respective class file.
//!
//! Style: each function is named with the spec name in `snake_case`. Spec
//! sections are cited at every function so reviewers can grep for the WPT
//! spec text.

use crate::streams::readable::StreamState;
use crate::streams::readable_default_reader::{ReadRequest, ReadRequestKind};
use crate::streams::slots::{self, READER, STORED_ERROR};

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
