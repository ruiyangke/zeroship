//! `ReadableStreamDefaultTee` — spec §3.5.2.
//!
//! Per design §X.1: each branch is an INDEPENDENT ReadableStream with
//! its own controller AND its own queue (D-11). The two branches share
//! a SINGLE source-side reader via a SHARED `pullAlgorithm`. There is
//! no shared queue.
//!
//! The shared `pullAlgorithm` issues ONE read on the source reader; on
//! chunk it queues a microtask to enqueue the chunk into BOTH branches'
//! controllers (delayed so that source-side synchronous errors win the
//! race over synchronously-available reads — see ref impl comment).
//!
//! Per CRITICAL #6 (streams round-1): tee MUST NOT use a shared queue;
//! that's the v1 design's "wrong way" that was corrected to D-11's
//! "independent queues, shared pullAlgorithm".

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::streams::algorithms;
use crate::streams::promise_resolve;
use crate::streams::readable::{build_value_stream_wrapper_for_internal, with_rs_state};
use crate::streams::readable_default_controller::{
    readable_stream_default_controller_close, readable_stream_default_controller_enqueue,
    readable_stream_default_controller_error,
    set_up_readable_stream_default_controller_from_underlying_source_with_strategy, SizeAlgorithm,
};
use crate::streams::readable_default_reader::{
    acquire_readable_stream_default_reader, ReadRequest, ReadRequestKind, ReadRequestNative,
};
use crate::streams::slots::{self, CLOSED_PROMISE, CONTROLLER};

// ---------------------------------------------------------------------------
// TeeState
// ---------------------------------------------------------------------------

/// Shared state for the two tee branches. Lives inside the closure
/// holders for `pullAlgorithm` and `cancelN`. All fields are Cell /
/// RefCell so the closure can be called repeatedly without exclusive
/// borrows being held across V8 boundaries.
#[allow(missing_debug_implementations)]
pub struct TeeState {
    /// Source ReadableStream wrapper.
    pub source: v8::Global<v8::Object>,
    /// Default reader on the source (shared by both branches' pulls).
    pub reader: v8::Global<v8::Object>,
    /// Branch-1 ReadableStream wrapper. Set once by `readable_stream_default_tee`.
    pub branch1: RefCell<Option<v8::Global<v8::Object>>>,
    pub branch2: RefCell<Option<v8::Global<v8::Object>>>,
    /// Spec flags.
    pub reading: Cell<bool>,
    pub read_again: Cell<bool>,
    pub canceled1: Cell<bool>,
    pub canceled2: Cell<bool>,
    pub reason1: RefCell<Option<v8::Global<v8::Value>>>,
    pub reason2: RefCell<Option<v8::Global<v8::Value>>>,
    /// `cancelPromise` per spec — resolves with the source-cancel
    /// promise when both branches cancel, or with undefined when the
    /// source closes/errors before both cancellations.
    pub cancel_promise_resolver: v8::Global<v8::PromiseResolver>,
    pub cancel_promise: v8::Global<v8::Promise>,
}

// ---------------------------------------------------------------------------
// Public entrypoint — ReadableStreamDefaultTee
// ---------------------------------------------------------------------------

/// `ReadableStreamDefaultTee(stream, cloneForBranch2)` — spec §3.5.2.
///
/// `clone_for_branch2` is currently always false (the public `tee()`
/// passes false; the structuredClone path is reserved for future
/// callers — see design note in §X.1).
pub fn readable_stream_default_tee<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    source: v8::Local<v8::Object>,
    _clone_for_branch2: bool,
) -> Result<[v8::Local<'s, v8::Object>; 2], String> {
    // Acquire a default reader on the source. The source becomes locked
    // for the lifetime of both branches.
    let reader = acquire_readable_stream_default_reader(scope, source)
        .map_err(|e| format!("tee: {e}"))?;

    // Allocate the shared cancelPromise + resolver.
    let cancel_resolver = v8::PromiseResolver::new(scope).unwrap();
    let cancel_promise = cancel_resolver.get_promise(scope);

    let tee_state = Rc::new(TeeState {
        source: v8::Global::new(scope, source),
        reader: v8::Global::new(scope, reader),
        branch1: RefCell::new(None),
        branch2: RefCell::new(None),
        reading: Cell::new(false),
        read_again: Cell::new(false),
        canceled1: Cell::new(false),
        canceled2: Cell::new(false),
        reason1: RefCell::new(None),
        reason2: RefCell::new(None),
        cancel_promise_resolver: v8::Global::new(scope, cancel_resolver),
        cancel_promise: v8::Global::new(scope, cancel_promise),
    });

    // Build branch 1 — independent ReadableStream. underlyingSource:
    //   - pull: shared pullAlgorithm via `tee_pull_callback`.
    //   - cancel: branch-1 cancelAlgorithm.
    let branch1 = build_branch_stream(scope, &tee_state, BranchIdx::One)?;
    *tee_state.branch1.borrow_mut() = Some(v8::Global::new(scope, branch1));

    let branch2 = build_branch_stream(scope, &tee_state, BranchIdx::Two)?;
    *tee_state.branch2.borrow_mut() = Some(v8::Global::new(scope, branch2));

    // Watch the reader's closedPromise. On rejection, error both branches.
    chain_reader_closed_rejection(scope, &tee_state);

    Ok([branch1, branch2])
}

// ---------------------------------------------------------------------------
// build_branch_stream — construct one of the two branches
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum BranchIdx {
    One,
    Two,
}

fn build_branch_stream<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    tee_state: &Rc<TeeState>,
    idx: BranchIdx,
) -> Result<v8::Local<'s, v8::Object>, String> {
    // Build a JS underlyingSource = { pull, cancel } whose closures
    // capture the shared TeeState. Default-count strategy, HWM=0.

    let underlying = v8::Object::new(scope);

    // pull callback — shared across both branches via `tee_pull_callback`.
    {
        let pull_fn = build_pull_fn(scope, tee_state.clone());
        let key = v8::String::new(scope, "pull").unwrap();
        underlying.set(scope, key.into(), pull_fn.into());
    }

    // cancel callback — per-branch (branch-1 vs branch-2 logic).
    {
        let cancel_fn = build_cancel_fn(scope, tee_state.clone(), idx);
        let key = v8::String::new(scope, "cancel").unwrap();
        underlying.set(scope, key.into(), cancel_fn.into());
    }

    let stream = build_value_stream_wrapper_for_internal(scope);
    set_up_readable_stream_default_controller_from_underlying_source_with_strategy(
        scope,
        stream,
        underlying.into(),
        /* hwm */ 1.0,
        SizeAlgorithm::DefaultCount,
    )
    .map_err(|e| format!("tee: build branch: {e}"))?;
    Ok(stream)
}

// ---------------------------------------------------------------------------
// Pull function — shared across both branches
// ---------------------------------------------------------------------------

/// Holder for the pull-function closure data.
struct PullHolder {
    tee_state: Rc<TeeState>,
}

fn build_pull_fn<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    tee_state: Rc<TeeState>,
) -> v8::Local<'s, v8::Function> {
    let holder = Rc::new(PullHolder { tee_state });
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
    // SAFETY: holder pinned in External via Rc::into_raw at build time;
    // remains live as long as the underlyingSource (and hence the
    // controller) is reachable.
    let holder: &PullHolder = unsafe { &*raw };
    let tee_state = holder.tee_state.clone();

    pull_algorithm(scope, &tee_state);

    // pull returns a resolved promise per spec.
    let p = algorithms::resolved_undefined_promise(scope);
    rv.set(p.into());
}

/// Spec `pullAlgorithm` body. Implements the read-one-then-enqueue-both
/// loop with the microtask delay (so source-side errors win the race).
fn pull_algorithm(scope: &mut v8::PinScope, tee_state: &Rc<TeeState>) {
    if tee_state.reading.get() {
        tee_state.read_again.set(true);
        return;
    }
    tee_state.reading.set(true);

    // Issue a read using a Native ReadRequest so chunk/close/error
    // steps run synchronously inside the controller's fulfill path.
    // Per spec §3.5.2 chunkSteps is then delayed by exactly one
    // microtask via `enqueue_microtask` (see chunk_steps body) so
    // source-side synchronous errors reach branches before the
    // synchronously-available chunk does.
    let reader_l = v8::Local::new(scope, &tee_state.reader);
    let source_l = v8::Local::new(scope, &tee_state.source);

    let request = ReadRequest {
        kind: ReadRequestKind::Native(Box::new(TeeReadRequest {
            tee_state: tee_state.clone(),
        })),
    };
    crate::streams::readable_default_reader::readable_stream_default_reader_read(
        scope, reader_l, source_l, request,
    );
}

struct TeeReadRequest {
    tee_state: Rc<TeeState>,
}

impl ReadRequestNative for TeeReadRequest {
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
        // errorSteps — set reading=false; reader.closedPromise rejects
        // and chain_reader_closed_rejection errors both branches.
        self.tee_state.reading.set(false);
    }
}

fn chunk_steps<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    tee_state: &Rc<TeeState>,
    chunk: v8::Local<'s, v8::Value>,
) {
    // Per spec: the chunkSteps body runs inside a queued microtask. The
    // promise-then chain we used above already imposes one microtask;
    // we add an additional microtask here so source-side errors win the
    // race even when the read came from an already-buffered chunk.
    let chunk_g = v8::Global::new(scope, chunk);
    let tee_state = tee_state.clone();
    promise_resolve::enqueue_microtask(scope, move |scope| {
        tee_state.read_again.set(false);
        let chunk_l = v8::Local::new(scope, &chunk_g);

        if !tee_state.canceled1.get() {
            if let Some(branch1_g) = tee_state.branch1.borrow().clone() {
                let branch1_l = v8::Local::new(scope, &branch1_g);
                let controller_v = slots::read_slot(scope, branch1_l, CONTROLLER);
                if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
                    let _ = readable_stream_default_controller_enqueue(scope, controller, chunk_l);
                }
            }
        }
        if !tee_state.canceled2.get() {
            if let Some(branch2_g) = tee_state.branch2.borrow().clone() {
                let branch2_l = v8::Local::new(scope, &branch2_g);
                let controller_v = slots::read_slot(scope, branch2_l, CONTROLLER);
                if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
                    let _ = readable_stream_default_controller_enqueue(scope, controller, chunk_l);
                }
            }
        }

        tee_state.reading.set(false);
        if tee_state.read_again.get() {
            pull_algorithm(scope, &tee_state);
        }
    });
}

fn close_steps(scope: &mut v8::PinScope, tee_state: &Rc<TeeState>) {
    tee_state.reading.set(false);
    if !tee_state.canceled1.get() {
        if let Some(branch1_g) = tee_state.branch1.borrow().clone() {
            let branch1_l = v8::Local::new(scope, &branch1_g);
            let controller_v = slots::read_slot(scope, branch1_l, CONTROLLER);
            if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
                readable_stream_default_controller_close(scope, controller);
            }
        }
    }
    if !tee_state.canceled2.get() {
        if let Some(branch2_g) = tee_state.branch2.borrow().clone() {
            let branch2_l = v8::Local::new(scope, &branch2_g);
            let controller_v = slots::read_slot(scope, branch2_l, CONTROLLER);
            if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
                readable_stream_default_controller_close(scope, controller);
            }
        }
    }
    if !tee_state.canceled1.get() || !tee_state.canceled2.get() {
        // Resolve cancelPromise with undefined (per spec).
        let resolver_l = v8::Local::new(scope, &tee_state.cancel_promise_resolver);
        let und = v8::undefined(scope);
        resolver_l.resolve(scope, und.into());
    }
}

// ---------------------------------------------------------------------------
// Cancel function — per-branch
// ---------------------------------------------------------------------------

struct CancelHolder {
    tee_state: Rc<TeeState>,
    idx: BranchIdx,
}

fn build_cancel_fn<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    tee_state: Rc<TeeState>,
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
    tee_state: &Rc<TeeState>,
    idx: BranchIdx,
    reason: v8::Local<'s, v8::Value>,
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
        // Composite reason — array of [reason1, reason2].
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
        // Forward cancel_result to cancelPromise.
        let resolver_l = v8::Local::new(scope, &tee_state.cancel_promise_resolver);
        // Use Promise resolution semantics: PromiseResolver::resolve with
        // a Promise value adopts that promise's resolution.
        resolver_l.resolve(scope, cancel_result.into());
    }
    v8::Local::new(scope, &tee_state.cancel_promise)
}

// ---------------------------------------------------------------------------
// chain_reader_closed_rejection — error both branches if reader.closed rejects
// ---------------------------------------------------------------------------

fn chain_reader_closed_rejection(scope: &mut v8::PinScope, tee_state: &Rc<TeeState>) {
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
            // Error both branch controllers.
            if let Some(branch1_g) = tee_state.branch1.borrow().clone() {
                let branch1_l = v8::Local::new(scope, &branch1_g);
                // Only error if branch1 is still readable (canceled
                // branches have already been closed/errored).
                let st = with_rs_state(scope, branch1_l, |s| s.state.get());
                if matches!(st, Some(crate::streams::readable::StreamState::Readable)) {
                    let controller_v = slots::read_slot(scope, branch1_l, CONTROLLER);
                    if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
                        readable_stream_default_controller_error(scope, controller, reason);
                    }
                }
            }
            if let Some(branch2_g) = tee_state.branch2.borrow().clone() {
                let branch2_l = v8::Local::new(scope, &branch2_g);
                let st = with_rs_state(scope, branch2_l, |s| s.state.get());
                if matches!(st, Some(crate::streams::readable::StreamState::Readable)) {
                    let controller_v = slots::read_slot(scope, branch2_l, CONTROLLER);
                    if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
                        readable_stream_default_controller_error(scope, controller, reason);
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
