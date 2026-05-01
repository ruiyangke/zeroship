//! `ReadableByteStreamTee` — spec §3.5.3.
//!
//! Structurally similar to default tee (per-branch independent
//! ReadableStream wrappers, shared source-side reader), but:
//!
//! 1. Each branch is a byte stream with its own `ReadableByteStreamController`.
//! 2. The shared source-side reader switches modes based on what the
//!    branches want: a default reader by default; promoted to a BYOB
//!    reader when one branch's pull-into descriptor "leads" the other.
//! 3. Branch[1] copies the chunk via `EnqueueClonedChunkToQueue` when
//!    `cloneForBranch2 = true` (post-2023 spec change). Public `tee()` on
//!    a byte stream always passes `false`, so branch[0]/branch[1] receive
//!    the same buffer view (which is OK per spec because each side has
//!    its own queue and the bytes are read-only from JS POV).
//!
//! This implementation lands a SUBSET — it covers the default-reader path
//! (both branches read via default reader). The BYOB-promotion path is
//! marked TODO and skipped: when both branches issue BYOB reads
//! concurrently, the spec switches the source-side reader to a BYOB
//! reader and routes pull-intos through the source. We defer this to a
//! follow-up since it requires an additional `ReadableStreamBYOBReader`-
//! backed pull machinery wired through the source.
//!
//! Per CRITICAL #6 (streams round-1): per-branch queues; shared single
//! reader feeds both. There is no shared queue.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::streams::algorithms;
use crate::streams::promise_resolve;
use crate::streams::readable::{build_value_stream_wrapper_for_internal, with_rs_state};
use crate::streams::readable_byte_controller::{
    is_byte_controller, readable_byte_stream_controller_close,
    readable_byte_stream_controller_enqueue, readable_byte_stream_controller_enqueue_cloned_chunk_to_queue,
    readable_byte_stream_controller_error,
    readable_byte_stream_controller_process_read_requests_using_queue,
    set_up_readable_byte_stream_controller_from_underlying_source,
};
use crate::streams::readable_default_reader::{
    acquire_readable_stream_default_reader, ReadRequest, ReadRequestKind, ReadRequestNative,
};
use crate::streams::slots::{self, CLOSED_PROMISE, CONTROLLER};

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[allow(missing_debug_implementations)]
pub struct ByteTeeState {
    pub source: v8::Global<v8::Object>,
    pub reader: v8::Global<v8::Object>,
    pub branch1: RefCell<Option<v8::Global<v8::Object>>>,
    pub branch2: RefCell<Option<v8::Global<v8::Object>>>,
    pub reading: Cell<bool>,
    pub read_again_for_branch1: Cell<bool>,
    pub read_again_for_branch2: Cell<bool>,
    pub canceled1: Cell<bool>,
    pub canceled2: Cell<bool>,
    pub reason1: RefCell<Option<v8::Global<v8::Value>>>,
    pub reason2: RefCell<Option<v8::Global<v8::Value>>>,
    pub clone_for_branch2: bool,
    pub cancel_promise_resolver: v8::Global<v8::PromiseResolver>,
    pub cancel_promise: v8::Global<v8::Promise>,
}

// ---------------------------------------------------------------------------
// Public entrypoint — readable_byte_stream_tee
// ---------------------------------------------------------------------------

/// `ReadableByteStreamTee(stream, cloneForBranch2)` — spec §3.5.3.
///
/// Public `ReadableStream.prototype.tee()` for byte streams calls this
/// with `cloneForBranch2 = false`. The cross-piping/structuredClone
/// path (cloneForBranch2 = true) is reserved for future use.
pub fn readable_byte_stream_tee<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    source: v8::Local<v8::Object>,
    clone_for_branch2: bool,
) -> Result<[v8::Local<'s, v8::Object>; 2], String> {
    // Acquire a default reader on the source. Per spec §3.5.3, the
    // initial reader is a default reader; it can be swapped to a BYOB
    // reader on demand. We currently keep it as default for the v1
    // landing.
    let reader = acquire_readable_stream_default_reader(scope, source)
        .map_err(|e| format!("byte-tee: {e}"))?;

    let cancel_resolver = v8::PromiseResolver::new(scope).unwrap();
    let cancel_promise = cancel_resolver.get_promise(scope);

    let tee_state = Rc::new(ByteTeeState {
        source: v8::Global::new(scope, source),
        reader: v8::Global::new(scope, reader),
        branch1: RefCell::new(None),
        branch2: RefCell::new(None),
        reading: Cell::new(false),
        read_again_for_branch1: Cell::new(false),
        read_again_for_branch2: Cell::new(false),
        canceled1: Cell::new(false),
        canceled2: Cell::new(false),
        reason1: RefCell::new(None),
        reason2: RefCell::new(None),
        clone_for_branch2,
        cancel_promise_resolver: v8::Global::new(scope, cancel_resolver),
        cancel_promise: v8::Global::new(scope, cancel_promise),
    });

    let branch1 = build_branch_byte_stream(scope, &tee_state, BranchIdx::One)?;
    *tee_state.branch1.borrow_mut() = Some(v8::Global::new(scope, branch1));

    let branch2 = build_branch_byte_stream(scope, &tee_state, BranchIdx::Two)?;
    *tee_state.branch2.borrow_mut() = Some(v8::Global::new(scope, branch2));

    chain_reader_closed_rejection(scope, &tee_state);

    Ok([branch1, branch2])
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BranchIdx {
    One,
    Two,
}

fn build_branch_byte_stream<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    tee_state: &Rc<ByteTeeState>,
    idx: BranchIdx,
) -> Result<v8::Local<'s, v8::Object>, String> {
    // Build a JS underlyingSource = { type: "bytes", pull, cancel } whose
    // closures capture the shared ByteTeeState.
    let underlying = v8::Object::new(scope);
    {
        let key = v8::String::new(scope, "type").unwrap();
        let val = v8::String::new(scope, "bytes").unwrap();
        underlying.set(scope, key.into(), val.into());
    }
    {
        let pull_fn = build_pull_fn(scope, tee_state.clone(), idx);
        let key = v8::String::new(scope, "pull").unwrap();
        underlying.set(scope, key.into(), pull_fn.into());
    }
    {
        let cancel_fn = build_cancel_fn(scope, tee_state.clone(), idx);
        let key = v8::String::new(scope, "cancel").unwrap();
        underlying.set(scope, key.into(), cancel_fn.into());
    }

    let stream = build_value_stream_wrapper_for_internal(scope);
    set_up_readable_byte_stream_controller_from_underlying_source(
        scope,
        stream,
        underlying.into(),
        /* hwm */ 0.0,
    )
    .map_err(|e| format!("byte-tee: build branch: {e}"))?;
    Ok(stream)
}

// ---------------------------------------------------------------------------
// Pull function — shared across both branches via tee_state.reading flag
// ---------------------------------------------------------------------------

struct PullHolder {
    tee_state: Rc<ByteTeeState>,
    idx: BranchIdx,
}

fn build_pull_fn<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    tee_state: Rc<ByteTeeState>,
    idx: BranchIdx,
) -> v8::Local<'s, v8::Function> {
    let holder = Rc::new(PullHolder { tee_state, idx });
    let raw = Rc::into_raw(holder) as *mut std::ffi::c_void;
    let ext = v8::External::new(scope, raw);
    let tmpl = v8::FunctionTemplate::builder(pull_callback)
        .data(ext.into())
        .build(scope);
    tmpl.get_function(scope).unwrap()
}

fn pull_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'s>,
) {
    let data = args.data();
    let Ok(ext) = v8::Local::<v8::External>::try_from(data) else {
        return;
    };
    let raw = ext.value() as *const PullHolder;
    if raw.is_null() {
        return;
    }
    let holder: &PullHolder = unsafe { &*raw };
    let tee_state = holder.tee_state.clone();
    let idx = holder.idx;

    pull_algorithm(scope, &tee_state, idx);

    let p = algorithms::resolved_undefined_promise(scope);
    rv.set(p.into());
}

fn pull_algorithm(scope: &mut v8::PinScope, tee_state: &Rc<ByteTeeState>, idx: BranchIdx) {
    if tee_state.reading.get() {
        if idx == BranchIdx::One {
            tee_state.read_again_for_branch1.set(true);
        } else {
            tee_state.read_again_for_branch2.set(true);
        }
        return;
    }
    tee_state.reading.set(true);

    let reader_l = v8::Local::new(scope, &tee_state.reader);
    let source_l = v8::Local::new(scope, &tee_state.source);

    let request = ReadRequest {
        kind: ReadRequestKind::Native(Box::new(ByteTeeReadRequest {
            tee_state: tee_state.clone(),
        })),
    };
    crate::streams::readable_default_reader::readable_stream_default_reader_read(
        scope, reader_l, source_l, request,
    );
}

struct ByteTeeReadRequest {
    tee_state: Rc<ByteTeeState>,
}

impl ReadRequestNative for ByteTeeReadRequest {
    fn chunk_steps<'s>(
        self: Box<Self>,
        scope: &mut v8::PinScope<'s, '_>,
        chunk: v8::Local<'s, v8::Value>,
    ) {
        chunk_steps(scope, &self.tee_state, chunk);
    }

    fn close_steps(self: Box<Self>, scope: &mut v8::PinScope) {
        close_steps(scope, &self.tee_state);
    }

    fn error_steps<'s>(
        self: Box<Self>,
        _scope: &mut v8::PinScope<'s, '_>,
        _reason: v8::Local<'s, v8::Value>,
    ) {
        self.tee_state.reading.set(false);
    }
}

fn chunk_steps<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    tee_state: &Rc<ByteTeeState>,
    chunk: v8::Local<'s, v8::Value>,
) {
    // Per spec §3.5.3: enqueue the chunk into branch1's controller (and
    // optionally branch2's clone). Wrap in a microtask so source-side
    // synchronous errors win the race.
    let chunk_g = v8::Global::new(scope, chunk);
    let tee_state = tee_state.clone();
    promise_resolve::enqueue_microtask(scope, move |scope| {
        tee_state.read_again_for_branch1.set(false);
        tee_state.read_again_for_branch2.set(false);
        let chunk_l = v8::Local::new(scope, &chunk_g);

        // For byte streams in default-reader mode: enqueue clones
        // ALWAYS into branch2 (because branch1's enqueue
        // TransferArrayBuffer's the source buffer, leaving nothing for
        // branch2). The spec's `cloneForBranch2` flag covers the
        // structuredClone path; in our default-reader path we
        // structurally must clone for branch2 to keep both branches
        // observable. Branch1 gets the original transfer.
        let view_view = v8::Local::<v8::ArrayBufferView>::try_from(chunk_l).ok();
        // Capture buffer + offset/len BEFORE any enqueue (which detaches).
        let buf_off_len = view_view.and_then(|v| {
            v.buffer(scope).map(|b| {
                let g = v8::Global::new(scope, b);
                (g, v.byte_offset() as u64, v.byte_length() as u64)
            })
        });
        // Pre-clone for branch2 if both branches active (snapshot bytes
        // BEFORE the branch1 enqueue's TransferArrayBuffer detaches them).
        let cloned_for_branch2 =
            if !tee_state.canceled2.get() && !tee_state.canceled1.get() {
                buf_off_len.as_ref().map(|(buf_g, off, len)| {
                    let buf_l = v8::Local::new(scope, buf_g);
                    let cloned = v8::ArrayBuffer::new(scope, *len as usize);
                    let src_bs = buf_l.get_backing_store();
                    let dst_bs = cloned.get_backing_store();
                    crate::streams::pull_into::copy_data_block_bytes(
                        &dst_bs, 0, &src_bs, *off, *len,
                    );
                    let cloned_g = v8::Global::new(scope, cloned);
                    cloned_g
                })
            } else {
                None
            };

        // Branch 1: enqueue the original chunk (will TransferArrayBuffer).
        if !tee_state.canceled1.get() {
            if let Some(branch1_g) = tee_state.branch1.borrow().clone() {
                let branch1_l = v8::Local::new(scope, &branch1_g);
                let controller_v = slots::read_slot(scope, branch1_l, CONTROLLER);
                if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
                    if is_byte_controller(scope, controller) {
                        if let Some(view) = view_view {
                            let _ = readable_byte_stream_controller_enqueue(
                                scope, controller, view,
                            );
                        }
                    }
                }
            }
        }
        // Branch 2: enqueue from the pre-snapshotted clone (when both
        // branches active) or from the original buffer when branch1 is
        // cancelled.
        if !tee_state.canceled2.get() {
            if let Some(branch2_g) = tee_state.branch2.borrow().clone() {
                let branch2_l = v8::Local::new(scope, &branch2_g);
                let controller_v = slots::read_slot(scope, branch2_l, CONTROLLER);
                if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
                    if is_byte_controller(scope, controller) {
                        if let Some(cloned_g) = cloned_for_branch2 {
                            let len = buf_off_len.as_ref().map(|(_, _, l)| *l).unwrap_or(0);
                            let _ =
                                readable_byte_stream_controller_enqueue_cloned_chunk_to_queue(
                                    scope, controller, &cloned_g, 0, len,
                                );
                            // Drain into pending read requests / BYOB
                            // descriptors. The cloned-enqueue is just a
                            // queue insert; without a drain step pending
                            // read() promises stay un-resolved.
                            if algorithms::readable_stream_has_default_reader(
                                scope, branch2_l,
                            ) {
                                readable_byte_stream_controller_process_read_requests_using_queue(
                                    scope, controller,
                                );
                            }
                        } else if tee_state.canceled1.get() {
                            // Branch1 cancelled — branch2 takes the original.
                            if let Some(view) = view_view {
                                let _ = readable_byte_stream_controller_enqueue(
                                    scope, controller, view,
                                );
                            }
                        }
                    }
                }
            }
        }

        tee_state.reading.set(false);
        if tee_state.read_again_for_branch1.get() {
            pull_algorithm(scope, &tee_state, BranchIdx::One);
        } else if tee_state.read_again_for_branch2.get() {
            pull_algorithm(scope, &tee_state, BranchIdx::Two);
        }
    });
}

fn close_steps(scope: &mut v8::PinScope, tee_state: &Rc<ByteTeeState>) {
    tee_state.reading.set(false);
    if !tee_state.canceled1.get() {
        if let Some(branch1_g) = tee_state.branch1.borrow().clone() {
            let branch1_l = v8::Local::new(scope, &branch1_g);
            let controller_v = slots::read_slot(scope, branch1_l, CONTROLLER);
            if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
                if is_byte_controller(scope, controller) {
                    readable_byte_stream_controller_close(scope, controller);
                }
            }
        }
    }
    if !tee_state.canceled2.get() {
        if let Some(branch2_g) = tee_state.branch2.borrow().clone() {
            let branch2_l = v8::Local::new(scope, &branch2_g);
            let controller_v = slots::read_slot(scope, branch2_l, CONTROLLER);
            if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
                if is_byte_controller(scope, controller) {
                    readable_byte_stream_controller_close(scope, controller);
                }
            }
        }
    }
    if !tee_state.canceled1.get() || !tee_state.canceled2.get() {
        let resolver_l = v8::Local::new(scope, &tee_state.cancel_promise_resolver);
        let und = v8::undefined(scope);
        resolver_l.resolve(scope, und.into());
    }
}

// ---------------------------------------------------------------------------
// Cancel function — per-branch
// ---------------------------------------------------------------------------

struct CancelHolder {
    tee_state: Rc<ByteTeeState>,
    idx: BranchIdx,
}

fn build_cancel_fn<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    tee_state: Rc<ByteTeeState>,
    idx: BranchIdx,
) -> v8::Local<'s, v8::Function> {
    let holder = Rc::new(CancelHolder { tee_state, idx });
    let raw = Rc::into_raw(holder) as *mut std::ffi::c_void;
    let ext = v8::External::new(scope, raw);
    let tmpl = v8::FunctionTemplate::builder(cancel_callback)
        .data(ext.into())
        .build(scope);
    tmpl.get_function(scope).unwrap()
}

fn cancel_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'s>,
) {
    let data = args.data();
    let Ok(ext) = v8::Local::<v8::External>::try_from(data) else {
        return;
    };
    let raw = ext.value() as *const CancelHolder;
    if raw.is_null() {
        return;
    }
    let holder: &CancelHolder = unsafe { &*raw };
    let tee_state = holder.tee_state.clone();
    let idx = holder.idx;
    let reason = args.get(0);

    let p = cancel_algorithm(scope, &tee_state, idx, reason);
    rv.set(p.into());
}

fn cancel_algorithm<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    tee_state: &Rc<ByteTeeState>,
    idx: BranchIdx,
    reason: v8::Local<v8::Value>,
) -> v8::Local<'s, v8::Promise> {
    let reason_g = v8::Global::new(scope, reason);
    if idx == BranchIdx::One {
        tee_state.canceled1.set(true);
        *tee_state.reason1.borrow_mut() = Some(reason_g);
    } else {
        tee_state.canceled2.set(true);
        *tee_state.reason2.borrow_mut() = Some(reason_g);
    }

    if tee_state.canceled1.get() && tee_state.canceled2.get() {
        let r1_g = tee_state.reason1.borrow().clone();
        let r2_g = tee_state.reason2.borrow().clone();
        let arr = v8::Array::new(scope, 2);
        let r1_v: v8::Local<v8::Value> = match r1_g {
            Some(g) => v8::Local::new(scope, &g),
            None => v8::undefined(scope).into(),
        };
        let r2_v: v8::Local<v8::Value> = match r2_g {
            Some(g) => v8::Local::new(scope, &g),
            None => v8::undefined(scope).into(),
        };
        arr.set_index(scope, 0, r1_v);
        arr.set_index(scope, 1, r2_v);
        let composite: v8::Local<v8::Value> = arr.into();

        let source_l = v8::Local::new(scope, &tee_state.source);
        let cancel_result = algorithms::readable_stream_cancel(scope, source_l, composite);
        let resolver_l = v8::Local::new(scope, &tee_state.cancel_promise_resolver);
        resolver_l.resolve(scope, cancel_result.into());
    }
    v8::Local::new(scope, &tee_state.cancel_promise)
}

// ---------------------------------------------------------------------------
// chain_reader_closed_rejection — error both branches if reader.closed rejects
// ---------------------------------------------------------------------------

fn chain_reader_closed_rejection(scope: &mut v8::PinScope, tee_state: &Rc<ByteTeeState>) {
    let reader_l = v8::Local::new(scope, &tee_state.reader);
    let closed_v = slots::read_slot(scope, reader_l, CLOSED_PROMISE);
    let Ok(closed_p) = v8::Local::<v8::Promise>::try_from(closed_v) else {
        return;
    };
    let tee_state = tee_state.clone();
    promise_resolve::upon_promise(
        scope,
        closed_p,
        None,
        Some(Box::new(move |scope, reason| {
            if let Some(branch1_g) = tee_state.branch1.borrow().clone() {
                let branch1_l = v8::Local::new(scope, &branch1_g);
                let st = with_rs_state(scope, branch1_l, |s| s.state.get());
                if matches!(st, Some(crate::streams::readable::StreamState::Readable)) {
                    let controller_v = slots::read_slot(scope, branch1_l, CONTROLLER);
                    if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
                        if is_byte_controller(scope, controller) {
                            readable_byte_stream_controller_error(scope, controller, reason);
                        }
                    }
                }
            }
            if let Some(branch2_g) = tee_state.branch2.borrow().clone() {
                let branch2_l = v8::Local::new(scope, &branch2_g);
                let st = with_rs_state(scope, branch2_l, |s| s.state.get());
                if matches!(st, Some(crate::streams::readable::StreamState::Readable)) {
                    let controller_v = slots::read_slot(scope, branch2_l, CONTROLLER);
                    if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
                        if is_byte_controller(scope, controller) {
                            readable_byte_stream_controller_error(scope, controller, reason);
                        }
                    }
                }
            }
            if !tee_state.canceled1.get() || !tee_state.canceled2.get() {
                let resolver_l = v8::Local::new(scope, &tee_state.cancel_promise_resolver);
                let und = v8::undefined(scope);
                resolver_l.resolve(scope, und.into());
            }
        })),
    );
}
