//! `TransformStream` — spec §5.2.
//!
//! Constructor uses `#[v8_class]` +
//! `#[v8_constructor(post_init = ...)]`. Per-instance getters
//! (`readable` / `writable`) and the cross-module receiver checks in
//! `algorithms.rs` / `transform_controller.rs`, which hold raw
//! `Local<Object>`s, test the same `TSStreamState` brand the macro's
//! callbacks do (see `crate::brand`).
//!
//! IDL surface (§5.2):
//! ```webidl
//! [Exposed=*]
//! interface TransformStream {
//!   constructor(
//!     optional object transformer,
//!     optional QueuingStrategy writableStrategy = {},
//!     optional QueuingStrategy readableStrategy = {});
//!   readonly attribute ReadableStream readable;
//!   readonly attribute WritableStream writable;
//! };
//! ```
//!
//! Storage:
//! - `[[backpressure]]`               → Rust Cell<bool>            (SLOT)
//! - `[[backpressureChangePromise]]`  → Rust paired Promise+Resolver (SLOT)
//! - `[[readable]]`                   → V8 priv sym `[[readable]]` (SLOT)
//! - `[[writable]]`                   → V8 priv sym `[[writable]]` (SLOT)
//! - `[[controller]]` (TS controller) → V8 priv sym `[[ts.controller]]` (SLOT)
//! - `[[Detached]]`                   → not supported yet
//! - `transformerCodec` (compression dep #5) → V8 priv sym holding External —
//!   lands with compression, not in v1.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use zeroship_runtime_macros::v8_class;

use crate::callback::Callback;
use crate::intrinsics::{self, Intrinsic};
use crate::state::OpError;
use crate::streams::budget::{try_alloc_streams, StreamBudgetGuard};
use crate::streams::readable_default_controller::SizeAlgorithm;
use crate::streams::slots;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// What `InitializeTransformStream` needs to build one half: its queuing
/// strategy and the budget unit charged for it.
pub struct HalfSetup {
    pub hwm: f64,
    pub size: SizeAlgorithm,
    pub budget: StreamBudgetGuard,
}

/// Both halves of a `TransformStream`. Their budget units are charged together
/// with the stream's own, so a construction the budget cannot cover is
/// refused before any of the three streams exists.
pub struct TransformHalves {
    pub readable: HalfSetup,
    pub writable: HalfSetup,
}

/// Setup args stashed by the constructor body so `after_install` can
/// finish wiring once the box is reachable via internal field 0. None
/// for streams built by `create_identity_transform_stream` (the Rust-side helper
/// runs its own controller setup directly).
struct PendingTransformSetup {
    transformer: v8::Global<v8::Value>,
    halves: TransformHalves,
}

/// `Box<TSStreamState>` lives in the wrapper's V8 internal field 0.
/// This struct holds ONLY pure-Rust slots (the
/// backpressure pair). The `[[readable]]`, `[[writable]]`, `[[controller]]`
/// slots live in V8 private symbols.
#[allow(missing_debug_implementations)]
pub struct TSStreamState {
    /// SLOT: `[[backpressure]]` — current backpressure flag.
    pub backpressure: Cell<bool>,
    /// SLOT: `[[backpressureChangePromise]]` — paired storage. The Promise
    /// is the JS-visible side; the Resolver lets Rust resolve it when
    /// backpressure flips. We keep it in one place, in Rust state,
    /// because the spec only needs the Promise
    /// for `await`-ing in the source's pull algorithm and for
    /// SetBackpressure to resolve+replace; no algorithm needs to compare
    /// it for JS identity.
    pub bp_change_promise: RefCell<Option<v8::Global<v8::Promise>>>,
    pub bp_change_resolver: RefCell<Option<v8::Global<v8::PromiseResolver>>>,
    /// Constructor args plumbing — populated by the constructor body, consumed
    /// (`take()`) by `after_install`. None on the `create_identity_transform_stream`
    /// path which wires the controller directly without the post_init hook.
    pending_setup: RefCell<Option<PendingTransformSetup>>,
    /// This stream's charge to its isolate's budget.
    _budget: StreamBudgetGuard,
}

impl TSStreamState {
    /// Allocate the boxed state with no pending setup — used by
    /// `create_identity_transform_stream`, which drives controller setup directly
    /// rather than through the macro's post_init hook.
    fn new_for_internal(budget: StreamBudgetGuard) -> Self {
        Self {
            backpressure: Cell::new(false),
            bp_change_promise: RefCell::new(None),
            bp_change_resolver: RefCell::new(None),
            pending_setup: RefCell::new(None),
            _budget: budget,
        }
    }
}

#[v8_class]
#[v8_to_string_tag = "TransformStream"]
impl TSStreamState {
    /// `new TransformStream(transformer?, writableStrategy?, readableStrategy?)` —
    /// spec §5.2.4.
    ///
    /// Body covers the pre-box-install half:
    ///   1. Charge the budget for the stream and both halves at once.
    ///   2. Parse writableStrategy (default HWM = 1.0).
    ///   3. Parse readableStrategy (default HWM = 0.0).
    ///   4. Reject `transformer.readableType` / `writableType`
    ///      (RangeError per §5.2.4 step 7+8).
    ///
    /// `after_install` runs the post-box-install half: `start_resolver`,
    /// controller setup. Splitting at the V8-wrapper
    /// boundary lets the macro install the Box first; only after that
    /// can `set_up_transform_stream_default_controller_from_transformer`
    /// drive `with_ts_state`-keyed algorithms.
    #[v8_constructor(post_init = "after_install")]
    fn new(
        scope: &mut v8::PinScope,
        transformer: v8::Local<v8::Value>,
        writable_strategy: v8::Local<v8::Value>,
        readable_strategy: v8::Local<v8::Value>,
    ) -> Result<Self, OpError> {
        // Budget first: the stream and its two halves, all or none.
        let [budget, readable_budget, writable_budget] = try_alloc_streams::<3>(scope)?;

        // Spec §5.2.4 ordering: strategies converted before transformer
        // dictionary lookup (writableStrategy default HWM = 1, readable
        // default HWM = 0). `parse_strategy_local` returns OpError; user
        // exceptions in size/highWaterMark accessors land as
        // OpError::JsValue and the macro's throw arms re-throw verbatim.
        let (writable_hwm, writable_size) =
            crate::streams::readable::parse_strategy_local(scope, writable_strategy, 1.0)?;
        let (readable_hwm, readable_size) =
            crate::streams::readable::parse_strategy_local(scope, readable_strategy, 0.0)?;

        // Spec §5.2.4 step 7+8: reject `readableType` / `writableType`.
        // Use a tc_scope so a throwing user getter on the transformer
        // dict surfaces as OpError::JsValue (verbatim re-throw) rather
        // than a swallowed pending exception.
        if let Ok(t_obj) = v8::Local::<v8::Object>::try_from(transformer) {
            for key_name in ["readableType", "writableType"] {
                v8::tc_scope!(let tc, scope);
                let key = v8::String::new(tc, key_name).unwrap();
                let v = t_obj.get(tc, key.into());
                if tc.has_caught() {
                    let exc = tc.exception().unwrap();
                    return Err(OpError::js_value(
                        tc,
                        exc,
                        format!("transformer.{key_name} getter threw"),
                    ));
                }
                let Some(v) = v else {
                    return Err(OpError::error(format!(
                        "error reading transformer.{key_name}"
                    )));
                };
                if !v.is_undefined() {
                    return Err(OpError::range_error(format!(
                        "TransformStream: transformer.{key_name} is reserved"
                    )));
                }
            }
        }

        let transformer_g = v8::Global::new(scope, transformer);
        Ok(Self {
            backpressure: Cell::new(false),
            bp_change_promise: RefCell::new(None),
            bp_change_resolver: RefCell::new(None),
            pending_setup: RefCell::new(Some(PendingTransformSetup {
                transformer: transformer_g,
                halves: TransformHalves {
                    readable: HalfSetup {
                        hwm: readable_hwm,
                        size: readable_size,
                        budget: readable_budget,
                    },
                    writable: HalfSetup {
                        hwm: writable_hwm,
                        size: writable_size,
                        budget: writable_budget,
                    },
                },
            })),
            _budget: budget,
        })
    }

    /// The post-init hook: `start_resolver` alloc, controller setup from the
    /// transformer dict. Runs AFTER the macro installed and branded the
    /// Box in field 0, so subsequent `with_ts_state` calls (driven by
    /// `set_up_transform_stream_default_controller_from_transformer` →
    /// `initialize_transform_stream` → `transform_stream_set_backpressure`)
    /// can recover the boxed state.
    pub(crate) fn after_install(
        scope: &mut v8::PinScope,
        this: v8::Local<v8::Object>,
    ) -> Result<(), OpError> {
        let setup = with_ts_state(scope, this, |s| s.pending_setup.borrow_mut().take())
            .ok_or_else(|| OpError::error("after_install: with_ts_state returned None"))?
            .ok_or_else(|| OpError::error("after_install: missing pending_setup"))?;
        let transformer = v8::Local::new(scope, &setup.transformer);

        // Owned by the post_init so a synchronous user `start()` throw
        // surfaces as OpError::JsValue from the helper — the macro's
        // throw arms re-throw verbatim, no sentinel-string dance.
        let start_resolver = v8::PromiseResolver::new(scope)
            .ok_or_else(|| OpError::error("PromiseResolver::new failed"))?;
        crate::streams::transform_controller::set_up_transform_stream_default_controller_from_transformer(
            scope,
            this,
            transformer,
            start_resolver,
            setup.halves,
        )
    }
}

// ---------------------------------------------------------------------------
// V8 wrapper helpers
// ---------------------------------------------------------------------------

/// Whether `obj` is a `TransformStream` wrapper: one this runtime branded as
/// holding a `TSStreamState`.
pub fn is_transform_stream(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
) -> bool {
    crate::brand::is::<TSStreamState>(scope, obj)
}

pub fn with_ts_state<R>(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
    f: impl FnOnce(&TSStreamState) -> R,
) -> Option<R> {
    let ptr = crate::brand::state::<TSStreamState>(scope, stream)?;
    // SAFETY: the brand proves internal field 0 holds the
    // Box<TSStreamState> stored at construction. The Box is dropped only
    // by the V8 weak finalizer, which cannot run while `stream` is
    // reachable.
    let inst = unsafe { ptr.as_ref() };
    Some(f(inst))
}

/// Read the `[[readable]]` priv sym (the JS-visible ReadableStream half).
pub fn readable_slot<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    ts: v8::Local<v8::Object>,
) -> v8::Local<'s, v8::Value> {
    slots::read_slot(scope, ts, slots::READABLE)
}

/// Read the `[[writable]]` priv sym (the JS-visible WritableStream half).
pub fn writable_slot<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    ts: v8::Local<v8::Object>,
) -> v8::Local<'s, v8::Value> {
    slots::read_slot(scope, ts, slots::WRITABLE)
}

/// Convenience: as Object (None if priv sym empty/not an object).
pub fn readable_slot_obj<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    ts: v8::Local<v8::Object>,
) -> Option<v8::Local<'s, v8::Object>> {
    let v = readable_slot(scope, ts);
    v8::Local::<v8::Object>::try_from(v).ok()
}

pub fn writable_slot_obj<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    ts: v8::Local<v8::Object>,
) -> Option<v8::Local<'s, v8::Object>> {
    let v = writable_slot(scope, ts);
    v8::Local::<v8::Object>::try_from(v).ok()
}

/// Read the TS-level `[[controller]]` priv sym (TransformStreamDefaultController
/// wrapper).
pub fn ts_controller_slot<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    ts: v8::Local<v8::Object>,
) -> v8::Local<'s, v8::Value> {
    slots::read_slot(scope, ts, slots::TS_CONTROLLER)
}

// ---------------------------------------------------------------------------
// Identity TransformStream: Rust-only constructor
// ---------------------------------------------------------------------------

/// Streams "create an identity TransformStream": a TransformStream whose
/// transform enqueues every chunk unchanged, set up with the writable
/// high-water mark 1 and readable high-water mark 0 that "set up a
/// TransformStream" uses.
///
/// Built from the intrinsic class rather than `globalThis.TransformStream`,
/// so creator code cannot substitute it. The stream and both halves are
/// charged to the isolate's budget before any of them exists, so an
/// exhausted budget is the budget's `RangeError` and builds nothing.
pub(crate) fn create_identity_transform_stream<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    let [budget, readable_budget, writable_budget] = try_alloc_streams::<3>(scope)?;
    let start_resolver = v8::PromiseResolver::new(scope)
        .ok_or_else(|| OpError::error("identity TransformStream: no start promise"))?;
    let stream = build_stream_wrapper(scope, budget)
        .ok_or_else(|| OpError::error("identity TransformStream: wrapper allocation failed"))?;
    let no_transformer: v8::Local<v8::Value> = v8::undefined(scope).into();
    crate::streams::transform_controller::set_up_transform_stream_default_controller_from_transformer(
        scope,
        stream,
        no_transformer,
        start_resolver,
        TransformHalves {
            readable: HalfSetup {
                hwm: 0.0,
                size: SizeAlgorithm::DefaultCount,
                budget: readable_budget,
            },
            writable: HalfSetup {
                hwm: 1.0,
                size: SizeAlgorithm::DefaultCount,
                budget: writable_budget,
            },
        },
    )?;
    Ok(stream)
}

/// Construct the bare `TransformStream` JS wrapper — no controller wired,
/// no halves attached. Used by `create_identity_transform_stream`. The
/// macro's post_init path mints its own wrapper through the constructor
/// callback; this helper is the parallel manual mint for the Rust-side path.
/// `None` when V8 cannot allocate the wrapper, which happens only with an
/// exception or a termination already pending.
fn build_stream_wrapper<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    budget: StreamBudgetGuard,
) -> Option<v8::Local<'s, v8::Object>> {
    let tmpl = TSStreamState::install(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let stream_obj = inst_tmpl.new_instance(scope)?;

    crate::brand::wrap(scope, stream_obj, TSStreamState::new_for_internal(budget));

    let proto = if let Some(proto) = intrinsics::prototype(scope, Intrinsic::TransformStream) {
        proto.into()
    } else {
        let class_fn = tmpl.get_function(scope)?;
        let proto_key = v8::String::new(scope, "prototype")?;
        class_fn.get(scope, proto_key.into())?
    };
    stream_obj.set_prototype(scope, proto);

    Some(stream_obj)
}

// ---------------------------------------------------------------------------
// `readable` getter (§5.2.5.1) and `writable` getter (§5.2.5.2)
// ---------------------------------------------------------------------------
//
// Stay raw FunctionCallbacks — same rationale as the readers/writer (they
// need direct `args.this()` access for priv-sym reads, which the macro's
// `&self`-only getter shape doesn't surface). Migrating these onto
// `#[v8_getter]` is part of the deferred per-method refactor.

fn readable_getter_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    if !is_transform_stream(scope, this) {
        let msg = v8::String::new(scope, "readable: receiver is not a TransformStream").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    rv.set(readable_slot(scope, this));
}

fn writable_getter_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    if !is_transform_stream(scope, this) {
        let msg = v8::String::new(scope, "writable: receiver is not a TransformStream").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    rv.set(writable_slot(scope, this));
}

// ---------------------------------------------------------------------------
// build_readable_for_ts / build_writable_for_ts — used by InitializeTransformStream
// ---------------------------------------------------------------------------
//
// These build the JS-visible halves with algorithms that forward to the
// TS controller's transform/flush/cancel. The forwarding logic uses the
// `Js` variant with closures that we install via FunctionTemplate so the
// existing `set_up_…` paths can drive them.
//
// Key spec details (§5.4.2.1, §5.4.2.2):
//   sink.write(chunk)  → TransformStreamDefaultControllerPerformTransform(controller, chunk)
//   sink.close()       → flush_promise; on settle, close readable + (re-)error if needed
//   sink.abort(reason) → TransformStreamError(stream, reason); reject finishPromise
//
//   source.pull()      → TransformStreamSetBackpressure(stream, false); return bp_change_promise
//   source.cancel(r)   → TransformStreamErrorWritableAndUnblockWrite(stream, r);
//                        and resolve cancelPromise so the controller's [[finishPromise]] settles
//
// Implementation strategy:
//   - Build sink/source as JS-side closures bound via FunctionTemplate, keyed
//     on the TS wrapper's V8 identity. Closures capture a Global to the TS.
//   - The closures are constructed OUTSIDE the existing `set_up_…_from_underlying_…`
//     path because we need to pass JS Functions, not Rust traits. The
//     existing `set_up_writable_stream_default_controller_from_underlying_sink_with_strategy`
//     accepts an `underlyingSink` JS object — so we build that object with
//     write/close/abort properties pointing at our forwarders.

pub fn build_readable_for_ts<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    ts: v8::Local<v8::Object>,
    start_promise: v8::Local<'s, v8::Promise>,
    half: HalfSetup,
) -> Option<v8::Local<'s, v8::Object>> {
    let HalfSetup { hwm, size, budget } = half;
    // Build a JS underlyingSource = { start, pull, cancel } where:
    //   start  → returns startPromise
    //   pull   → calls source pull algorithm
    //   cancel → calls source cancel algorithm
    //
    // Because `set_up_readable_stream_default_controller_from_underlying_source_with_strategy`
    // takes a JS Value, we build a fresh Object with those three functions.

    let underlying = v8::Object::new(scope);
    let ts_global = v8::Global::new(scope, ts);

    let start_fn = build_ts_readable_start_fn(scope, start_promise);
    install_function(scope, underlying, "start", start_fn);
    let pull_fn = build_ts_readable_pull_fn(scope, ts_global.clone());
    install_function(scope, underlying, "pull", pull_fn);
    let cancel_fn = build_ts_readable_cancel_fn(scope, ts_global);
    install_function(scope, underlying, "cancel", cancel_fn);

    // Build a fresh ReadableStream wrapper — same as the public
    // constructor but skip the `type === "bytes"` check etc. since we're
    // building a default ReadableStream programmatically.
    let readable = crate::streams::readable::build_value_stream_wrapper_for_internal(scope, budget)?;
    let _ = crate::streams::readable_default_controller::set_up_readable_stream_default_controller_from_underlying_source_with_strategy(
        scope,
        readable,
        underlying.into(),
        hwm,
        size,
    );
    Some(readable)
}

pub fn build_writable_for_ts<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    ts: v8::Local<v8::Object>,
    start_promise: v8::Local<'s, v8::Promise>,
    half: HalfSetup,
) -> Option<v8::Local<'s, v8::Object>> {
    let HalfSetup { hwm, size, budget } = half;
    let underlying = v8::Object::new(scope);
    let ts_global = v8::Global::new(scope, ts);

    let start_fn = build_ts_writable_start_fn(scope, start_promise);
    install_function(scope, underlying, "start", start_fn);
    let write_fn = build_ts_writable_write_fn(scope, ts_global.clone());
    install_function(scope, underlying, "write", write_fn);
    let close_fn = build_ts_writable_close_fn(scope, ts_global.clone());
    install_function(scope, underlying, "close", close_fn);
    let abort_fn = build_ts_writable_abort_fn(scope, ts_global);
    install_function(scope, underlying, "abort", abort_fn);

    let writable = crate::streams::writable::build_value_stream_wrapper_for_internal(scope, budget)?;
    let _ = crate::streams::writable_controller::set_up_writable_stream_default_controller_from_underlying_sink_with_strategy(
        scope,
        writable,
        underlying.into(),
        hwm,
        ws_size(size),
    );
    Some(writable)
}

/// SizeAlgorithm conversion — readable_default_controller's enum is shared
/// for both halves; writable_controller pulls the same `SizeAlgorithm`
/// type via `pub use` so this is a noop semantically. We re-state it as a
/// passthrough so a future divergence (e.g. byte-length normalization for
/// WS) is captured in one place.
fn ws_size(s: SizeAlgorithm) -> SizeAlgorithm {
    s
}

fn install_function(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    name: &str,
    f: v8::Local<v8::Function>,
) {
    let key = v8::String::new(scope, name).unwrap();
    obj.set(scope, key.into(), f.into());
}

// ---------------------------------------------------------------------------
// Forwarder builders — closures captured in External holders
// ---------------------------------------------------------------------------
//
// The JS underlyingSource/underlyingSink expects `Function` values. We build
// FunctionTemplates with `data = External(holder)` carrying the forwarder
// closure. Each forwarder captures the TS Global so it can fish out state
// from any callsite.

fn build_ts_readable_start_fn<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    start_promise: v8::Local<'s, v8::Promise>,
) -> v8::Local<'s, v8::Function> {
    // start() returns startPromise so that the readable's controller's
    // started flag flips when start_promise settles.
    let p_g = v8::Global::new(scope, start_promise);
    build_oneshot_promise_fn(scope, p_g)
}

fn build_ts_writable_start_fn<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    start_promise: v8::Local<'s, v8::Promise>,
) -> v8::Local<'s, v8::Function> {
    let p_g = v8::Global::new(scope, start_promise);
    build_oneshot_promise_fn(scope, p_g)
}

fn build_ts_readable_pull_fn<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    ts_g: v8::Global<v8::Object>,
) -> v8::Local<'s, v8::Function> {
    build_method_fn(scope, ts_g, ReadableMethod::Pull)
}

fn build_ts_readable_cancel_fn<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    ts_g: v8::Global<v8::Object>,
) -> v8::Local<'s, v8::Function> {
    build_method_fn(scope, ts_g, ReadableMethod::Cancel)
}

fn build_ts_writable_write_fn<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    ts_g: v8::Global<v8::Object>,
) -> v8::Local<'s, v8::Function> {
    build_method_fn(scope, ts_g, ReadableMethod::Write)
}

fn build_ts_writable_close_fn<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    ts_g: v8::Global<v8::Object>,
) -> v8::Local<'s, v8::Function> {
    build_method_fn(scope, ts_g, ReadableMethod::Close)
}

fn build_ts_writable_abort_fn<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    ts_g: v8::Global<v8::Object>,
) -> v8::Local<'s, v8::Function> {
    build_method_fn(scope, ts_g, ReadableMethod::Abort)
}

#[derive(Clone, Copy)]
enum ReadableMethod {
    Pull,
    Cancel,
    Write,
    Close,
    Abort,
}

#[allow(missing_debug_implementations)]
struct MethodHolder {
    ts: v8::Global<v8::Object>,
    method: ReadableMethod,
}

fn build_method_fn<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    ts_g: v8::Global<v8::Object>,
    method: ReadableMethod,
) -> v8::Local<'s, v8::Function> {
    let holder = Rc::new(MethodHolder { ts: ts_g, method });
    let raw = Rc::into_raw(holder) as *mut std::ffi::c_void;
    let ext = v8::External::new(scope, raw);
    let tmpl = crate::callback::template_builder(method_callback)
        .data(ext.into())
        .build(scope);
    let f = tmpl.get_function(scope).unwrap();

    // Drop the Rc when the function is GC'd. We use a Weak finalizer on the
    // function to release the holder — but FunctionTemplate's raw External
    // doesn't have a finalizer hook directly. Instead, we leak the Rc; the
    // function (and its template's data) lives as long as the TS does.
    // Acceptable: the TS keeps the function reachable through the
    // underlyingSource/underlyingSink, and the TS's own weak finalizer
    // doesn't drop these (only its TSStreamState Box).
    //
    // For full hygiene we'd attach the holder to the TS via a separate
    // priv-sym; the current shape mirrors `promise_resolve.rs`'s holder
    // pattern (see microtask_callback).
    //
    // SAFETY caveat: if the TS is GC'd before the function fires, the holder
    // remains reachable via `ext`'s pointer; the function can outlive the
    // TS only by way of being in a still-reachable underlyingSink dict — so
    // by construction, the TS Global the holder carries is also alive.
    std::mem::forget(unsafe { Rc::from_raw(raw as *const MethodHolder) }.clone());
    // Restore the original Rc strong count back to 1 (just `forget`'d the clone).
    f
}

fn method_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'s>,
) {
    let data = args.data();
    let Ok(ext) = v8::Local::<v8::External>::try_from(data) else {
        return;
    };
    let raw = ext.value() as *const MethodHolder;
    if raw.is_null() {
        return;
    }
    // We only borrow the holder; do NOT take ownership (the closure can
    // fire repeatedly — pull is a recurring callback).
    let holder: &MethodHolder = unsafe { &*raw };

    let scope_local_ts = v8::Local::new(scope, &holder.ts);

    match holder.method {
        ReadableMethod::Pull => {
            // source.pull(controller): set backpressure to false (resolve the
            // change-promise so any pending writable transform admits) and
            // return the new pre-pending change-promise so the readable's
            // controller awaits the next "we want more" signal.
            crate::streams::algorithms::transform_stream_set_backpressure(
                scope, scope_local_ts, false,
            );
            let p_g = with_ts_state(scope, scope_local_ts, |s| {
                s.bp_change_promise.borrow().clone()
            })
            .flatten();
            if let Some(p_g) = p_g {
                let p_l = v8::Local::new(scope, &p_g);
                rv.set(p_l.into());
            } else {
                let und = v8::undefined(scope);
                rv.set(und.into());
            }
        }
        ReadableMethod::Cancel => {
            // source.cancel(reason): per spec §5.4.6 (TransformStreamDefault
            // SourceCancelAlgorithm). We compose:
            //  1. controller.[[finishPromise]] becomes the cancel pipeline.
            //  2. TransformStreamErrorWritableAndUnblockWrite(stream, reason).
            //  3. Run cancelAlgorithm(reason); on its settle resolve the
            //     finishPromise.
            //
            // For v1 we run the user's cancel(reason) (or default no-op) and
            // immediately resolve, then call ErrorWritableAndUnblockWrite.
            let reason = args.get(0);
            let p = crate::streams::transform_controller::transform_stream_default_source_cancel(
                scope,
                scope_local_ts,
                reason,
            );
            rv.set(p.into());
        }
        ReadableMethod::Write => {
            // sink.write(chunk, controller): per spec §5.4.6 (TransformStream
            // DefaultSinkWriteAlgorithm). Just call PerformTransform.
            let chunk = args.get(0);
            let p = crate::streams::transform_controller::transform_stream_default_sink_write(
                scope,
                scope_local_ts,
                chunk,
            );
            rv.set(p.into());
        }
        ReadableMethod::Close => {
            // sink.close(): per spec §5.4.6.
            //  1. controller.[[finishPromise]] becomes the close pipeline.
            //  2. Run flush; on fulfill close readable; on reject error TS.
            let p = crate::streams::transform_controller::transform_stream_default_sink_close(
                scope,
                scope_local_ts,
            );
            rv.set(p.into());
        }
        ReadableMethod::Abort => {
            // sink.abort(reason): per spec §5.4.6.
            let reason = args.get(0);
            let p = crate::streams::transform_controller::transform_stream_default_sink_abort(
                scope,
                scope_local_ts,
                reason,
            );
            rv.set(p.into());
        }
    }
}

/// Build a 0-arity function that always returns the captured Promise. Used
/// for start hooks where the underlyingSource/underlyingSink expects a
/// Function whose return value is a Promise (or value coerced to a Promise).
fn build_oneshot_promise_fn<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    p_g: v8::Global<v8::Promise>,
) -> v8::Local<'s, v8::Function> {
    let holder = Rc::new(p_g);
    let raw = Rc::into_raw(holder) as *mut std::ffi::c_void;
    let ext = v8::External::new(scope, raw);
    let tmpl = crate::callback::template_builder(promise_returning_callback)
        .data(ext.into())
        .build(scope);
    let f = tmpl.get_function(scope).unwrap();
    // See note on build_method_fn re: lifetime/leak. We restore the Rc count.
    std::mem::forget(unsafe { Rc::from_raw(raw as *const v8::Global<v8::Promise>) }.clone());
    f
}

fn promise_returning_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'s>,
) {
    let data = args.data();
    let Ok(ext) = v8::Local::<v8::External>::try_from(data) else {
        return;
    };
    let raw = ext.value() as *const v8::Global<v8::Promise>;
    if raw.is_null() {
        return;
    }
    let p_g: &v8::Global<v8::Promise> = unsafe { &*raw };
    let p_l = v8::Local::new(scope, p_g);
    rv.set(p_l.into());
}

// ---------------------------------------------------------------------------
// Public install — wire onto `globalThis`
// ---------------------------------------------------------------------------

pub fn install_native_transform_stream(
    scope: &mut v8::PinScope,
    global: v8::Local<v8::Object>,
) {
    // Macro-emitted FunctionTemplate carries the constructor + must-new
    // prologue + Symbol.toStringTag. We patch in the readable / writable
    // getters on the prototype because they stay raw FunctionCallbacks
    // (need direct args.this() access for priv-sym reads). Same shape as
    // the readers/writer install functions.
    let tmpl = TSStreamState::install(scope);
    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    let proto: v8::Local<v8::Object> = proto_v.try_into().unwrap();

    install_proto_getter(scope, proto, "readable", readable_getter_callback);
    install_proto_getter(scope, proto, "writable", writable_getter_callback);

    let key = v8::String::new(scope, "TransformStream").unwrap();
    global.set(scope, key.into(), class_fn.into());
    intrinsics::capture(scope, Intrinsic::TransformStream, class_fn);
}

fn install_proto_getter(
    scope: &mut v8::PinScope,
    proto: v8::Local<v8::Object>,
    name: &str,
    cb: impl Callback,
) {
    let key = v8::String::new(scope, name).unwrap();
    let getter_tmpl = crate::callback::template(scope, cb);
    let getter_fn = getter_tmpl.get_function(scope).unwrap();
    let mut desc = v8::PropertyDescriptor::new_from_get_set(
        getter_fn.into(),
        v8::undefined(scope).into(),
    );
    desc.set_configurable(true);
    desc.set_enumerable(true);
    proto.define_property(scope, key.into(), &desc);
}
