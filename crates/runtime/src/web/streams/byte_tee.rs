//! `ReadableByteStreamTee` — spec §3.5.3.
//!
//! Per the WHATWG Streams reference implementation, the byte-stream tee
//! shares a single source-side `reader` whose mode (default vs BYOB)
//! switches based on which branch's controller currently has a pending
//! BYOB request.
//!
//! pull{1,2}Algorithm:
//!   1. If `reading` is set → mark readAgainForBranchN, return.
//!   2. Get branchN.[[controller]].byobRequest.
//!   3. If null → pullWithDefaultReader().
//!      Else → pullWithBYOBReader(byobRequest.view, forBranch2=N==2).
//!
//! pullWithDefaultReader:
//!   - If reader is BYOB → release + acquire fresh default reader.
//!   - read(): on chunk, microtask:
//!     - clone chunk for branch2 (if both active) — CloneAsUint8Array.
//!     - enqueue chunk1 into branch1, chunk2 into branch2 (if branch
//!       not cancelled).
//!     - if readAgainForBranch{1,2} → call pull{1,2}Algorithm.
//!
//! pullWithBYOBReader(view, forBranch2):
//!   - If reader is Default → release + acquire fresh BYOB reader.
//!   - read(view, {min: 1}): on chunk, microtask:
//!     - clone chunk for the OTHER branch (CloneAsUint8Array).
//!     - byobBranch.respondWithNewView(chunk).
//!     - otherBranch.enqueue(clonedChunk).
//!     - if readAgainForBranch{1,2} → call pull{1,2}Algorithm.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::streams::algorithms;
use crate::streams::promise_resolve;
use crate::streams::pull_into::copy_data_block_bytes;
use crate::streams::readable::{build_value_stream_wrapper_for_internal, with_rs_state, StreamState};
use crate::streams::readable_byte_controller::{
    is_byte_controller, readable_byte_stream_controller_close,
    readable_byte_stream_controller_enqueue,
    readable_byte_stream_controller_error,
    readable_byte_stream_controller_get_byob_request,
    readable_byte_stream_controller_respond,
    readable_byte_stream_controller_respond_with_new_view,
    set_up_readable_byte_stream_controller_from_underlying_source,
};
use crate::streams::readable_byob_reader::{
    acquire_readable_stream_byob_reader, readable_stream_byob_reader_release, ReadIntoRequest,
    ReadIntoRequestKind, ReadIntoRequestNative,
};
use crate::streams::readable_default_reader::{
    acquire_readable_stream_default_reader, readable_stream_default_reader_release, ReadRequest,
    ReadRequestKind, ReadRequestNative,
};
use crate::streams::slots::{self, CLOSED_PROMISE, CONTROLLER};

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[allow(missing_debug_implementations)]
pub struct ByteTeeState {
    pub source: v8::Global<v8::Object>,
    /// Current source-side reader. Starts as a default reader; can be
    /// swapped to BYOB on demand. Always present (never empty during
    /// the tee's lifetime).
    pub reader: RefCell<v8::Global<v8::Object>>,
    pub reader_is_byob: Cell<bool>,
    pub branch1: RefCell<Option<v8::Global<v8::Object>>>,
    pub branch2: RefCell<Option<v8::Global<v8::Object>>>,
    pub reading: Cell<bool>,
    pub read_again_for_branch1: Cell<bool>,
    pub read_again_for_branch2: Cell<bool>,
    pub canceled1: Cell<bool>,
    pub canceled2: Cell<bool>,
    pub reason1: RefCell<Option<v8::Global<v8::Value>>>,
    pub reason2: RefCell<Option<v8::Global<v8::Value>>>,
    pub cancel_promise_resolver: v8::Global<v8::PromiseResolver>,
    pub cancel_promise: v8::Global<v8::Promise>,
}

// ---------------------------------------------------------------------------
// Public entrypoint — readable_byte_stream_tee
// ---------------------------------------------------------------------------

/// `ReadableByteStreamTee(stream)` — spec §3.5.3.
pub fn readable_byte_stream_tee<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    source: v8::Local<v8::Object>,
    _clone_for_branch2: bool,
) -> Result<[v8::Local<'s, v8::Object>; 2], String> {
    // Initial reader: default (will swap to BYOB on demand).
    let reader = acquire_readable_stream_default_reader(scope, source)
        .map_err(|e| format!("byte-tee: {e}"))?;

    let cancel_resolver = v8::PromiseResolver::new(scope).unwrap();
    let cancel_promise = cancel_resolver.get_promise(scope);

    let tee_state = Rc::new(ByteTeeState {
        source: v8::Global::new(scope, source),
        reader: RefCell::new(v8::Global::new(scope, reader)),
        reader_is_byob: Cell::new(false),
        branch1: RefCell::new(None),
        branch2: RefCell::new(None),
        reading: Cell::new(false),
        read_again_for_branch1: Cell::new(false),
        read_again_for_branch2: Cell::new(false),
        canceled1: Cell::new(false),
        canceled2: Cell::new(false),
        reason1: RefCell::new(None),
        reason2: RefCell::new(None),
        cancel_promise_resolver: v8::Global::new(scope, cancel_resolver),
        cancel_promise: v8::Global::new(scope, cancel_promise),
    });

    let branch1 = build_branch_byte_stream(scope, &tee_state, BranchIdx::One)?;
    *tee_state.branch1.borrow_mut() = Some(v8::Global::new(scope, branch1));

    let branch2 = build_branch_byte_stream(scope, &tee_state, BranchIdx::Two)?;
    *tee_state.branch2.borrow_mut() = Some(v8::Global::new(scope, branch2));

    forward_reader_error(scope, &tee_state);

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
// Pull function — pulls dispatch to default-reader or BYOB-reader pull
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

    // Decide based on the BRANCH's byobRequest: if non-null, use BYOB reader
    // and read into its view. Else use default reader.
    let branch_g = match idx {
        BranchIdx::One => tee_state.branch1.borrow().clone(),
        BranchIdx::Two => tee_state.branch2.borrow().clone(),
    };
    let Some(branch_g) = branch_g else {
        return;
    };
    let branch_l = v8::Local::new(scope, &branch_g);
    let controller_v = slots::read_slot(scope, branch_l, CONTROLLER);
    let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) else {
        return;
    };
    let byob_req_v = readable_byte_stream_controller_get_byob_request(scope, controller);
    if byob_req_v.is_null() {
        pull_with_default_reader(scope, tee_state);
    } else {
        // Read the byobRequest's view, then call pullWithBYOBReader.
        let Ok(req_obj) = v8::Local::<v8::Object>::try_from(byob_req_v) else {
            // Shouldn't happen; fall back to default.
            pull_with_default_reader(scope, tee_state);
            return;
        };
        let view_key = v8::String::new(scope, "view").unwrap();
        let view_v = req_obj
            .get(scope, view_key.into())
            .unwrap_or_else(|| v8::undefined(scope).into());
        let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(view_v) else {
            pull_with_default_reader(scope, tee_state);
            return;
        };
        let for_branch2 = idx == BranchIdx::Two;
        pull_with_byob_reader(scope, tee_state, view, for_branch2);
    }
}

// ---------------------------------------------------------------------------
// pullWithDefaultReader / pullWithBYOBReader
// ---------------------------------------------------------------------------

fn pull_with_default_reader(scope: &mut v8::PinScope, tee_state: &Rc<ByteTeeState>) {
    // If current reader is BYOB, release + acquire default.
    if tee_state.reader_is_byob.get() {
        let cur_reader_g = tee_state.reader.borrow().clone();
        let cur_reader_l = v8::Local::new(scope, &cur_reader_g);
        readable_stream_byob_reader_release(scope, cur_reader_l);
        let source_l = v8::Local::new(scope, &tee_state.source);
        match acquire_readable_stream_default_reader(scope, source_l) {
            Ok(r) => {
                *tee_state.reader.borrow_mut() = v8::Global::new(scope, r);
                tee_state.reader_is_byob.set(false);
                forward_reader_error(scope, tee_state);
            }
            Err(_) => return,
        }
    }
    // Now the reader is default; issue read().
    let reader_g = tee_state.reader.borrow().clone();
    let reader_l = v8::Local::new(scope, &reader_g);
    let source_l = v8::Local::new(scope, &tee_state.source);

    let request = ReadRequest {
        kind: ReadRequestKind::Native(Box::new(ByteTeeDefaultReadRequest {
            tee_state: tee_state.clone(),
        })),
    };
    crate::streams::readable_default_reader::readable_stream_default_reader_read(
        scope, reader_l, source_l, request,
    );
}

fn pull_with_byob_reader<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    tee_state: &Rc<ByteTeeState>,
    view: v8::Local<'s, v8::ArrayBufferView>,
    for_branch2: bool,
) {
    // If current reader is default, release + acquire BYOB.
    if !tee_state.reader_is_byob.get() {
        let cur_reader_g = tee_state.reader.borrow().clone();
        let cur_reader_l = v8::Local::new(scope, &cur_reader_g);
        readable_stream_default_reader_release(scope, cur_reader_l);
        let source_l = v8::Local::new(scope, &tee_state.source);
        match acquire_readable_stream_byob_reader(scope, source_l) {
            Ok(r) => {
                *tee_state.reader.borrow_mut() = v8::Global::new(scope, r);
                tee_state.reader_is_byob.set(true);
                forward_reader_error(scope, tee_state);
            }
            Err(_) => return,
        }
    }
    // Issue BYOB read with min=1.
    let request = ReadIntoRequest {
        kind: ReadIntoRequestKind::Native(Box::new(ByteTeeBYOBReadRequest {
            tee_state: tee_state.clone(),
            for_branch2,
        })),
    };
    let elem_size = match crate::streams::pull_into::ViewConstructor::from_view(view) {
        Some(ctor) => ctor.element_size(),
        None => 1,
    };
    let minimum_fill = elem_size; // min=1 element
    let reader_g = tee_state.reader.borrow().clone();
    let reader_l = v8::Local::new(scope, &reader_g);
    let stream_v =
        crate::streams::slots::read_slot(scope, reader_l, crate::streams::slots::STREAM);
    let Ok(stream) = v8::Local::<v8::Object>::try_from(stream_v) else {
        return;
    };
    let controller_v = slots::read_slot(scope, stream, CONTROLLER);
    if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
        crate::streams::readable_byte_controller::readable_byte_stream_controller_pull_into(
            scope,
            controller,
            view,
            minimum_fill,
            request,
        );
    }
}

// ---------------------------------------------------------------------------
// Read requests
// ---------------------------------------------------------------------------

struct ByteTeeDefaultReadRequest {
    tee_state: Rc<ByteTeeState>,
}

impl ReadRequestNative for ByteTeeDefaultReadRequest {
    fn chunk_steps<'s>(
        self: Box<Self>,
        scope: &mut v8::PinScope<'s, '_>,
        chunk: v8::Local<'s, v8::Value>,
    ) {
        let chunk_g = v8::Global::new(scope, chunk);
        let tee_state = self.tee_state;
        promise_resolve::enqueue_microtask(scope, move |scope| {
            tee_state.read_again_for_branch1.set(false);
            tee_state.read_again_for_branch2.set(false);
            let chunk_l = v8::Local::new(scope, &chunk_g);
            let chunk_view = v8::Local::<v8::ArrayBufferView>::try_from(chunk_l).ok();

            // Snapshot buffer info for cloning.
            let buf_off_len = chunk_view.and_then(|v| {
                v.buffer(scope).map(|b| {
                    let g = v8::Global::new(scope, b);
                    (g, v.byte_offset() as u64, v.byte_length() as u64)
                })
            });

            // Clone for branch2 if both active. spec: CloneAsUint8Array.
            let cloned_for_branch2 = if !tee_state.canceled1.get() && !tee_state.canceled2.get() {
                buf_off_len.as_ref().map(|(buf_g, off, len)| {
                    let buf_l = v8::Local::new(scope, buf_g);
                    let cloned = v8::ArrayBuffer::new(scope, *len as usize);
                    let src_bs = buf_l.get_backing_store();
                    let dst_bs = cloned.get_backing_store();
                    copy_data_block_bytes(&dst_bs, 0, &src_bs, *off, *len);
                    let cloned_view = v8::Uint8Array::new(scope, cloned, 0, *len as usize);
                    cloned_view.map(|v| {
                        let view: v8::Local<v8::ArrayBufferView> = v.into();
                        v8::Global::new(scope, view)
                    })
                }).flatten()
            } else {
                None
            };

            // Branch 1: enqueue original chunk (TransferArrayBuffer).
            if !tee_state.canceled1.get() {
                if let Some(branch1_g) = tee_state.branch1.borrow().clone() {
                    let branch1_l = v8::Local::new(scope, &branch1_g);
                    let controller_v = slots::read_slot(scope, branch1_l, CONTROLLER);
                    if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
                        if is_byte_controller(scope, controller) {
                            if let Some(view) = chunk_view {
                                let _ = readable_byte_stream_controller_enqueue(
                                    scope, controller, view,
                                );
                            }
                        }
                    }
                }
            }
            // Branch 2: enqueue the cloned chunk.
            if !tee_state.canceled2.get() {
                if let Some(branch2_g) = tee_state.branch2.borrow().clone() {
                    let branch2_l = v8::Local::new(scope, &branch2_g);
                    let controller_v = slots::read_slot(scope, branch2_l, CONTROLLER);
                    if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
                        if is_byte_controller(scope, controller) {
                            if let Some(cloned_view_g) = cloned_for_branch2 {
                                let cloned_view_l = v8::Local::new(scope, &cloned_view_g);
                                let _ = readable_byte_stream_controller_enqueue(
                                    scope, controller, cloned_view_l,
                                );
                            } else if tee_state.canceled1.get() {
                                if let Some(view) = chunk_view {
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

    fn close_steps(self: Box<Self>, scope: &mut v8::PinScope) {
        let tee_state = self.tee_state;
        tee_state.reading.set(false);
        close_both_branches(scope, &tee_state, /* may_have_pending */ true, /* close_chunk */ None);
    }

    fn error_steps<'s>(
        self: Box<Self>,
        _scope: &mut v8::PinScope<'s, '_>,
        _reason: v8::Local<'s, v8::Value>,
    ) {
        self.tee_state.reading.set(false);
    }
}

struct ByteTeeBYOBReadRequest {
    tee_state: Rc<ByteTeeState>,
    for_branch2: bool,
}

impl ReadIntoRequestNative for ByteTeeBYOBReadRequest {
    fn chunk_steps<'s>(
        self: Box<Self>,
        scope: &mut v8::PinScope<'s, '_>,
        chunk: v8::Local<'s, v8::Value>,
    ) {
        let chunk_g = v8::Global::new(scope, chunk);
        let tee_state = self.tee_state;
        let for_branch2 = self.for_branch2;
        promise_resolve::enqueue_microtask(scope, move |scope| {
            tee_state.read_again_for_branch1.set(false);
            tee_state.read_again_for_branch2.set(false);
            let chunk_l = v8::Local::new(scope, &chunk_g);
            let chunk_view = match v8::Local::<v8::ArrayBufferView>::try_from(chunk_l) {
                Ok(v) => v,
                Err(_) => {
                    tee_state.reading.set(false);
                    return;
                }
            };

            let byob_canceled = if for_branch2 {
                tee_state.canceled2.get()
            } else {
                tee_state.canceled1.get()
            };
            let other_canceled = if for_branch2 {
                tee_state.canceled1.get()
            } else {
                tee_state.canceled2.get()
            };

            let byob_branch_g = if for_branch2 {
                tee_state.branch2.borrow().clone()
            } else {
                tee_state.branch1.borrow().clone()
            };
            let other_branch_g = if for_branch2 {
                tee_state.branch1.borrow().clone()
            } else {
                tee_state.branch2.borrow().clone()
            };

            // Snapshot for cloning.
            let buf_off_len = chunk_view.buffer(scope).map(|b| {
                let g = v8::Global::new(scope, b);
                (g, chunk_view.byte_offset() as u64, chunk_view.byte_length() as u64)
            });

            if !other_canceled {
                // Clone for the OTHER branch (CloneAsUint8Array equivalent).
                let cloned_view_g = buf_off_len.as_ref().map(|(buf_g, off, len)| {
                    let buf_l = v8::Local::new(scope, buf_g);
                    let cloned = v8::ArrayBuffer::new(scope, *len as usize);
                    let src_bs = buf_l.get_backing_store();
                    let dst_bs = cloned.get_backing_store();
                    copy_data_block_bytes(&dst_bs, 0, &src_bs, *off, *len);
                    let cloned_view = v8::Uint8Array::new(scope, cloned, 0, *len as usize);
                    cloned_view.map(|v| {
                        let view: v8::Local<v8::ArrayBufferView> = v.into();
                        v8::Global::new(scope, view)
                    })
                }).flatten();
                if !byob_canceled {
                    // respondWithNewView on byobBranch.
                    if let Some(byob_g) = byob_branch_g.clone() {
                        let branch_l = v8::Local::new(scope, &byob_g);
                        let controller_v = slots::read_slot(scope, branch_l, CONTROLLER);
                        if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
                            if is_byte_controller(scope, controller) {
                                let _ = readable_byte_stream_controller_respond_with_new_view(
                                    scope, controller, chunk_view,
                                );
                            }
                        }
                    }
                }
                // Enqueue cloned to other branch.
                if let Some(other_g) = other_branch_g {
                    if let Some(cloned_view_g) = cloned_view_g {
                        let branch_l = v8::Local::new(scope, &other_g);
                        let controller_v = slots::read_slot(scope, branch_l, CONTROLLER);
                        if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
                            if is_byte_controller(scope, controller) {
                                let cloned_view_l = v8::Local::new(scope, &cloned_view_g);
                                let _ = readable_byte_stream_controller_enqueue(
                                    scope, controller, cloned_view_l,
                                );
                            }
                        }
                    }
                }
            } else if !byob_canceled {
                // Only byob branch active.
                if let Some(byob_g) = byob_branch_g {
                    let branch_l = v8::Local::new(scope, &byob_g);
                    let controller_v = slots::read_slot(scope, branch_l, CONTROLLER);
                    if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
                        if is_byte_controller(scope, controller) {
                            let _ = readable_byte_stream_controller_respond_with_new_view(
                                scope, controller, chunk_view,
                            );
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

    fn close_steps<'s>(
        self: Box<Self>,
        scope: &mut v8::PinScope<'s, '_>,
        chunk: v8::Local<'s, v8::Value>,
    ) {
        // Spec: chunk is the partial-fill view (possibly zero byteLength).
        let tee_state = self.tee_state;
        tee_state.reading.set(false);
        let chunk_view = v8::Local::<v8::ArrayBufferView>::try_from(chunk).ok();
        let chunk_g = chunk_view.map(|v| v8::Global::new(scope, v));
        close_both_branches(scope, &tee_state, /* may_have_pending */ true, chunk_g.as_ref().map(|g| (g.clone(), self.for_branch2)));
    }

    fn error_steps<'s>(
        self: Box<Self>,
        _scope: &mut v8::PinScope<'s, '_>,
        _reason: v8::Local<'s, v8::Value>,
    ) {
        self.tee_state.reading.set(false);
    }
}

fn close_both_branches(
    scope: &mut v8::PinScope,
    tee_state: &Rc<ByteTeeState>,
    may_have_pending: bool,
    byob_close_chunk: Option<(v8::Global<v8::ArrayBufferView>, bool)>,
) {
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
    // Spec: if there's a BYOB-close chunk, respondWithNewView the byob
    // branch and respond(0) to the other branch's pending pull-into.
    if let Some((chunk_g, for_branch2)) = byob_close_chunk {
        let chunk_l = v8::Local::new(scope, &chunk_g);
        let byob_canceled = if for_branch2 {
            tee_state.canceled2.get()
        } else {
            tee_state.canceled1.get()
        };
        let other_canceled = if for_branch2 {
            tee_state.canceled1.get()
        } else {
            tee_state.canceled2.get()
        };
        let byob_branch_g = if for_branch2 {
            tee_state.branch2.borrow().clone()
        } else {
            tee_state.branch1.borrow().clone()
        };
        let other_branch_g = if for_branch2 {
            tee_state.branch1.borrow().clone()
        } else {
            tee_state.branch2.borrow().clone()
        };
        if !byob_canceled {
            if let Some(byob_g) = byob_branch_g {
                let branch_l = v8::Local::new(scope, &byob_g);
                let controller_v = slots::read_slot(scope, branch_l, CONTROLLER);
                if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
                    if is_byte_controller(scope, controller) {
                        let _ = readable_byte_stream_controller_respond_with_new_view(
                            scope, controller, chunk_l,
                        );
                    }
                }
            }
        }
        if !other_canceled {
            if let Some(other_g) = other_branch_g {
                let branch_l = v8::Local::new(scope, &other_g);
                let controller_v = slots::read_slot(scope, branch_l, CONTROLLER);
                if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
                    if is_byte_controller(scope, controller) {
                        let has_pending = crate::streams::readable_byte_controller::with_controller_state(
                            scope, controller, |s| !s.pending_pull_intos.borrow().is_empty(),
                        ).unwrap_or(false);
                        if has_pending {
                            let _ = readable_byte_stream_controller_respond(scope, controller, 0);
                        }
                    }
                }
            }
        }
    } else if may_have_pending {
        // Default-reader close path: respond(0) on any pending pull-intos.
        if !tee_state.canceled1.get() {
            if let Some(branch1_g) = tee_state.branch1.borrow().clone() {
                let branch1_l = v8::Local::new(scope, &branch1_g);
                let controller_v = slots::read_slot(scope, branch1_l, CONTROLLER);
                if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
                    if is_byte_controller(scope, controller) {
                        let has_pending = crate::streams::readable_byte_controller::with_controller_state(
                            scope, controller, |s| !s.pending_pull_intos.borrow().is_empty(),
                        ).unwrap_or(false);
                        if has_pending {
                            let _ = readable_byte_stream_controller_respond(scope, controller, 0);
                        }
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
                        let has_pending = crate::streams::readable_byte_controller::with_controller_state(
                            scope, controller, |s| !s.pending_pull_intos.borrow().is_empty(),
                        ).unwrap_or(false);
                        if has_pending {
                            let _ = readable_byte_stream_controller_respond(scope, controller, 0);
                        }
                    }
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
// forward_reader_error — error both branches if reader.closed rejects
// ---------------------------------------------------------------------------

fn forward_reader_error(scope: &mut v8::PinScope, tee_state: &Rc<ByteTeeState>) {
    let reader_g = tee_state.reader.borrow().clone();
    let reader_l = v8::Local::new(scope, &reader_g);
    let closed_v = slots::read_slot(scope, reader_l, CLOSED_PROMISE);
    let Ok(closed_p) = v8::Local::<v8::Promise>::try_from(closed_v) else {
        return;
    };
    // Compare by V8 identity to gate the rejection handling — we only act
    // when the rejected promise is the CURRENT reader's closed promise.
    let captured_reader_g = reader_g.clone();
    let tee_state = tee_state.clone();
    promise_resolve::upon_promise(
        scope,
        closed_p,
        None,
        Some(Box::new(move |scope, reason| {
            // If the active reader has changed since this handler was
            // installed, ignore (the new reader has its own forwarding).
            let cur_reader_g = tee_state.reader.borrow().clone();
            let cur_reader_l = v8::Local::new(scope, &cur_reader_g);
            let cap_reader_l = v8::Local::new(scope, &captured_reader_g);
            if !cur_reader_l.strict_equals(cap_reader_l.into()) {
                return;
            }
            if let Some(branch1_g) = tee_state.branch1.borrow().clone() {
                let branch1_l = v8::Local::new(scope, &branch1_g);
                let st = with_rs_state(scope, branch1_l, |s| s.state.get());
                if matches!(st, Some(StreamState::Readable)) {
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
                if matches!(st, Some(StreamState::Readable)) {
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
