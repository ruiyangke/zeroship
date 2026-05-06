//! `ReadableByteStreamController` — spec §3.7 + §3.11.
//!
//! IDL (§3.7):
//! ```webidl
//! [Exposed=*]
//! interface ReadableByteStreamController {
//!   readonly attribute ReadableStreamBYOBRequest? byobRequest;
//!   readonly attribute unrestricted double? desiredSize;
//!   undefined close();
//!   undefined enqueue(ArrayBufferView chunk);
//!   undefined error(optional any e);
//! };
//! ```
//!
//! Internal slots (§II.6 + §XV.3):
//! - `[[autoAllocateChunkSize]]`     → Rust `Option<u64>`
//! - `[[byobRequest]]`               → V8 priv sym `[[byobRequest]]`
//! - `[[cancelAlgorithm]]`           → Rust enum AlgorithmFn
//! - `[[closeRequested]]`            → Rust Cell<bool>
//! - `[[pullAgain]]`                 → Rust Cell<bool>
//! - `[[pullAlgorithm]]`             → Rust enum AlgorithmFn
//! - `[[pulling]]`                   → Rust Cell<bool>
//! - `[[queue]]`                     → Rust ByteQueue (queue.rs)
//! - `[[queueTotalSize]]`            → Rust f64 inside ByteQueue
//! - `[[started]]`                   → Rust Cell<bool>
//! - `[[strategyHWM]]`               → Rust f64
//! - `[[stream]]`                    → V8 priv sym `[[bc.streamObj]]`
//! - `[[pendingPullIntos]]`          → Rust VecDeque<PullIntoDescriptor>
//!
//! `[[byobRequest]]` lives only in the V8 private symbol.
//! `respond(0)` gates on stream `[[state]]`, not `closeRequested`.
//! Every BufferSource path checks `was_detached`.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

use crate::streams::algorithms;
use crate::streams::promise_resolve;
use crate::streams::pull_into::{
    can_transfer_array_buffer, copy_data_block_bytes, is_detached_buffer, transfer_array_buffer,
    PullIntoDescriptor, ReaderType, ViewConstructor,
};
use crate::streams::queue::{ByteQueue, ByteQueueEntry};
use crate::streams::readable::{with_rs_state, StreamState};
use crate::streams::readable_default_controller::AlgorithmFn;
use crate::streams::readable_default_reader::{
    fulfill_read_request_chunk, ReadRequest, ReadRequestKind, ReadRequestNative,
};
use crate::streams::slots::{self, BYOB_REQUEST, CONTROLLER, STORED_ERROR};

const STREAM_OBJ_SLOT: &str = "[[bc.streamObj]]";

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// `Box<ByteControllerState>` lives in the controller wrapper's V8
/// internal field 0.
#[allow(missing_debug_implementations)]
pub struct ByteControllerState {
    pub queue: ByteQueue,
    pub strategy_hwm: f64,
    pub auto_allocate_chunk_size: Option<u64>,
    pub pull_algorithm: AlgorithmFn,
    pub cancel_algorithm: AlgorithmFn,
    pub started: Cell<bool>,
    pub close_requested: Cell<bool>,
    pub pulling: Cell<bool>,
    pub pull_again: Cell<bool>,
    pub pending_pull_intos: RefCell<VecDeque<PullIntoDescriptor>>,
}

impl ByteControllerState {
    fn new(
        hwm: f64,
        auto_allocate_chunk_size: Option<u64>,
        pull_algorithm: AlgorithmFn,
        cancel_algorithm: AlgorithmFn,
    ) -> Self {
        Self {
            queue: ByteQueue::new(),
            strategy_hwm: hwm,
            auto_allocate_chunk_size,
            pull_algorithm,
            cancel_algorithm,
            started: Cell::new(false),
            close_requested: Cell::new(false),
            pulling: Cell::new(false),
            pull_again: Cell::new(false),
            pending_pull_intos: RefCell::new(VecDeque::new()),
        }
    }
}

// ---------------------------------------------------------------------------
// V8 wrapper helpers
// ---------------------------------------------------------------------------

/// True iff `obj` looks like a ReadableByteStreamController wrapper (its
/// internal field 0 holds a non-null External pointing at our state). We
/// distinguish from default controllers via a class-tag priv sym set at
/// `set_up_*` time.
const BC_TAG_SLOT: &str = "[[bc.tag]]";

pub fn is_byte_controller(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>) -> bool {
    if obj
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
        .map(|e| e.value().is_null())
        .unwrap_or(true)
    {
        return false;
    }
    !slots::slot_is_empty(scope, obj, BC_TAG_SLOT)
}

pub fn with_controller_state<R>(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
    f: impl FnOnce(&ByteControllerState) -> R,
) -> Option<R> {
    let raw = controller.get_internal_field(scope, 0)?;
    let ext = v8::Local::<v8::External>::try_from(raw).ok()?;
    let ptr = ext.value() as *const ByteControllerState;
    if ptr.is_null() {
        return None;
    }
    // SAFETY: External points at a Box<ByteControllerState> set during
    // construction; dropped only by the V8 weak finalizer.
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
// Spec algorithms — §3.11
// ---------------------------------------------------------------------------

/// `ReadableByteStreamControllerGetDesiredSize(controller)` — §3.11.x.
/// Returns:
/// - None (JS null) if state is errored
/// - Some(0)        if state is closed
/// - Some(hwm - queueTotalSize) otherwise
pub fn readable_byte_stream_controller_get_desired_size(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) -> Option<f64> {
    let stream = stream_obj(scope, controller)?;
    let state = with_rs_state(scope, stream, |s| s.state.get())?;
    if state == StreamState::Errored {
        return None;
    }
    if state == StreamState::Closed {
        return Some(0.0);
    }
    with_controller_state(scope, controller, |s| s.strategy_hwm - s.queue.total_size())
}

/// `ReadableByteStreamControllerShouldCallPull(controller)` — spec §3.11.
pub fn readable_byte_stream_controller_should_call_pull(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) -> bool {
    let stream = match stream_obj(scope, controller) {
        Some(s) => s,
        None => return false,
    };
    // 1. If state is not "readable" → return false.
    let st = match with_rs_state(scope, stream, |s| s.state.get()) {
        Some(s) => s,
        None => return false,
    };
    if st != StreamState::Readable {
        return false;
    }
    // 2. If closeRequested is true → return false.
    let close_requested =
        with_controller_state(scope, controller, |s| s.close_requested.get()).unwrap_or(true);
    if close_requested {
        return false;
    }
    // 3. If started is false → return false.
    let started = with_controller_state(scope, controller, |s| s.started.get()).unwrap_or(false);
    if !started {
        return false;
    }
    // 4. If ReadableStreamHasDefaultReader && GetNumReadRequests > 0 → true.
    if algorithms::readable_stream_has_default_reader(scope, stream)
        && algorithms::readable_stream_get_num_read_requests(scope, stream) > 0
    {
        return true;
    }
    // 5. If ReadableStreamHasBYOBReader && GetNumReadIntoRequests > 0 → true.
    if crate::streams::readable_byob_reader::readable_stream_has_byob_reader(scope, stream)
        && crate::streams::readable_byob_reader::readable_stream_get_num_read_into_requests(
            scope, stream,
        ) > 0
    {
        return true;
    }
    // 6. desiredSize > 0 → true; else false.
    matches!(
        readable_byte_stream_controller_get_desired_size(scope, controller),
        Some(n) if n > 0.0
    )
}

/// `ReadableByteStreamControllerCallPullIfNeeded(controller)` — spec §3.11.
pub fn readable_byte_stream_controller_call_pull_if_needed(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) {
    if !readable_byte_stream_controller_should_call_pull(scope, controller) {
        return;
    }
    let pulling = with_controller_state(scope, controller, |s| s.pulling.get()).unwrap_or(false);
    if pulling {
        with_controller_state(scope, controller, |s| s.pull_again.set(true));
        return;
    }
    debug_assert!(
        !with_controller_state(scope, controller, |s| s.pull_again.get()).unwrap_or(true)
    );
    with_controller_state(scope, controller, |s| s.pulling.set(true));
    let pull_promise = invoke_pull_algorithm(scope, controller);

    let controller_g = v8::Global::new(scope, controller);
    let controller_g2 = controller_g.clone();
    promise_resolve::upon_promise(
        scope,
        pull_promise,
        Some(Box::new(move |scope, _v| {
            let controller = v8::Local::new(scope, &controller_g);
            with_controller_state(scope, controller, |s| s.pulling.set(false));
            let pull_again = with_controller_state(scope, controller, |s| s.pull_again.get())
                .unwrap_or(false);
            if pull_again {
                with_controller_state(scope, controller, |s| s.pull_again.set(false));
                readable_byte_stream_controller_call_pull_if_needed(scope, controller);
            }
        })),
        Some(Box::new(move |scope, reason| {
            let controller = v8::Local::new(scope, &controller_g2);
            readable_byte_stream_controller_error(scope, controller, reason);
        })),
    );
}

fn invoke_pull_algorithm<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller: v8::Local<v8::Object>,
) -> v8::Local<'s, v8::Promise> {
    let snap = with_controller_state(scope, controller, |s| algorithm_snapshot(&s.pull_algorithm))
        .flatten();
    let Some(snap) = snap else {
        return algorithms::resolved_undefined_promise(scope);
    };
    snap.invoke_with_controller(scope, controller)
}

/// Cheap snapshot of an AlgorithmFn — same pattern as default
/// controller (avoid holding the borrow across user-callback execution).
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
            AlgorithmSnapshot::Js { function, this_obj } => AlgorithmFn::Js { function, this_obj },
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
            AlgorithmSnapshot::Js { function, this_obj } => AlgorithmFn::Js { function, this_obj },
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

// ---------------------------------------------------------------------------
// ClearAlgorithms / ClearPendingPullIntos / Close / Error / HandleQueueDrain
// ---------------------------------------------------------------------------

/// `ReadableByteStreamControllerClearAlgorithms(controller)` — §3.11.x.
pub fn readable_byte_stream_controller_clear_algorithms(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) {
    let raw = match controller
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
    {
        Some(e) => e.value() as *mut ByteControllerState,
        None => return,
    };
    if raw.is_null() {
        return;
    }
    let state = unsafe { &mut *raw };
    state.pull_algorithm = AlgorithmFn::Noop;
    state.cancel_algorithm = AlgorithmFn::Noop;
}

/// `ReadableByteStreamControllerClearPendingPullIntos(controller)` — §3.11.x.
pub fn readable_byte_stream_controller_clear_pending_pull_intos(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) {
    readable_byte_stream_controller_invalidate_byob_request(scope, controller);
    with_controller_state(scope, controller, |s| {
        s.pending_pull_intos.borrow_mut().clear();
    });
}

/// `ReadableByteStreamControllerInvalidateBYOBRequest(controller)` — §3.11.x.
pub fn readable_byte_stream_controller_invalidate_byob_request(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) {
    let req_v = slots::read_slot(scope, controller, BYOB_REQUEST);
    let Ok(req) = v8::Local::<v8::Object>::try_from(req_v) else {
        return;
    };
    crate::streams::byob_request::invalidate(scope, req);
    slots::delete_slot(scope, controller, BYOB_REQUEST);
}

/// `ReadableByteStreamControllerHandleQueueDrain(controller)` — §3.11.x.
pub fn readable_byte_stream_controller_handle_queue_drain(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) {
    let stream = match stream_obj(scope, controller) {
        Some(s) => s,
        None => return,
    };
    debug_assert_eq!(
        with_rs_state(scope, stream, |s| s.state.get()),
        Some(StreamState::Readable)
    );
    let q_empty = with_controller_state(scope, controller, |s| s.queue.is_empty()).unwrap_or(true);
    let close_requested =
        with_controller_state(scope, controller, |s| s.close_requested.get()).unwrap_or(false);
    if q_empty && close_requested {
        readable_byte_stream_controller_clear_algorithms(scope, controller);
        algorithms::readable_stream_close(scope, stream);
    } else {
        readable_byte_stream_controller_call_pull_if_needed(scope, controller);
    }
}

/// `ReadableByteStreamControllerError(controller, e)` — §3.11.x.
pub fn readable_byte_stream_controller_error<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller: v8::Local<v8::Object>,
    error: v8::Local<'s, v8::Value>,
) {
    let stream = match stream_obj(scope, controller) {
        Some(s) => s,
        None => return,
    };
    let st = match with_rs_state(scope, stream, |s| s.state.get()) {
        Some(s) => s,
        None => return,
    };
    if st != StreamState::Readable {
        return;
    }
    readable_byte_stream_controller_clear_pending_pull_intos(scope, controller);
    with_controller_state(scope, controller, |s| s.queue.reset_queue());
    readable_byte_stream_controller_clear_algorithms(scope, controller);
    algorithms::readable_stream_error(scope, stream, error);
}

/// `ReadableByteStreamControllerClose(controller)` — §3.11.x.
pub fn readable_byte_stream_controller_close(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) {
    let close_requested =
        with_controller_state(scope, controller, |s| s.close_requested.get()).unwrap_or(true);
    if close_requested {
        return;
    }
    let stream = match stream_obj(scope, controller) {
        Some(s) => s,
        None => return,
    };
    let st = match with_rs_state(scope, stream, |s| s.state.get()) {
        Some(s) => s,
        None => return,
    };
    if st != StreamState::Readable {
        return;
    }
    let queue_total = with_controller_state(scope, controller, |s| s.queue.total_size())
        .unwrap_or(0.0);
    // Spec step 4: defer close while queue has bytes (the queue will be
    // drained by ProcessReadRequestsUsingQueue or ProcessPullIntoUsingQueue
    // and HandleQueueDrain will then call close).
    let has_pending = with_controller_state(scope, controller, |s| {
        !s.pending_pull_intos.borrow().is_empty()
    })
    .unwrap_or(false);
    with_controller_state(scope, controller, |s| s.close_requested.set(true));
    if queue_total > 0.0 {
        // Defer close until queue drains.
        return;
    }
    // Spec step 5: a pending pull-into with non-aligned filled bytes
    // means the consumer asked for whole elements but the stream gave
    // us a partial element — that's a usage error.
    if has_pending {
        let unaligned = with_controller_state(scope, controller, |s| {
            s.pending_pull_intos
                .borrow()
                .front()
                .map(|d| d.bytes_filled % d.element_size != 0)
                .unwrap_or(false)
        })
        .unwrap_or(false);
        if unaligned {
            let msg = v8::String::new(
                scope,
                "Insufficient bytes to fill elements in the given buffer",
            )
            .unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            readable_byte_stream_controller_error(scope, controller, exc);
            // Throw to the caller of close().
            scope.throw_exception(exc);
            return;
        }
    }
    readable_byte_stream_controller_clear_algorithms(scope, controller);
    algorithms::readable_stream_close(scope, stream);
}

// ---------------------------------------------------------------------------
// Enqueue paths with detached-buffer checks throughout
// ---------------------------------------------------------------------------

/// `ReadableByteStreamControllerEnqueue(controller, chunk)` — §3.11.x.
///
/// Spec steps:
///   1. If queue is empty AND closeRequested is true → return.
///   2. View args: buffer = chunk.[[ArrayBuffer]], byteOffset, byteLength.
///   3. If IsDetachedBuffer(buffer) → throw TypeError.
///   4. transferredBuffer = TransferArrayBuffer(buffer).
///   5. If pendingPullIntos non-empty:
///        firstPending = front; if its buffer is detached → TypeError.
///        InvalidateBYOBRequest; firstPending.buffer = TransferArrayBuffer(firstPending.buffer).
///        if firstPending.readerType is "none":
///           EnqueueDetachedPullIntoToQueue(controller, firstPending).
///   6. If ReadableStreamHasDefaultReader:
///        a. ProcessReadRequestsUsingQueue. (drain controller queue into
///           outstanding default-reader read requests.)
///        b. If GetNumReadRequests == 0:
///             EnqueueChunkToQueue(controller, transferredBuffer, byteOffset, byteLength).
///           Else:
///             debug_assert!(queue is empty).
///             debug_assert!(no pendingPullIntos with readerType == "none").
///             let view = Uint8Array(transferredBuffer, byteOffset, byteLength);
///             FulfillReadRequest(stream, view, false).
///   7. Else if HasBYOBReader:
///        EnqueueChunkToQueue, then ProcessPullIntoDescriptorsUsingQueue.
///        (The BYOB reads expect descriptors filled from the queue.)
///   8. Else:
///        EnqueueChunkToQueue.
///   9. CallPullIfNeeded.
pub fn readable_byte_stream_controller_enqueue<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller: v8::Local<v8::Object>,
    chunk_view: v8::Local<v8::ArrayBufferView>,
) -> Result<(), v8::Global<v8::Value>> {
    let close_requested =
        with_controller_state(scope, controller, |s| s.close_requested.get()).unwrap_or(true);
    let stream = stream_obj(scope, controller).ok_or_else(|| make_type_error_g(scope, "controller has no stream"))?;
    let st = with_rs_state(scope, stream, |s| s.state.get())
        .ok_or_else(|| make_type_error_g(scope, "stream has no state"))?;
    if close_requested || st != StreamState::Readable {
        return Ok(());
    }

    let byte_offset = chunk_view.byte_offset() as u64;
    let byte_length = chunk_view.byte_length() as u64;
    let buffer = chunk_view
        .buffer(scope)
        .ok_or_else(|| make_type_error_g(scope, "chunk view has no buffer"))?;

    // Detached check.
    if is_detached_buffer(buffer) {
        return Err(make_type_error_g(scope, "chunk's buffer is detached"));
    }
    if !can_transfer_array_buffer(buffer) {
        return Err(make_type_error_g(
            scope,
            "chunk's buffer cannot be transferred",
        ));
    }

    // 4. transferredBuffer = TransferArrayBuffer(buffer).
    let transferred = transfer_array_buffer(scope, buffer);
    let transferred_g = v8::Global::new(scope, transferred);
    let buffer_byte_length = transferred.byte_length() as u64;

    // 5. If pendingPullIntos non-empty: refresh the first descriptor.
    let needs_first_refresh = with_controller_state(scope, controller, |s| {
        !s.pending_pull_intos.borrow().is_empty()
    })
    .unwrap_or(false);
    if needs_first_refresh {
        // Detached check on the first descriptor's buffer.
        let first_buf = with_controller_state(scope, controller, |s| {
            s.pending_pull_intos.borrow().front().map(|d| d.buffer.clone())
        })
        .flatten();
        if let Some(first_buf_g) = first_buf {
            let first_buf_l = v8::Local::new(scope, &first_buf_g);
            if is_detached_buffer(first_buf_l) {
                return Err(make_type_error_g(
                    scope,
                    "pending pullInto buffer is detached",
                ));
            }
            readable_byte_stream_controller_invalidate_byob_request(scope, controller);
            // Re-transfer the first descriptor's buffer.
            let first_new = transfer_array_buffer(scope, first_buf_l);
            let first_new_g = v8::Global::new(scope, first_new);
            with_controller_state(scope, controller, |s| {
                if let Some(d) = s.pending_pull_intos.borrow_mut().front_mut() {
                    d.buffer = first_new_g;
                }
            });
            // If readerType is None, move the descriptor onto the queue.
            let is_none_type = with_controller_state(scope, controller, |s| {
                s.pending_pull_intos
                    .borrow()
                    .front()
                    .map(|d| d.reader_type == ReaderType::None)
                    .unwrap_or(false)
            })
            .unwrap_or(false);
            if is_none_type {
                readable_byte_stream_controller_enqueue_detached_pull_into_to_queue(
                    scope, controller,
                );
            }
        }
    }

    // 6/7/8. Branch on reader type.
    if algorithms::readable_stream_has_default_reader(scope, stream) {
        readable_byte_stream_controller_process_read_requests_using_queue(scope, controller);
        if algorithms::readable_stream_get_num_read_requests(scope, stream) == 0 {
            // No outstanding reads; just queue.
            debug_assert!(
                !crate::streams::readable_byob_reader::readable_stream_has_byob_reader(
                    scope, stream
                )
            );
            readable_byte_stream_controller_enqueue_chunk_to_queue(
                scope,
                controller,
                &transferred_g,
                byte_offset,
                byte_length,
            );
        } else {
            // Fast path: deliver directly to the front read request as
            // a Uint8Array view.
            debug_assert!(
                with_controller_state(scope, controller, |s| s.queue.is_empty()).unwrap_or(true)
            );
            // Per spec / ref impl: if pendingPullIntos is non-empty here,
            // the front MUST be readerType="default" (an auto-alloc
            // descriptor). Shift it off — it is being discarded in favour
            // of the fast path.
            let has_default_pending = with_controller_state(scope, controller, |s| {
                s.pending_pull_intos
                    .borrow()
                    .front()
                    .map(|d| d.reader_type == ReaderType::Default)
                    .unwrap_or(false)
            })
            .unwrap_or(false);
            if has_default_pending {
                with_controller_state(scope, controller, |s| {
                    s.pending_pull_intos.borrow_mut().pop_front();
                });
            }
            let view = v8::Uint8Array::new(
                scope,
                transferred,
                byte_offset as usize,
                byte_length as usize,
            )
            .ok_or_else(|| make_type_error_g(scope, "view alloc"))?;
            let chunk_v: v8::Local<v8::Value> = view.into();
            algorithms::readable_stream_fulfill_read_request(scope, stream, chunk_v, false);
        }
    } else if crate::streams::readable_byob_reader::readable_stream_has_byob_reader(scope, stream) {
        readable_byte_stream_controller_enqueue_chunk_to_queue(
            scope,
            controller,
            &transferred_g,
            byte_offset,
            byte_length,
        );
        readable_byte_stream_controller_process_pull_into_descriptors_using_queue(scope, controller);
    } else {
        debug_assert!(!algorithms::is_readable_stream_locked(scope, stream));
        readable_byte_stream_controller_enqueue_chunk_to_queue(
            scope,
            controller,
            &transferred_g,
            byte_offset,
            byte_length,
        );
    }

    let _ = buffer_byte_length; // suppress unused warning in some paths
    readable_byte_stream_controller_call_pull_if_needed(scope, controller);
    Ok(())
}

/// `ReadableByteStreamControllerEnqueueChunkToQueue(controller, buffer,
/// byteOffset, byteLength)` — §3.11.x.
pub fn readable_byte_stream_controller_enqueue_chunk_to_queue(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
    buffer: &v8::Global<v8::ArrayBuffer>,
    byte_offset: u64,
    byte_length: u64,
) {
    with_controller_state(scope, controller, |s| {
        s.queue.enqueue_byte_entry(ByteQueueEntry {
            buffer: buffer.clone(),
            byte_offset: byte_offset as usize,
            byte_length: byte_length as usize,
        });
    });
}

/// `ReadableByteStreamControllerEnqueueClonedChunkToQueue(controller,
/// buffer, byteOffset, byteLength)` — used by byte-tee branch[1] cloning.
/// Per spec we copy the bytes into a freshly-allocated ArrayBuffer.
pub fn readable_byte_stream_controller_enqueue_cloned_chunk_to_queue(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
    buffer: &v8::Global<v8::ArrayBuffer>,
    byte_offset: u64,
    byte_length: u64,
) -> Result<(), v8::Global<v8::Value>> {
    let src = v8::Local::new(scope, buffer);
    if is_detached_buffer(src) {
        return Err(make_type_error_g(scope, "source buffer is detached"));
    }
    let cloned = v8::ArrayBuffer::new(scope, byte_length as usize);
    let cloned_g = v8::Global::new(scope, cloned);
    let src_bs = src.get_backing_store();
    let dst_bs = cloned.get_backing_store();
    copy_data_block_bytes(&dst_bs, 0, &src_bs, byte_offset, byte_length);
    readable_byte_stream_controller_enqueue_chunk_to_queue(
        scope, controller, &cloned_g, 0, byte_length,
    );
    Ok(())
}

/// `ReadableByteStreamControllerEnqueueDetachedPullIntoToQueue` — §3.11.x.
/// Move the front pending pull-into onto the controller's queue (used
/// when the reader was released mid-fill — `readerType == None`).
/// Spec uses `EnqueueClonedChunkToQueue` (cloning bytes into a fresh
/// buffer) so the originally-buffered descriptor's ownership stays
/// distinct from the queue entry.
pub fn readable_byte_stream_controller_enqueue_detached_pull_into_to_queue(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) {
    let descriptor = with_controller_state(scope, controller, |s| {
        s.pending_pull_intos.borrow_mut().pop_front()
    })
    .flatten();
    let Some(d) = descriptor else {
        return;
    };
    debug_assert!(d.reader_type == ReaderType::None);
    if d.bytes_filled > 0 {
        let _ = readable_byte_stream_controller_enqueue_cloned_chunk_to_queue(
            scope,
            controller,
            &d.buffer,
            d.byte_offset,
            d.bytes_filled,
        );
    }
}

// ---------------------------------------------------------------------------
// Fill helpers
// ---------------------------------------------------------------------------

/// `ReadableByteStreamControllerFillHeadPullIntoDescriptor(controller,
/// size, descriptor)` — spec §3.11.x. Update bytes_filled in place.
///
/// Spec asserts that exactly one of (a) the head reader_type is None, or
/// (b) the stream has a default reader, or (c) the stream has a BYOB
/// reader. We only debug_assert that pendingPullIntos is non-empty here
/// — the caller invariants are enforced by the spec algorithms.
pub fn readable_byte_stream_controller_fill_head_pull_into_descriptor(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
    size: u64,
) {
    debug_assert!(
        !with_controller_state(scope, controller, |s| s.pending_pull_intos.borrow().is_empty())
            .unwrap_or(true)
    );
    readable_byte_stream_controller_invalidate_byob_request(scope, controller);
    with_controller_state(scope, controller, |s| {
        if let Some(d) = s.pending_pull_intos.borrow_mut().front_mut() {
            d.bytes_filled += size;
        }
    });
}

/// `ReadableByteStreamControllerFillPullIntoDescriptorFromQueue(
/// controller, descriptor)` — spec §3.11.x. Returns true if descriptor's
/// minimumFill is now satisfied AND we should commit, false otherwise.
///
/// The function copies bytes from the controller's queue into the
/// descriptor's buffer, popping queue entries as fully consumed.
/// Per spec, each copy iteration calls FillHeadPullIntoDescriptor
/// (which invalidates the byob_request and bumps bytes_filled).
pub fn readable_byte_stream_controller_fill_pull_into_descriptor_from_queue(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
    descriptor: &mut PullIntoDescriptor,
) -> bool {
    let max_bytes_to_copy = std::cmp::min(
        with_controller_state(scope, controller, |s| s.queue.total_size() as u64).unwrap_or(0),
        descriptor.byte_length - descriptor.bytes_filled,
    );
    let max_bytes_filled = descriptor.bytes_filled + max_bytes_to_copy;
    let mut total_bytes_to_copy_remaining = max_bytes_to_copy;
    let mut ready = false;

    // Align bytes to descriptor.element_size.
    let remainder_bytes = max_bytes_filled % descriptor.element_size;
    let max_aligned_bytes = max_bytes_filled - remainder_bytes;
    if max_aligned_bytes >= descriptor.minimum_fill {
        total_bytes_to_copy_remaining = max_aligned_bytes - descriptor.bytes_filled;
        ready = true;
    }

    let dst_buf_l = v8::Local::new(scope, &descriptor.buffer);
    let dst_bs = dst_buf_l.get_backing_store();

    while total_bytes_to_copy_remaining > 0 {
        let head = with_controller_state(scope, controller, |s| {
            s.queue
                .drain_into_first()
                .map(|h| (h.buffer, h.byte_offset, h.byte_length))
        })
        .flatten();
        let Some((src_buf_g, src_off, src_len)) = head else {
            break;
        };
        let src_buf_l = v8::Local::new(scope, &src_buf_g);
        let bytes_to_copy = std::cmp::min(total_bytes_to_copy_remaining, src_len as u64);
        let dst_off = descriptor.byte_offset + descriptor.bytes_filled;
        let src_bs = src_buf_l.get_backing_store();
        copy_data_block_bytes(&dst_bs, dst_off, &src_bs, src_off as u64, bytes_to_copy);
        if (src_len as u64) == bytes_to_copy {
            // Whole entry consumed — already popped by drain_into_first.
        } else {
            // Partial — push back the remainder by re-inserting an entry.
            let new_off = src_off as u64 + bytes_to_copy;
            let new_len = src_len as u64 - bytes_to_copy;
            with_controller_state(scope, controller, |s| {
                s.queue.push_front_byte_entry(ByteQueueEntry {
                    buffer: src_buf_g.clone(),
                    byte_offset: new_off as usize,
                    byte_length: new_len as usize,
                });
            });
        }
        // Spec: FillHeadPullIntoDescriptor invalidates byob_request and
        // bumps bytes_filled. We bump locally (the caller of
        // process_pull_into_descriptors_using_queue is responsible for
        // syncing back to the front-of-list). Invalidate happens here
        // so successive byobRequest reads during multi-chunk fills see
        // a fresh request reflecting the current bytes_filled.
        descriptor.bytes_filled += bytes_to_copy;
        readable_byte_stream_controller_invalidate_byob_request(scope, controller);
        total_bytes_to_copy_remaining -= bytes_to_copy;
    }

    if !ready {
        debug_assert!(
            with_controller_state(scope, controller, |s| s.queue.total_size()).unwrap_or(0.0)
                == 0.0
        );
        debug_assert!(descriptor.bytes_filled > 0);
        debug_assert!(descriptor.bytes_filled < descriptor.minimum_fill);
    }
    ready
}

/// `ReadableByteStreamControllerFillReadRequestFromQueue(controller,
/// readRequest)` — spec §3.11.x. Pops queue head + delivers as Uint8Array
/// to the read request. (Used by default-reader paths when we have queue
/// bytes to consume.)
pub fn readable_byte_stream_controller_fill_read_request_from_queue(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
    request: ReadRequest,
) {
    debug_assert!(
        with_controller_state(scope, controller, |s| s.queue.total_size()).unwrap_or(0.0) > 0.0
    );
    let head = with_controller_state(scope, controller, |s| s.queue.dequeue_byte_entry()).flatten();
    let Some(entry) = head else {
        return;
    };
    readable_byte_stream_controller_handle_queue_drain(scope, controller);
    let buf_l = v8::Local::new(scope, &entry.buffer);
    let view =
        match v8::Uint8Array::new(scope, buf_l, entry.byte_offset, entry.byte_length) {
            Some(v) => v,
            None => return,
        };
    let chunk: v8::Local<v8::Value> = view.into();
    fulfill_read_request_chunk(scope, request, chunk);
}

/// `ReadableByteStreamControllerProcessReadRequestsUsingQueue(controller)` — §3.11.x.
pub fn readable_byte_stream_controller_process_read_requests_using_queue(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) {
    let stream = match stream_obj(scope, controller) {
        Some(s) => s,
        None => return,
    };
    debug_assert!(algorithms::readable_stream_has_default_reader(scope, stream));
    while algorithms::readable_stream_get_num_read_requests(scope, stream) > 0 {
        let total = with_controller_state(scope, controller, |s| s.queue.total_size())
            .unwrap_or(0.0);
        if total <= 0.0 {
            return;
        }
        let req = crate::streams::readable_default_reader::pop_front_read_request(scope, stream);
        let Some(req) = req else {
            return;
        };
        readable_byte_stream_controller_fill_read_request_from_queue(scope, controller, req);
    }
}

/// `ReadableByteStreamControllerProcessPullIntoDescriptorsUsingQueue(
/// controller)` — spec §3.11.x. Returns the list of filled descriptors.
/// The caller commits each of them into BYOB read requests via
/// CommitPullIntoDescriptor.
///
/// Reference-impl pattern: PEEK the front descriptor (don't pop),
/// fill in place, then ShiftPendingPullInto only when ready. This
/// matters because FillPullIntoDescriptorFromQueue calls
/// FillHeadPullIntoDescriptor which asserts pendingPullIntos non-empty
/// and calls InvalidateBYOBRequest per copy.
pub fn readable_byte_stream_controller_process_pull_into_descriptors_using_queue(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) -> Vec<PullIntoDescriptor> {
    debug_assert!(
        !with_controller_state(scope, controller, |s| s.close_requested.get()).unwrap_or(true)
    );
    let mut filled: Vec<PullIntoDescriptor> = Vec::new();
    loop {
        // Stop if no descriptors or no bytes to consume.
        let pending_empty = with_controller_state(scope, controller, |s| {
            s.pending_pull_intos.borrow().is_empty()
        })
        .unwrap_or(true);
        if pending_empty {
            break;
        }
        let q_total = with_controller_state(scope, controller, |s| s.queue.total_size())
            .unwrap_or(0.0);
        if q_total == 0.0 {
            break;
        }
        // PEEK the front; FillPullIntoDescriptorFromQueue modifies
        // bytes_filled in place via the borrow-mut path inside the fill
        // helper (we can't pass &mut while peeking without splitting).
        // Snapshot then call the fill function on a temporary local.
        let mut local = match with_controller_state(scope, controller, |s| {
            s.pending_pull_intos.borrow().front().map(clone_descriptor)
        })
        .flatten()
        {
            Some(d) => d,
            None => break,
        };
        let ready = readable_byte_stream_controller_fill_pull_into_descriptor_from_queue(
            scope,
            controller,
            &mut local,
        );
        // Persist the (possibly partial) bytes_filled back to the front
        // descriptor in pendingPullIntos.
        with_controller_state(scope, controller, |s| {
            if let Some(d) = s.pending_pull_intos.borrow_mut().front_mut() {
                d.bytes_filled = local.bytes_filled;
                d.buffer = local.buffer.clone();
            }
        });
        if !ready {
            // Queue exhausted before reaching minimumFill; descriptor
            // stays at front.
            break;
        }
        // ShiftPendingPullInto + record for commit.
        let popped = with_controller_state(scope, controller, |s| {
            s.pending_pull_intos.borrow_mut().pop_front()
        })
        .flatten();
        if let Some(d) = popped {
            filled.push(d);
        }
    }
    // Commit each filled descriptor on the BYOB reader's read-into queue.
    let stream = match stream_obj(scope, controller) {
        Some(s) => s,
        None => return filled,
    };
    for d in &filled {
        readable_byte_stream_controller_commit_pull_into_descriptor(scope, stream, d);
    }
    filled
}

/// Make a fresh `PullIntoDescriptor` with all the same fields as `src`.
/// Used to peek-fill in place: we can't hold a `&mut` into the
/// pending_pull_intos vec across the fill call (which itself reaches
/// into controller state via with_controller_state), so we work on a
/// local clone and write back.
fn clone_descriptor(src: &PullIntoDescriptor) -> PullIntoDescriptor {
    PullIntoDescriptor {
        buffer: src.buffer.clone(),
        buffer_byte_length: src.buffer_byte_length,
        byte_offset: src.byte_offset,
        byte_length: src.byte_length,
        bytes_filled: src.bytes_filled,
        minimum_fill: src.minimum_fill,
        element_size: src.element_size,
        view_constructor: src.view_constructor,
        reader_type: src.reader_type,
    }
}

/// `ReadableByteStreamControllerCommitPullIntoDescriptor(stream,
/// descriptor)` — spec §3.11.x. Constructs the view and fulfills the
/// front read-into request on the BYOB reader.
pub fn readable_byte_stream_controller_commit_pull_into_descriptor(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
    descriptor: &PullIntoDescriptor,
) {
    debug_assert!(
        with_rs_state(scope, stream, |s| s.state.get()) != Some(StreamState::Errored)
    );
    debug_assert!(descriptor.reader_type != ReaderType::None);
    let done = with_rs_state(scope, stream, |s| s.state.get()) == Some(StreamState::Closed);
    let view = build_view_from_descriptor(scope, descriptor);
    let view_v: v8::Local<v8::Value> = match view {
        Some(v) => v.into(),
        None => v8::undefined(scope).into(),
    };
    if descriptor.reader_type == ReaderType::Default {
        // Should never happen for read-into requests, but spec accepts.
        algorithms::readable_stream_fulfill_read_request(scope, stream, view_v, done);
    } else {
        debug_assert!(descriptor.reader_type == ReaderType::Byob);
        crate::streams::readable_byob_reader::readable_stream_fulfill_read_into_request(
            scope, stream, view_v, done,
        );
    }
}

/// `ReadableByteStreamControllerConvertPullIntoDescriptor(descriptor)` —
/// spec §3.11.x. Spec calls TransferArrayBuffer on the descriptor's
/// buffer before constructing the view (so the view the consumer
/// receives owns a fresh ArrayBuffer; the descriptor's local copy is
/// detached). Then constructs `new viewConstructor(transferred,
/// byteOffset, bytesFilled / elementSize)`.
fn build_view_from_descriptor<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    descriptor: &PullIntoDescriptor,
) -> Option<v8::Local<'s, v8::ArrayBufferView>> {
    let buf_l = v8::Local::new(scope, &descriptor.buffer);
    // Spec: TransferArrayBuffer on the descriptor's buffer.
    let transferred = transfer_array_buffer(scope, buf_l);
    descriptor.view_constructor.new_view(
        scope,
        transferred,
        descriptor.byte_offset as usize,
        descriptor.bytes_filled as usize,
    )
}

// ---------------------------------------------------------------------------
// PullInto (BYOB read entry point)
// ---------------------------------------------------------------------------

/// `ReadableByteStreamControllerPullInto(controller, view, min, readIntoRequest)`
/// — spec §3.11.x. Append a descriptor to pendingPullIntos and either
/// fulfill from queue or call pull algorithm.
///
/// `view`'s buffer is detached on success; spec `read({min})` validates
/// `min > 0` and `min <= view.byteLength / element_size` BEFORE calling
/// PullInto.
pub fn readable_byte_stream_controller_pull_into<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller: v8::Local<v8::Object>,
    view: v8::Local<v8::ArrayBufferView>,
    minimum_fill: u64,
    read_into_request: crate::streams::readable_byob_reader::ReadIntoRequest,
) {
    let Some(view_ctor) = ViewConstructor::from_view(view) else {
        let exc = make_type_error_g(scope, "Unrecognized view constructor");
        let exc_l = v8::Local::new(scope, &exc);
        crate::streams::readable_byob_reader::error_read_into_request(
            scope,
            read_into_request,
            exc_l,
        );
        return;
    };
    let element_size = view_ctor.element_size();
    let byte_offset = view.byte_offset() as u64;
    let byte_length = view.byte_length() as u64;
    let buffer = match view.buffer(scope) {
        Some(b) => b,
        None => {
            let exc = make_type_error_g(scope, "view has no buffer");
            let exc_l = v8::Local::new(scope, &exc);
            crate::streams::readable_byob_reader::error_read_into_request(
                scope,
                read_into_request,
                exc_l,
            );
            return;
        }
    };
    let buffer_byte_length = buffer.byte_length() as u64;

    if !can_transfer_array_buffer(buffer) {
        // Detached or shared.
        let exc = make_type_error_g(scope, "view's buffer is detached or non-transferable");
        let exc_l = v8::Local::new(scope, &exc);
        crate::streams::readable_byob_reader::error_read_into_request(
            scope,
            read_into_request,
            exc_l,
        );
        return;
    }

    let stream = match stream_obj(scope, controller) {
        Some(s) => s,
        None => return,
    };
    let reader_type = if crate::streams::readable_byob_reader::readable_stream_has_byob_reader(
        scope, stream,
    ) {
        ReaderType::Byob
    } else {
        // Should be reached only when the spec routes a default reader's
        // read through the byte controller's PullSteps with an auto-alloc
        // buffer — the descriptor's readerType is "default". This pull_into
        // entry point is BYOB-only; default-path is the auto-alloc fast path
        // which goes through enqueue + ProcessReadRequestsUsingQueue.
        ReaderType::Byob
    };

    // Transfer the buffer.
    let transferred = transfer_array_buffer(scope, buffer);
    let transferred_g = v8::Global::new(scope, transferred);

    let mut descriptor = PullIntoDescriptor {
        buffer: transferred_g,
        buffer_byte_length,
        byte_offset,
        byte_length,
        bytes_filled: 0,
        minimum_fill,
        element_size,
        view_constructor: view_ctor,
        reader_type,
    };

    // If pending pull-intos non-empty, just queue. The current head will
    // fill first.
    let already_pending = with_controller_state(scope, controller, |s| {
        !s.pending_pull_intos.borrow().is_empty()
    })
    .unwrap_or(false);
    if already_pending {
        with_controller_state(scope, controller, |s| {
            s.pending_pull_intos.borrow_mut().push_back(descriptor);
        });
        crate::streams::readable_byob_reader::add_read_into_request(scope, stream, read_into_request);
        return;
    }

    // Stream closed → commit a zero-byte view and resolve done=true.
    let st = with_rs_state(scope, stream, |s| s.state.get()).unwrap_or(StreamState::Closed);
    if st == StreamState::Closed {
        // Build a view of length 0 over the transferred buffer.
        let buf_l = v8::Local::new(scope, &descriptor.buffer);
        let zero_view = descriptor.view_constructor.new_view(
            scope,
            buf_l,
            descriptor.byte_offset as usize,
            0,
        );
        let v: v8::Local<v8::Value> = match zero_view {
            Some(v) => v.into(),
            None => v8::undefined(scope).into(),
        };
        crate::streams::readable_byob_reader::resolve_read_into_request_done(
            scope,
            read_into_request,
            v,
        );
        return;
    }

    // If queue has bytes, try to fill from queue.
    let q_total = with_controller_state(scope, controller, |s| s.queue.total_size()).unwrap_or(0.0);
    if q_total > 0.0 {
        let ready =
            readable_byte_stream_controller_fill_pull_into_descriptor_from_queue(
                scope,
                controller,
                &mut descriptor,
            );
        if ready {
            // Spec order:
            //   1. Build the view (ConvertPullIntoDescriptor — no state check).
            //   2. HandleQueueDrain (may transition state to closed if queue
            //      is empty AND closeRequested).
            //   3. Resolve the read with {value: view, done: false}.
            // We do NOT route through CommitPullIntoDescriptor here because
            // its done-flag depends on stream state; in this fast path we
            // know the descriptor was filled from the queue (so done=false
            // even if the stream subsequently closes).
            let view = build_view_from_descriptor(scope, &descriptor);
            readable_byte_stream_controller_handle_queue_drain(scope, controller);
            let view_v: v8::Local<v8::Value> = match view {
                Some(v) => v.into(),
                None => v8::undefined(scope).into(),
            };
            // Deliver the chunk to the read-into request directly. We
            // pass `done=false` because the descriptor was filled from
            // the queue.
            crate::streams::readable_byob_reader::resolve_read_into_request_chunk(
                scope,
                read_into_request,
                view_v,
            );
            return;
        }
        // Not ready; if closeRequested, error and return.
        if with_controller_state(scope, controller, |s| s.close_requested.get())
            .unwrap_or(false)
        {
            let msg = v8::String::new(
                scope,
                "Insufficient bytes to fill elements in the given buffer",
            )
            .unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            readable_byte_stream_controller_error(scope, controller, exc);
            crate::streams::readable_byob_reader::error_read_into_request(
                scope,
                read_into_request,
                exc,
            );
            return;
        }
    }

    // Else: queue partial fill; push descriptor onto pending and call pull.
    with_controller_state(scope, controller, |s| {
        s.pending_pull_intos.borrow_mut().push_back(descriptor);
    });
    crate::streams::readable_byob_reader::add_read_into_request(scope, stream, read_into_request);
    readable_byte_stream_controller_call_pull_if_needed(scope, controller);
}

// ---------------------------------------------------------------------------
// Respond paths
// ---------------------------------------------------------------------------

/// `ReadableByteStreamControllerRespond(controller, bytesWritten)` —
/// spec §3.11.x. Gate on stream `[[state]]`, not on
/// controller `closeRequested`. After `controller.close()` with non-empty
/// queue, `closeRequested == true` but state is still `readable` —
/// `respond(0)` MUST throw TypeError in that window.
pub fn readable_byte_stream_controller_respond<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller: v8::Local<v8::Object>,
    bytes_written: u64,
) -> Result<(), v8::Global<v8::Value>> {
    debug_assert!(
        !with_controller_state(scope, controller, |s| s.pending_pull_intos.borrow().is_empty())
            .unwrap_or(true)
    );
    let stream = stream_obj(scope, controller)
        .ok_or_else(|| make_type_error_g(scope, "controller has no stream"))?;
    let stream_state = with_rs_state(scope, stream, |s| s.state.get())
        .ok_or_else(|| make_type_error_g(scope, "stream has no state"))?;

    // Gate on stream state.
    if stream_state == StreamState::Closed {
        if bytes_written != 0 {
            return Err(make_type_error_g(
                scope,
                "bytesWritten must be 0 when calling respond() on a closed stream",
            ));
        }
    } else {
        debug_assert_eq!(stream_state, StreamState::Readable);
        if bytes_written == 0 {
            return Err(make_type_error_g(
                scope,
                "bytesWritten must be greater than 0 when calling respond() on a readable stream",
            ));
        }
        // Bounds check: bytesFilled + bytesWritten <= byteLength.
        let exceeds = with_controller_state(scope, controller, |s| {
            s.pending_pull_intos
                .borrow()
                .front()
                .map(|d| d.bytes_filled + bytes_written > d.byte_length)
                .unwrap_or(false)
        })
        .unwrap_or(false);
        if exceeds {
            return Err(make_range_error_g(scope, "bytesWritten out of range"));
        }
    }

    // Detached check on first descriptor's buffer.
    let first_buf_g = with_controller_state(scope, controller, |s| {
        s.pending_pull_intos.borrow().front().map(|d| d.buffer.clone())
    })
    .flatten();
    let Some(first_buf_g) = first_buf_g else {
        return Err(make_type_error_g(scope, "no pending pull-into"));
    };
    let first_buf_l = v8::Local::new(scope, &first_buf_g);
    if is_detached_buffer(first_buf_l) {
        return Err(make_type_error_g(
            scope,
            "respond: descriptor's buffer is detached",
        ));
    }

    // Re-transfer first descriptor's buffer (per spec).
    let new_buf = transfer_array_buffer(scope, first_buf_l);
    let new_buf_g = v8::Global::new(scope, new_buf);
    with_controller_state(scope, controller, |s| {
        if let Some(d) = s.pending_pull_intos.borrow_mut().front_mut() {
            d.buffer = new_buf_g;
        }
    });

    readable_byte_stream_controller_respond_internal(scope, controller, bytes_written)
}

/// `ReadableByteStreamControllerRespondInternal` — dispatches to
/// `RespondInClosedState` or `RespondInReadableState` based on stream state.
pub fn readable_byte_stream_controller_respond_internal(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
    bytes_written: u64,
) -> Result<(), v8::Global<v8::Value>> {
    let stream = stream_obj(scope, controller)
        .ok_or_else(|| make_type_error_g(scope, "controller has no stream"))?;
    let st = with_rs_state(scope, stream, |s| s.state.get())
        .ok_or_else(|| make_type_error_g(scope, "stream has no state"))?;
    readable_byte_stream_controller_invalidate_byob_request(scope, controller);
    if st == StreamState::Closed {
        debug_assert_eq!(bytes_written, 0);
        readable_byte_stream_controller_respond_in_closed_state(scope, controller);
    } else {
        debug_assert!(bytes_written > 0);
        readable_byte_stream_controller_respond_in_readable_state(scope, controller, bytes_written);
    }
    readable_byte_stream_controller_call_pull_if_needed(scope, controller);
    Ok(())
}

/// `ReadableByteStreamControllerRespondInClosedState(controller, descriptor)`
/// — spec §3.11.x. The `descriptor` parameter is the FRONT of pendingPullIntos.
///
/// Spec steps:
///   1. assert descriptor.bytesFilled mod descriptor.elementSize == 0
///   2. If descriptor.readerType is "none":
///        ShiftPendingPullInto(controller)
///   3. stream = controller.[[stream]]
///   4. If ReadableStreamHasBYOBReader(stream):
///        While ReadableStreamGetNumReadIntoRequests(stream) > 0:
///          d = ShiftPendingPullInto(controller)
///          CommitPullIntoDescriptor(stream, d)
pub fn readable_byte_stream_controller_respond_in_closed_state(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
) {
    // Inspect the FRONT descriptor without popping yet.
    let front = with_controller_state(scope, controller, |s| {
        s.pending_pull_intos
            .borrow()
            .front()
            .map(|d| (d.bytes_filled, d.element_size, d.reader_type))
    })
    .flatten();
    let Some((bytes_filled, element_size, reader_type)) = front else {
        return;
    };
    debug_assert_eq!(bytes_filled % element_size, 0);

    if reader_type == ReaderType::None {
        // Step 2: pop and discard.
        with_controller_state(scope, controller, |s| {
            s.pending_pull_intos.borrow_mut().pop_front();
        });
    }

    // Step 4: drain BYOB read-into requests by shifting+committing.
    let stream = match stream_obj(scope, controller) {
        Some(s) => s,
        None => return,
    };
    if crate::streams::readable_byob_reader::readable_stream_has_byob_reader(scope, stream) {
        loop {
            let n = crate::streams::readable_byob_reader::readable_stream_get_num_read_into_requests(
                scope, stream,
            );
            if n == 0 {
                break;
            }
            let d = with_controller_state(scope, controller, |s| {
                s.pending_pull_intos.borrow_mut().pop_front()
            })
            .flatten();
            let Some(d) = d else { break };
            readable_byte_stream_controller_commit_pull_into_descriptor(scope, stream, &d);
        }
    }
}

/// `ReadableByteStreamControllerRespondInReadableState(controller,
/// bytesWritten, descriptor)` — spec §3.11.x.
///
/// Spec steps (the `descriptor` parameter is the front of pendingPullIntos):
///   1. assert descriptor.bytesFilled + bytesWritten <= descriptor.byteLength
///   2. FillHeadPullIntoDescriptor(controller, bytesWritten, descriptor)
///   3. If descriptor.readerType is "none":
///        a. If descriptor.bytesFilled > 0: EnqueueDetachedPullIntoToQueue(controller, descriptor)
///        b. Else: ShiftPendingPullInto(controller)
///        c. ProcessPullIntoDescriptorsUsingQueue(controller)
///        d. Return
///   4. If descriptor.bytesFilled < descriptor.minimumFill → return (wait)
///   5. ShiftPendingPullInto(controller)
///   6. remainderSize = descriptor.bytesFilled mod descriptor.elementSize
///   7. If remainderSize > 0:
///        a. end = descriptor.byteOffset + descriptor.bytesFilled
///        b. EnqueueClonedChunkToQueue(controller, descriptor.buffer, end-remainderSize, remainderSize)
///        c. descriptor.bytesFilled -= remainderSize
///   8. CommitPullIntoDescriptor(stream, descriptor)
///   9. ProcessPullIntoDescriptorsUsingQueue(controller)
pub fn readable_byte_stream_controller_respond_in_readable_state(
    scope: &mut v8::PinScope,
    controller: v8::Local<v8::Object>,
    bytes_written: u64,
) {
    // Snapshot front descriptor metadata.
    let front = with_controller_state(scope, controller, |s| {
        s.pending_pull_intos.borrow().front().map(|d| {
            (
                d.bytes_filled,
                d.minimum_fill,
                d.element_size,
                d.byte_length,
                d.reader_type,
            )
        })
    })
    .flatten();
    let Some((bytes_filled_pre, minimum_fill, element_size, byte_length, reader_type)) = front
    else {
        return;
    };
    debug_assert!(bytes_filled_pre + bytes_written <= byte_length);

    // Step 2: FillHeadPullIntoDescriptor — bumps bytes_filled by bytesWritten
    // (we already invalidated byob_request in respond_internal).
    with_controller_state(scope, controller, |s| {
        if let Some(d) = s.pending_pull_intos.borrow_mut().front_mut() {
            d.bytes_filled += bytes_written;
        }
    });
    let bytes_filled = bytes_filled_pre + bytes_written;

    // Step 3: readerType == "none".
    if reader_type == ReaderType::None {
        if bytes_filled > 0 {
            // EnqueueDetachedPullIntoToQueue pops the front descriptor and
            // moves its bytes into the queue.
            readable_byte_stream_controller_enqueue_detached_pull_into_to_queue(scope, controller);
        } else {
            // Empty descriptor; just pop.
            with_controller_state(scope, controller, |s| {
                s.pending_pull_intos.borrow_mut().pop_front();
            });
        }
        let _ = readable_byte_stream_controller_process_pull_into_descriptors_using_queue(
            scope, controller,
        );
        return;
    }

    // Step 4: not enough yet, wait for more.
    if bytes_filled < minimum_fill {
        return;
    }

    // Step 5: pop the descriptor.
    let mut descriptor = match with_controller_state(scope, controller, |s| {
        s.pending_pull_intos.borrow_mut().pop_front()
    })
    .flatten()
    {
        Some(d) => d,
        None => return,
    };

    // Steps 6-7: split off the trailing bytes that don't align to elementSize.
    let aligned_filled = (descriptor.bytes_filled / element_size) * element_size;
    let remainder = descriptor.bytes_filled - aligned_filled;
    if remainder > 0 {
        let end = descriptor.byte_offset + descriptor.bytes_filled;
        // Spec says EnqueueClonedChunkToQueue (clones the bytes — the
        // descriptor's buffer will be transferred to the consumer).
        let buf_g = descriptor.buffer.clone();
        let _ = readable_byte_stream_controller_enqueue_cloned_chunk_to_queue(
            scope,
            controller,
            &buf_g,
            end - remainder,
            remainder,
        );
        descriptor.bytes_filled = aligned_filled;
    }

    // Steps 8-9: commit + process subsequent descriptors.
    let stream = match stream_obj(scope, controller) {
        Some(s) => s,
        None => return,
    };
    readable_byte_stream_controller_commit_pull_into_descriptor(scope, stream, &descriptor);
    let _ = readable_byte_stream_controller_process_pull_into_descriptors_using_queue(
        scope, controller,
    );
}

/// `ReadableByteStreamControllerRespondWithNewView(controller, view)` —
/// spec §3.11.x. Validates `view`'s buffer + offset alignment then
/// proceeds through respond_internal.
pub fn readable_byte_stream_controller_respond_with_new_view<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller: v8::Local<v8::Object>,
    view: v8::Local<v8::ArrayBufferView>,
) -> Result<(), v8::Global<v8::Value>> {
    let stream = stream_obj(scope, controller)
        .ok_or_else(|| make_type_error_g(scope, "controller has no stream"))?;
    let st = with_rs_state(scope, stream, |s| s.state.get())
        .ok_or_else(|| make_type_error_g(scope, "stream has no state"))?;

    let view_byte_offset = view.byte_offset() as u64;
    let view_byte_length = view.byte_length() as u64;
    let view_buffer = view
        .buffer(scope)
        .ok_or_else(|| make_type_error_g(scope, "view has no buffer"))?;
    if is_detached_buffer(view_buffer) {
        return Err(make_type_error_g(
            scope,
            "respondWithNewView: view's buffer is detached",
        ));
    }
    if !can_transfer_array_buffer(view_buffer) {
        return Err(make_type_error_g(
            scope,
            "respondWithNewView: view's buffer cannot be transferred",
        ));
    }

    // Spec checks: view must point at the same byteOffset as the
    // descriptor, view.byteLength must be valid for the new fill amount,
    // view.buffer.byteLength must equal descriptor.buffer.byteLength.
    let first = with_controller_state(scope, controller, |s| {
        s.pending_pull_intos.borrow().front().map(|d| {
            (
                d.byte_offset,
                d.bytes_filled,
                d.byte_length,
                d.buffer_byte_length,
            )
        })
    })
    .flatten();
    let Some((d_off, d_filled, d_len, d_buf_len)) = first else {
        return Err(make_type_error_g(scope, "no pending pull-into"));
    };
    let view_buf_byte_length = view_buffer.byte_length() as u64;
    if st == StreamState::Closed {
        if view_byte_length != 0 {
            return Err(make_type_error_g(
                scope,
                "respondWithNewView on a closed stream: view byteLength must be 0",
            ));
        }
    } else {
        debug_assert_eq!(st, StreamState::Readable);
        if view_byte_length == 0 {
            return Err(make_type_error_g(
                scope,
                "respondWithNewView on a readable stream: view byteLength must be > 0",
            ));
        }
    }
    if d_off + d_filled != view_byte_offset {
        return Err(make_range_error_g(
            scope,
            "respondWithNewView: view byteOffset does not match descriptor",
        ));
    }
    if d_buf_len != view_buf_byte_length {
        return Err(make_range_error_g(
            scope,
            "respondWithNewView: view buffer byteLength does not match descriptor",
        ));
    }
    if d_filled + view_byte_length > d_len {
        return Err(make_range_error_g(scope, "respondWithNewView: out of range"));
    }

    // Replace the descriptor's buffer with view.buffer (transferred).
    let transferred = transfer_array_buffer(scope, view_buffer);
    let transferred_g = v8::Global::new(scope, transferred);
    with_controller_state(scope, controller, |s| {
        if let Some(d) = s.pending_pull_intos.borrow_mut().front_mut() {
            d.buffer = transferred_g;
        }
    });
    readable_byte_stream_controller_respond_internal(scope, controller, view_byte_length)
}

// ---------------------------------------------------------------------------
// GetBYOBRequest — lazily build / reuse the request wrapper
// ---------------------------------------------------------------------------

/// `ReadableByteStreamControllerGetBYOBRequest(controller)` — spec §3.11.x.
/// Returns the current BYOBRequest wrapper (creating a fresh one bound to
/// the front pendingPullInto if none exists).
pub fn readable_byte_stream_controller_get_byob_request<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    controller: v8::Local<v8::Object>,
) -> v8::Local<'s, v8::Value> {
    let existing = slots::read_slot(scope, controller, BYOB_REQUEST);
    if !existing.is_undefined() {
        return existing;
    }
    let head_view = with_controller_state(scope, controller, |s| {
        s.pending_pull_intos
            .borrow()
            .front()
            .map(|d| (d.buffer.clone(), d.byte_offset + d.bytes_filled, d.byte_length - d.bytes_filled))
    })
    .flatten();
    let Some((buf_g, off, len)) = head_view else {
        return v8::null(scope).into();
    };
    let buf_l = v8::Local::new(scope, &buf_g);
    let view = match v8::Uint8Array::new(scope, buf_l, off as usize, len as usize) {
        Some(v) => v,
        None => return v8::null(scope).into(),
    };
    let view_v: v8::Local<v8::ArrayBufferView> = view.into();
    let req = crate::streams::byob_request::build(scope, controller, view_v);
    slots::write_slot(scope, controller, BYOB_REQUEST, req.into());
    req.into()
}

// ---------------------------------------------------------------------------
// SetUp* helpers
// ---------------------------------------------------------------------------

/// `SetUpReadableByteStreamController(stream, controller, startAlgorithm,
///  pullAlgorithm, cancelAlgorithm, hwm, autoAllocateChunkSize)` — §3.11.x.
fn set_up_readable_byte_stream_controller(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
    start_algorithm: AlgorithmFn,
    pull_algorithm: AlgorithmFn,
    cancel_algorithm: AlgorithmFn,
    hwm: f64,
    auto_allocate_chunk_size: Option<u64>,
) -> Result<(), String> {
    let tmpl = controller_class_template(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let controller_obj = inst_tmpl
        .new_instance(scope)
        .ok_or_else(|| "alloc byte controller instance".to_string())?;

    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    controller_obj.set_prototype(scope, proto_v);

    let state = ByteControllerState::new(hwm, auto_allocate_chunk_size, pull_algorithm, cancel_algorithm);
    let boxed = Box::new(state);
    let raw_ptr = Box::into_raw(boxed);
    let raw_addr = raw_ptr as usize;
    let ext = v8::External::new(scope, raw_ptr as *mut std::ffi::c_void);
    controller_obj.set_internal_field(0, ext.into());
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        controller_obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut ByteControllerState));
        }),
    );
    std::mem::forget(weak);

    // Class tag for is_byte_controller.
    let tag = v8::Boolean::new(scope, true);
    slots::write_slot(scope, controller_obj, BC_TAG_SLOT, tag.into());

    // Wire bidirectional refs.
    slots::write_slot(scope, stream, CONTROLLER, controller_obj.into());
    slots::write_slot(scope, controller_obj, STREAM_OBJ_SLOT, stream.into());

    // Run startAlgorithm. For JS-defined start functions, surface
    // synchronous throws as construction-time errors (per spec — a
    // throwing start() makes the constructor throw). The
    // AlgorithmFn::invoke_with_controller helper wraps any throw in a
    // rejected Promise, so we have to peek the start function ourselves
    // to detect the synchronous-throw case.
    let start_promise = if let AlgorithmFn::Js {
        function,
        this_obj,
    } = &start_algorithm
    {
        let f = v8::Local::new(scope, function);
        let this = v8::Local::new(scope, this_obj);
        let outcome = {
            v8::tc_scope!(let tc, scope);
            let r = f.call(tc, this, &[controller_obj.into()]);
            if tc.has_caught() {
                let exc = tc.exception().map(|e| v8::Global::new(tc, e));
                Err(exc)
            } else {
                Ok(r.map(|v| v8::Global::new(tc, v)))
            }
        };
        match outcome {
            Err(Some(exc_g)) => {
                // Re-throw synchronously as the constructor's exception.
                scope.throw_exception(v8::Local::new(scope, &exc_g));
                return Err("start() threw".to_string());
            }
            Err(None) => {
                return Err("start() failed without an exception value".to_string());
            }
            Ok(None) => algorithms::resolved_undefined_promise(scope),
            Ok(Some(v_g)) => {
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
    } else {
        start_algorithm.invoke_with_controller(scope, controller_obj)
    };

    let controller_g = v8::Global::new(scope, controller_obj);
    let controller_g2 = controller_g.clone();
    promise_resolve::upon_promise(
        scope,
        start_promise,
        Some(Box::new(move |scope, _v| {
            let controller = v8::Local::new(scope, &controller_g);
            with_controller_state(scope, controller, |s| s.started.set(true));
            readable_byte_stream_controller_call_pull_if_needed(scope, controller);
        })),
        Some(Box::new(move |scope, reason| {
            let controller = v8::Local::new(scope, &controller_g2);
            readable_byte_stream_controller_error(scope, controller, reason);
        })),
    );

    Ok(())
}

/// `SetUpReadableByteStreamControllerFromUnderlyingSource(stream, source,
///  hwm)` — §3.11.x.
pub fn set_up_readable_byte_stream_controller_from_underlying_source(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
    underlying_source: v8::Local<v8::Value>,
    hwm: f64,
) -> Result<(), String> {
    // start, pull, cancel, autoAllocateChunkSize.
    let mut start_alg = AlgorithmFn::Noop;
    let mut pull_alg = AlgorithmFn::Noop;
    let mut cancel_alg = AlgorithmFn::Noop;
    let mut auto_alloc: Option<u64> = None;

    if let Ok(us_obj) = v8::Local::<v8::Object>::try_from(underlying_source) {
        // Pull out callbacks (same shape as default-controller path).
        let start_v = us_obj
            .get(scope, v8::String::new(scope, "start").unwrap().into())
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
        let pull_v = us_obj
            .get(scope, v8::String::new(scope, "pull").unwrap().into())
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
        let cancel_v = us_obj
            .get(scope, v8::String::new(scope, "cancel").unwrap().into())
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

        let auto_v = us_obj
            .get(
                scope,
                v8::String::new(scope, "autoAllocateChunkSize").unwrap().into(),
            )
            .unwrap_or_else(|| v8::undefined(scope).into());
        if !auto_v.is_undefined() {
            let n = auto_v
                .number_value(scope)
                .ok_or_else(|| "autoAllocateChunkSize must be a number".to_string())?;
            if !n.is_finite() || n <= 0.0 || n.fract() != 0.0 {
                return Err(
                    "autoAllocateChunkSize must be a positive integer".to_string(),
                );
            }
            auto_alloc = Some(n as u64);
        }
    }

    set_up_readable_byte_stream_controller(
        scope,
        stream,
        start_alg,
        pull_alg,
        cancel_alg,
        hwm,
        auto_alloc,
    )
}

// ---------------------------------------------------------------------------
// Internal methods (spec §3.7.6) — [[CancelSteps]], [[PullSteps]], [[ReleaseSteps]]
// ---------------------------------------------------------------------------

/// `[[CancelSteps]](reason)` — §3.7.6.1.
pub fn cancel_steps<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
    reason: v8::Local<'s, v8::Value>,
) -> v8::Local<'s, v8::Promise> {
    let controller_v = slots::read_slot(scope, stream, CONTROLLER);
    let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) else {
        return algorithms::resolved_undefined_promise(scope);
    };
    readable_byte_stream_controller_clear_pending_pull_intos(scope, controller);
    with_controller_state(scope, controller, |s| s.queue.reset_queue());

    // Snapshot cancel_algorithm, clear, then invoke.
    let snapshot = with_controller_state(scope, controller, |s| algorithm_snapshot(&s.cancel_algorithm))
        .flatten();
    readable_byte_stream_controller_clear_algorithms(scope, controller);
    let Some(snap) = snapshot else {
        return algorithms::resolved_undefined_promise(scope);
    };
    snap.invoke_with_reason(scope, reason)
}

/// `[[PullSteps]](readRequest)` — §3.7.6.2. Default reader hooks here:
/// if queue has bytes, deliver via FillReadRequestFromQueue. If
/// autoAllocateChunkSize is set, allocate a buffer + push descriptor.
/// Otherwise, queue the read request as usual.
pub fn pull_steps(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
    request: ReadRequest,
) {
    let controller_v = slots::read_slot(scope, stream, CONTROLLER);
    let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) else {
        return;
    };
    let q_total = with_controller_state(scope, controller, |s| s.queue.total_size())
        .unwrap_or(0.0);
    if q_total > 0.0 {
        debug_assert!(crate::streams::readable_default_reader::is_default_reader_attached(
            scope, stream
        ));
        readable_byte_stream_controller_fill_read_request_from_queue(scope, controller, request);
        return;
    }
    let auto_alloc =
        with_controller_state(scope, controller, |s| s.auto_allocate_chunk_size).flatten();
    if let Some(size) = auto_alloc {
        let buffer = v8::ArrayBuffer::new(scope, size as usize);
        let buffer_g = v8::Global::new(scope, buffer);
        let descriptor = PullIntoDescriptor {
            buffer: buffer_g,
            buffer_byte_length: size,
            byte_offset: 0,
            byte_length: size,
            bytes_filled: 0,
            minimum_fill: 1,
            element_size: 1,
            view_constructor: ViewConstructor::Uint8,
            reader_type: ReaderType::Default,
        };
        with_controller_state(scope, controller, |s| {
            s.pending_pull_intos.borrow_mut().push_back(descriptor);
        });
    }
    crate::streams::readable_default_reader::enqueue_read_request(scope, stream, request);
    readable_byte_stream_controller_call_pull_if_needed(scope, controller);
}

/// `[[ReleaseSteps]]()` — §3.7.6.3. If pendingPullIntos non-empty, mark
/// the front descriptor's readerType = "none" so subsequent reads fill
/// it but no commit happens.
pub fn release_steps(scope: &mut v8::PinScope, stream: v8::Local<v8::Object>) {
    let controller_v = slots::read_slot(scope, stream, CONTROLLER);
    let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) else {
        return;
    };
    with_controller_state(scope, controller, |s| {
        if let Some(d) = s.pending_pull_intos.borrow_mut().front_mut() {
            d.reader_type = ReaderType::None;
        }
    });
}

// ---------------------------------------------------------------------------
// Class template construction
// ---------------------------------------------------------------------------

fn controller_class_template<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::FunctionTemplate> {
    let ctor_tmpl = v8::FunctionTemplate::new(scope, illegal_constructor_callback);
    let class_name = v8::String::new(scope, "ReadableByteStreamController").unwrap();
    ctor_tmpl.set_class_name(class_name);
    ctor_tmpl
        .instance_template(scope)
        .set_internal_field_count(1);

    let proto = ctor_tmpl.prototype_template(scope);

    {
        let key = v8::String::new(scope, "byobRequest").unwrap();
        let getter_tmpl = v8::FunctionTemplate::new(scope, byob_request_getter_callback);
        proto.set_accessor_property(key.into(), Some(getter_tmpl.into()), None, v8::PropertyAttribute::NONE);
    }
    {
        let key = v8::String::new(scope, "desiredSize").unwrap();
        let getter_tmpl = v8::FunctionTemplate::new(scope, desired_size_getter_callback);
        proto.set_accessor_property(key.into(), Some(getter_tmpl.into()), None, v8::PropertyAttribute::NONE);
    }

    install_proto_method(scope, proto, "close", close_method_callback);
    install_proto_method(scope, proto, "enqueue", enqueue_method_callback);
    install_proto_method(scope, proto, "error", error_method_callback);

    let tag_sym = v8::Symbol::get_to_string_tag(scope);
    let tag_value = v8::String::new(scope, "ReadableByteStreamController").unwrap();
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
    let msg =
        v8::String::new(scope, "ReadableByteStreamController: illegal constructor").unwrap();
    let exc = v8::Exception::type_error(scope, msg);
    scope.throw_exception(exc);
}

// ---------------------------------------------------------------------------
// IDL methods
// ---------------------------------------------------------------------------

fn byob_request_getter_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    if !is_byte_controller(scope, this) {
        let msg = v8::String::new(scope, "byobRequest: receiver invalid").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    rv.set(readable_byte_stream_controller_get_byob_request(scope, this));
}

fn desired_size_getter_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    if !is_byte_controller(scope, this) {
        let msg = v8::String::new(scope, "desiredSize: receiver invalid").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    match readable_byte_stream_controller_get_desired_size(scope, this) {
        Some(n) => rv.set(v8::Number::new(scope, n).into()),
        None => rv.set(v8::null(scope).into()),
    }
}

fn close_method_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let this = args.this();
    if !is_byte_controller(scope, this) {
        let msg = v8::String::new(scope, "close: receiver invalid").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    let close_requested = with_controller_state(scope, this, |s| s.close_requested.get())
        .unwrap_or(true);
    if close_requested {
        let msg = v8::String::new(scope, "close: closeRequested already true").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    let stream = match stream_obj(scope, this) {
        Some(s) => s,
        None => return,
    };
    let st = match with_rs_state(scope, stream, |s| s.state.get()) {
        Some(s) => s,
        None => return,
    };
    if st != StreamState::Readable {
        let msg = v8::String::new(scope, "close: stream not readable").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    readable_byte_stream_controller_close(scope, this);
}

fn enqueue_method_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    _rv: v8::ReturnValue<'s>,
) {
    let this = args.this();
    if !is_byte_controller(scope, this) {
        let msg = v8::String::new(scope, "enqueue: receiver invalid").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    let chunk = args.get(0);
    // Per IDL: argument is ArrayBufferView; non-views throw TypeError.
    let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(chunk) else {
        let msg = v8::String::new(scope, "enqueue: chunk is not an ArrayBufferView").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    };
    if view.byte_length() == 0 {
        // Spec: empty view → TypeError (per WPT).
        let msg = v8::String::new(scope, "enqueue: chunk is an empty view").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    let close_requested =
        with_controller_state(scope, this, |s| s.close_requested.get()).unwrap_or(true);
    let stream = match stream_obj(scope, this) {
        Some(s) => s,
        None => return,
    };
    let st = match with_rs_state(scope, stream, |s| s.state.get()) {
        Some(s) => s,
        None => return,
    };
    if close_requested {
        let msg = v8::String::new(scope, "enqueue: closeRequested is true").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    if st != StreamState::Readable {
        let msg = v8::String::new(scope, "enqueue: stream not readable").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    if let Err(exc_g) = readable_byte_stream_controller_enqueue(scope, this, view) {
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
    if !is_byte_controller(scope, this) {
        let msg = v8::String::new(scope, "error: receiver invalid").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    let reason = args.get(0);
    readable_byte_stream_controller_error(scope, this, reason);
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn make_type_error_g<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    msg: &str,
) -> v8::Global<v8::Value> {
    let msg_v = v8::String::new(scope, msg).unwrap();
    let exc = v8::Exception::type_error(scope, msg_v);
    v8::Global::new(scope, exc)
}

fn make_range_error_g<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    msg: &str,
) -> v8::Global<v8::Value> {
    let msg_v = v8::String::new(scope, msg).unwrap();
    let exc = v8::Exception::range_error(scope, msg_v);
    v8::Global::new(scope, exc)
}

#[allow(dead_code)]
fn _unused_marker(_: &v8::Global<v8::Value>) {}

#[allow(dead_code)]
fn _unused_stored_error_marker() {
    let _ = STORED_ERROR;
}

#[allow(dead_code)]
fn _unused_native_marker(_: ReadRequestKind) {}

#[allow(dead_code)]
fn _unused_native_trait<T: ReadRequestNative>(_: T) {}

// ---------------------------------------------------------------------------
// Public install
// ---------------------------------------------------------------------------

pub fn install(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
    let tmpl = controller_class_template(scope);
    let class_fn = tmpl.get_function(scope).unwrap();
    let key = v8::String::new(scope, "ReadableByteStreamController").unwrap();
    global.set(scope, key.into(), class_fn.into());
}
