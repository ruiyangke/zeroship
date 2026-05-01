//! `ReadableStream` — spec §3.2.
//!
//! Hand-rolled rather than macro-driven because every method needs
//! `args.this()` access (the wrapper object) — the macro's getter/method
//! callbacks pass `&self` (a borrow of the boxed state) but not the
//! wrapper itself, and our spec algorithms key off the wrapper's V8
//! identity (private symbols `[[reader]]`, `[[storedError]]`,
//! `[[controller]]`).
//!
//! IDL surface (this dispatch ships the value-path subset):
//! ```webidl
//! [Exposed=*]
//! interface ReadableStream {
//!   constructor(optional object underlyingSource, optional QueuingStrategy strategy = {});
//!   readonly attribute boolean locked;
//!   Promise<undefined> cancel(optional any reason);
//!   ReadableStreamReader getReader(optional ReadableStreamGetReaderOptions options = {});
//!   /* pipeTo / pipeThrough / tee / values — stubbed (next dispatch) */
//! }
//! ```
//!
//! Storage (D-2 audit, design §XV.1):
//! - `state` (Readable / Closed / Errored)            → Rust field on RSState
//! - `disturbed`                                       → Rust field on RSState
//! - `[[reader]]`                                      → V8 priv sym `[[reader]]`
//! - `[[storedError]]`                                 → V8 priv sym `[[storedError]]`
//! - `[[controller]]` (wrapper-in-priv-sym + state)    → V8 priv sym `[[controller]]`
//!
//! The controller's heavy state lives in the controller wrapper's own
//! internal field 0; from the stream we reach it via
//! `crate::streams::readable_default_controller::with_controller_state`.

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;

use crate::state::OpError;
use crate::streams::budget::{try_alloc_stream, StreamBudgetGuard};
use crate::streams::readable_default_controller as ctlr;
use crate::streams::slots::{self, READER};

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// `[[state]]` — spec §3.2.5. The three observable states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    Readable,
    Closed,
    Errored,
}

/// `Box<RSState>` is stored in the wrapper's V8 internal field 0.
/// Per D-2: this struct holds ONLY pure-Rust slots. The
/// `[[controller]]` / `[[reader]]` / `[[storedError]]` slots live in
/// V8 private symbols.
#[allow(missing_debug_implementations)]
pub struct RSState {
    pub state: Cell<StreamState>,
    pub disturbed: Cell<bool>,
    /// D-18 budget guard. Decrements live count on Drop (i.e. when the
    /// V8 weak finalizer reclaims the Box).
    _budget: StreamBudgetGuard,
}

impl RSState {
    pub fn new(budget: StreamBudgetGuard) -> Self {
        Self {
            state: Cell::new(StreamState::Readable),
            disturbed: Cell::new(false),
            _budget: budget,
        }
    }
}

// ---------------------------------------------------------------------------
// V8 wrapper helpers
// ---------------------------------------------------------------------------

/// Confirm `obj` is a ReadableStream wrapper (its internal field 0 is
/// an External pointing at an `RSState`). Used by the public method
/// callbacks for the spec's "If !IsReadableStream(this) throw TypeError"
/// receiver check.
pub fn is_readable_stream(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
) -> bool {
    obj.get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
        .map(|ext| !ext.value().is_null())
        .unwrap_or(false)
}

/// Reach into a JS ReadableStream wrapper's RSState. Returns None if the
/// object is not a ReadableStream (its internal field 0 isn't an External
/// or the External is a null pointer — should never happen in practice).
pub fn with_rs_state<R>(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
    f: impl FnOnce(&RSState) -> R,
) -> Option<R> {
    let raw_v8_field = stream.get_internal_field(scope, 0)?;
    let ext = v8::Local::<v8::External>::try_from(raw_v8_field).ok()?;
    let ptr = ext.value() as *const RSState;
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the External was set during construction to a Box<RSState>
    // (see `build_stream_wrapper` and `constructor_callback`). The Box is
    // dropped only by the V8 weak finalizer, which fires after all JS
    // callbacks complete (single-threaded per isolate).
    let inst = unsafe { &*ptr };
    Some(f(inst))
}

// ---------------------------------------------------------------------------
// from_native_source — Rust-only constructor (D-9, §I.1)
// ---------------------------------------------------------------------------

/// Build a JS ReadableStream from a Rust source.
///
/// **C-12 INVARIANT (`#[doc(hidden)]`):** callers MUST NOT pass a
/// JS-bridged source (one whose `pull`/`cancel` indirectly call a
/// JS-visible ReadableStream). The lock-acquiring public path is the
/// only correct way to wrap a JS-visible stream as a NativeSource.
/// Otherwise this becomes a backdoor that bypasses the spec's lock
/// checks. Pull requests adding such a bridged Source MUST update this
/// invariant.
#[doc(hidden)]
pub fn from_native_source<'s, S: NativeSource + 'static>(
    scope: &mut v8::PinScope<'s, '_>,
    source: S,
    hwm: f64,
) -> v8::Local<'s, v8::Object> {
    let stream = build_stream_wrapper(scope);
    ctlr::set_up_readable_stream_default_controller_native(scope, stream, source, hwm);
    stream
}

/// Public wrapper around `build_stream_wrapper` for internal use by
/// `transform.rs` (TransformStream's readable half is a programmatically-
/// constructed ReadableStream that doesn't go through `new ReadableStream`).
/// Equivalent to invoking the constructor with `undefined` source +
/// strategy then skipping the type-check / strategy-parse — InitializeTransform
/// Stream's caller already parsed the strategy.
#[doc(hidden)]
pub fn build_value_stream_wrapper_for_internal<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::Object> {
    build_stream_wrapper(scope)
}

/// Construct the bare `ReadableStream` JS wrapper — no controller wired.
/// Used by `from_native_source` and by the user-visible constructor.
fn build_stream_wrapper<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::Object> {
    let tmpl = stream_class_template(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let stream_obj = inst_tmpl.new_instance(scope).unwrap();

    let budget = try_alloc_stream().expect("from_native_source: budget exceeded");
    let inst = RSState::new(budget);
    let boxed = Box::new(inst);
    let raw_ptr = Box::into_raw(boxed);
    let raw_addr = raw_ptr as usize;
    let ext = v8::External::new(scope, raw_ptr as *mut std::ffi::c_void);
    stream_obj.set_internal_field(0, ext.into());

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        stream_obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut RSState));
        }),
    );
    std::mem::forget(weak);

    // Wire the prototype. Prefer `globalThis.ReadableStream.prototype`
    // when available (so `branch instanceof ReadableStream` works for
    // tee branches and TS halves). Fall back to the just-built template
    // for tests that call this path before install_native_streams.
    let proto_v = global_class_prototype(scope, "ReadableStream").unwrap_or_else(|| {
        let class_fn = tmpl.get_function(scope).unwrap();
        let proto_key = v8::String::new(scope, "prototype").unwrap();
        class_fn.get(scope, proto_key.into()).unwrap()
    });
    stream_obj.set_prototype(scope, proto_v);

    stream_obj
}

/// Return `globalThis[name].prototype` if globalThis has a `name`
/// property and that property is a Function. Used to wire up
/// internally-built stream wrappers so they share the user-visible
/// class identity (instanceof works).
fn global_class_prototype<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    name: &str,
) -> Option<v8::Local<'s, v8::Value>> {
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, name)?;
    let class_v = global.get(scope, key.into())?;
    let class_obj = v8::Local::<v8::Object>::try_from(class_v).ok()?;
    let proto_key = v8::String::new(scope, "prototype")?;
    class_obj.get(scope, proto_key.into())
}

// ---------------------------------------------------------------------------
// Class template construction
// ---------------------------------------------------------------------------

/// Build the ReadableStream FunctionTemplate. Includes one internal
/// field (Box<RSState>), the constructor callback, and prototype
/// methods.
///
/// We don't cache: per `headers.rs::iter_template`'s notes, FunctionTemplates
/// can't outlive their isolate, and the cost is negligible.
fn stream_class_template<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::FunctionTemplate> {
    let ctor_tmpl = v8::FunctionTemplate::new(scope, constructor_callback);
    let class_name = v8::String::new(scope, "ReadableStream").unwrap();
    ctor_tmpl.set_class_name(class_name);
    ctor_tmpl
        .instance_template(scope)
        .set_internal_field_count(1);

    let proto = ctor_tmpl.prototype_template(scope);

    // locked getter (§3.2.5.1)
    {
        let key = v8::String::new(scope, "locked").unwrap();
        let getter_tmpl = v8::FunctionTemplate::new(scope, locked_getter_callback);
        proto.set_accessor_property(
            key.into(),
            Some(getter_tmpl.into()),
            None,
            v8::PropertyAttribute::NONE,
        );
    }

    // cancel(reason) — §3.2.5.4
    install_method(scope, proto, "cancel", cancel_method_callback);

    // getReader(options?) — §3.2.5.5
    install_method(scope, proto, "getReader", get_reader_method_callback);

    // pipeTo / pipeThrough / tee — implemented via crate::streams::pipe / tee.
    install_method(scope, proto, "pipeTo", pipe_to_method_callback);
    install_method(scope, proto, "pipeThrough", pipe_through_method_callback);
    install_method(scope, proto, "tee", tee_method_callback);
    // values / async-iteration is still deferred (next dispatch).
    install_method(scope, proto, "values", stub_values_callback);

    // Symbol.toStringTag → "ReadableStream" per WebIDL §3.7.4.
    let tag_sym = v8::Symbol::get_to_string_tag(scope);
    let tag_value = v8::String::new(scope, "ReadableStream").unwrap();
    proto.set_with_attr(
        tag_sym.into(),
        tag_value.into(),
        v8::PropertyAttribute::READ_ONLY | v8::PropertyAttribute::DONT_ENUM,
    );

    ctor_tmpl
}

fn install_method(
    scope: &mut v8::PinScope,
    proto: v8::Local<v8::ObjectTemplate>,
    name: &str,
    cb: impl v8::MapFnTo<v8::FunctionCallback>,
) {
    let key = v8::String::new(scope, name).unwrap();
    let tmpl = v8::FunctionTemplate::new(scope, cb);
    proto.set(key.into(), tmpl.into());
}

// ---------------------------------------------------------------------------
// constructor — `new ReadableStream(underlyingSource?, strategy?)`
// ---------------------------------------------------------------------------

fn constructor_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    if !args.is_construct_call() {
        let msg = v8::String::new(scope, "ReadableStream: must be called with 'new'").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }

    let stream_obj = args.this();
    let underlying_source = args.get(0);
    let strategy = args.get(1);

    // Per spec §3.2.1 step 1: if underlyingSource is null → TypeError.
    // (undefined is allowed; null is not — caught by WebIDL's `object`
    // type which doesn't accept null.)
    if underlying_source.is_null() {
        let msg = v8::String::new(scope, "ReadableStream: underlyingSource may not be null").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }

    // Per WebIDL — strategy + underlyingSource are converted in interleaved
    // order: strategy is converted at the IDL layer (so its `size`/`highWaterMark`
    // getters fire FIRST), then underlyingSource is converted in prose
    // (start/pull/cancel/type accessors fire after). This means a throwing
    // strategy.size getter wins over a throwing underlyingSource.start
    // getter — see WPT constructor.any.js "underlyingSource argument should
    // be converted after queuingStrategy argument".
    //
    // We probe strategy access first (parse_strategy reads highWaterMark
    // and size), then underlyingSource. If either throws, the exception
    // propagates via tc_scope set on call.

    // Parse strategy first (per spec). On error, propagate.
    let (hwm, size_algo) = match crate::streams::readable::parse_strategy_local(scope, strategy, 1.0) {
        Ok(v) => v,
        Err(()) => {
            // parse_strategy already pushed an exception via the V8 try-catch
            // mechanism (the getter throw propagates through `obj.get`).
            return;
        }
    };

    // Read `type` field on underlyingSource for the bytes/default branch.
    let mut is_byte_stream = false;
    if let Ok(us) = v8::Local::<v8::Object>::try_from(underlying_source) {
        let type_key = v8::String::new(scope, "type").unwrap();
        let type_v = match us.get(scope, type_key.into()) {
            Some(v) => v,
            None => return,
        };
        if !type_v.is_undefined() {
            // Per spec: ToString(type) — null/'' coerce. Then compare to
            // "bytes". Anything else → TypeError (not RangeError).
            let s_opt = type_v.to_string(scope);
            let Some(s_v) = s_opt else { return };
            let s = s_v.to_rust_string_lossy(scope);
            if s == "bytes" {
                is_byte_stream = true;
            } else {
                let msg = v8::String::new(scope, "ReadableStream: invalid underlyingSource.type")
                    .unwrap();
                let exc = v8::Exception::type_error(scope, msg);
                scope.throw_exception(exc);
                return;
            }
        }
    }
    // Per spec §3.2.4: byte streams default HWM = 0. The default-stream
    // path used 1.0 above; if the stream is byte-typed, override the
    // default. Also: byte streams forbid a custom `size` callback in the
    // strategy.
    if is_byte_stream {
        if !matches!(size_algo, ctlr::SizeAlgorithm::DefaultCount) {
            let msg = v8::String::new(
                scope,
                "ReadableStream(type=\"bytes\"): strategy.size cannot be specified",
            )
            .unwrap();
            let exc = v8::Exception::range_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
        // Default HWM=0 for byte streams unless explicitly set. Distinguish
        // user-supplied vs. defaulted: parse_strategy_local returned the
        // default 1.0; if strategy was undefined OR strategy.highWaterMark
        // was undefined, override. We can't tell easily here, so re-parse
        // with default 0 instead.
        let (hwm0, _) = match crate::streams::readable::parse_strategy_local(scope, strategy, 0.0) {
            Ok(v) => v,
            Err(()) => return,
        };
        // Use hwm0 below.
        let _ = hwm; // suppress unused warning
        let hwm = hwm0;

        // Allocate budget + Box<RSState>.
        let budget = match try_alloc_stream() {
            Ok(g) => g,
            Err(m) => {
                let msg = v8::String::new(scope, m).unwrap();
                let exc = v8::Exception::range_error(scope, msg);
                scope.throw_exception(exc);
                return;
            }
        };
        let inst = RSState::new(budget);
        let boxed = Box::new(inst);
        let raw_ptr = Box::into_raw(boxed);
        let raw_addr = raw_ptr as usize;
        let ext = v8::External::new(scope, raw_ptr as *mut std::ffi::c_void);
        stream_obj.set_internal_field(0, ext.into());
        let weak = v8::Weak::with_guaranteed_finalizer(
            scope,
            stream_obj,
            Box::new(move || unsafe {
                drop(Box::from_raw(raw_addr as *mut RSState));
            }),
        );
        std::mem::forget(weak);

        if let Err(msg) = crate::streams::readable_byte_controller::set_up_readable_byte_stream_controller_from_underlying_source(
            scope,
            stream_obj,
            underlying_source,
            hwm,
        ) {
            let v8_msg = v8::String::new(scope, &msg).unwrap();
            let exc = v8::Exception::type_error(scope, v8_msg);
            scope.throw_exception(exc);
        }
        return;
    }

    // Allocate budget + Box<RSState>.
    let budget = match try_alloc_stream() {
        Ok(g) => g,
        Err(m) => {
            let msg = v8::String::new(scope, m).unwrap();
            let exc = v8::Exception::range_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };
    let inst = RSState::new(budget);
    let boxed = Box::new(inst);
    let raw_ptr = Box::into_raw(boxed);
    let raw_addr = raw_ptr as usize;
    let ext = v8::External::new(scope, raw_ptr as *mut std::ffi::c_void);
    stream_obj.set_internal_field(0, ext.into());
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        stream_obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut RSState));
        }),
    );
    std::mem::forget(weak);

    // Wire the controller (default-only in this dispatch). Errors are
    // surfaced as TypeError per spec; the boxed RSState is retained so the
    // weak finalizer can drop it on GC even if construction throws here.
    if let Err(msg) = ctlr::set_up_readable_stream_default_controller_from_underlying_source_with_strategy(
        scope,
        stream_obj,
        underlying_source,
        hwm,
        size_algo,
    ) {
        let v8_msg = v8::String::new(scope, &msg).unwrap();
        let exc = v8::Exception::type_error(scope, v8_msg);
        scope.throw_exception(exc);
    }
}

// ---------------------------------------------------------------------------
// `locked` getter (§3.2.5.1)
// ---------------------------------------------------------------------------

fn locked_getter_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    if !is_readable_stream(scope, this) {
        let msg = v8::String::new(scope, "ReadableStream.locked: receiver is not a ReadableStream").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    let locked = !slots::slot_is_empty(scope, this, READER);
    rv.set(v8::Boolean::new(scope, locked).into());
}

// ---------------------------------------------------------------------------
// `cancel(reason)` (§3.2.5.4)
// ---------------------------------------------------------------------------

fn cancel_method_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'s>,
) {
    let this = args.this();
    if !is_readable_stream(scope, this) {
        let msg = v8::String::new(scope, "ReadableStream.cancel: receiver is not a ReadableStream").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    if crate::streams::algorithms::is_readable_stream_locked(scope, this) {
        // Spec: return promiseRejectedWith TypeError("cannot cancel a locked stream").
        let msg = v8::String::new(scope, "ReadableStream.cancel: stream is locked").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    let reason = args.get(0);
    let promise = crate::streams::algorithms::readable_stream_cancel(scope, this, reason);
    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// `getReader(options?)` (§3.2.5.5)
// ---------------------------------------------------------------------------

fn get_reader_method_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    if !is_readable_stream(scope, this) {
        let msg = v8::String::new(scope, "ReadableStream.getReader: receiver is not a ReadableStream").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    // Parse options.
    let options = args.get(0);
    let mut mode_byob = false;
    if !options.is_undefined() {
        let Ok(options_obj) = v8::Local::<v8::Object>::try_from(options) else {
            let msg = v8::String::new(scope, "getReader: options must be an object").unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return;
        };
        let mode_key = v8::String::new(scope, "mode").unwrap();
        let mode_v = options_obj
            .get(scope, mode_key.into())
            .unwrap_or_else(|| v8::undefined(scope).into());
        if !mode_v.is_undefined() {
            let s = mode_v.to_rust_string_lossy(scope);
            if s == "byob" {
                mode_byob = true;
            } else {
                // Anything else → TypeError per spec.
                let msg = v8::String::new(scope, "getReader: invalid mode").unwrap();
                let exc = v8::Exception::type_error(scope, msg);
                scope.throw_exception(exc);
                return;
            }
        }
    }
    if mode_byob {
        let reader = match crate::streams::readable_byob_reader::acquire_readable_stream_byob_reader(
            scope, this,
        ) {
            Ok(r) => r,
            Err(err) => {
                let msg = v8::String::new(scope, &err).unwrap();
                let exc = v8::Exception::type_error(scope, msg);
                scope.throw_exception(exc);
                return;
            }
        };
        rv.set(reader.into());
        return;
    }

    let reader = match crate::streams::readable_default_reader::acquire_readable_stream_default_reader(
        scope, this,
    ) {
        Ok(r) => r,
        Err(err) => {
            let msg = v8::String::new(scope, &err).unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };
    rv.set(reader.into());
}

// ---------------------------------------------------------------------------
// pipeTo / pipeThrough / tee — wired to crate::streams::{pipe, tee}.
// values / asyncIterator — still stubbed (lands with the iteration support).
// ---------------------------------------------------------------------------

fn throw_not_implemented(scope: &mut v8::PinScope, name: &str) {
    let msg = format!("ReadableStream.{name}: not implemented in this landing — see next dispatch");
    let v8_msg = v8::String::new(scope, &msg).unwrap();
    let exc = v8::Exception::error(scope, v8_msg);
    scope.throw_exception(exc);
}

/// `pipeTo(dest, options)` per spec §3.2.5.7.
///
/// Validation (spec):
///   1. Receiver MUST be a ReadableStream.
///   2. dest MUST be a WritableStream.
///   3. signal (in options) MUST be undefined or an AbortSignal-like.
///   4. If `this` is locked → reject with TypeError.
///   5. If dest is locked → reject with TypeError.
///   6. Else: return ReadableStreamPipeTo(this, dest, prevent…, signal).
fn pipe_to_method_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'s>,
) {
    let this = args.this();
    if !is_readable_stream(scope, this) {
        let msg = v8::String::new(scope, "pipeTo: receiver is not a ReadableStream").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let p = crate::streams::algorithms::rejected_with_promise(scope, exc.into());
        rv.set(p.into());
        return;
    }
    let dest_v = args.get(0);
    let Ok(dest) = v8::Local::<v8::Object>::try_from(dest_v) else {
        let msg = v8::String::new(scope, "pipeTo: argument 1 must be a WritableStream").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let p = crate::streams::algorithms::rejected_with_promise(scope, exc.into());
        rv.set(p.into());
        return;
    };
    if !crate::streams::writable::is_writable_stream(scope, dest) {
        let msg = v8::String::new(scope, "pipeTo: argument 1 must be a WritableStream").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let p = crate::streams::algorithms::rejected_with_promise(scope, exc.into());
        rv.set(p.into());
        return;
    }

    let (prevent_close, prevent_abort, prevent_cancel, signal) =
        match parse_pipe_options(scope, args.get(1)) {
            Ok(v) => v,
            Err(p) => {
                rv.set(p.into());
                return;
            }
        };

    if crate::streams::algorithms::is_readable_stream_locked(scope, this) {
        let msg = v8::String::new(scope, "pipeTo: source is locked").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let p = crate::streams::algorithms::rejected_with_promise(scope, exc.into());
        rv.set(p.into());
        return;
    }
    if crate::streams::algorithms::is_writable_stream_locked(scope, dest) {
        let msg = v8::String::new(scope, "pipeTo: destination is locked").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let p = crate::streams::algorithms::rejected_with_promise(scope, exc.into());
        rv.set(p.into());
        return;
    }

    let p = crate::streams::pipe::readable_stream_pipe_to(
        scope,
        this,
        dest,
        prevent_close,
        prevent_abort,
        prevent_cancel,
        signal,
    );
    rv.set(p.into());
}

/// `pipeThrough(transform, options)` per spec §3.2.5.6.
///
/// 1. Receiver check.
/// 2. `transform` must have `readable` (ReadableStream) + `writable`
///    (WritableStream) properties — i.e. a TransformStream or duck-type.
/// 3. Lock checks: this and transform.writable must be unlocked, else
///    THROW (sync) — pipeThrough is the rare method where lock errors
///    surface as synchronous throws, NOT promise rejections.
/// 4. Spawn a pipeTo into transform.writable; setPromiseIsHandledToTrue
///    on the result. Return transform.readable.
fn pipe_through_method_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'s>,
) {
    let this = args.this();
    if !is_readable_stream(scope, this) {
        let msg = v8::String::new(scope, "pipeThrough: receiver is not a ReadableStream").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    let transform_v = args.get(0);
    let Ok(transform_obj) = v8::Local::<v8::Object>::try_from(transform_v) else {
        let msg = v8::String::new(
            scope,
            "pipeThrough: argument 1 must be a { readable, writable } pair",
        )
        .unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    };
    let readable_key = v8::String::new(scope, "readable").unwrap();
    let writable_key = v8::String::new(scope, "writable").unwrap();
    let readable_v = transform_obj
        .get(scope, readable_key.into())
        .unwrap_or_else(|| v8::undefined(scope).into());
    let writable_v = transform_obj
        .get(scope, writable_key.into())
        .unwrap_or_else(|| v8::undefined(scope).into());
    let Ok(readable_obj) = v8::Local::<v8::Object>::try_from(readable_v) else {
        let msg = v8::String::new(
            scope,
            "pipeThrough: argument 1 has no ReadableStream readable side",
        )
        .unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    };
    let Ok(writable_obj) = v8::Local::<v8::Object>::try_from(writable_v) else {
        let msg = v8::String::new(
            scope,
            "pipeThrough: argument 1 has no WritableStream writable side",
        )
        .unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    };
    if !is_readable_stream(scope, readable_obj) {
        let msg = v8::String::new(
            scope,
            "pipeThrough: transform.readable is not a ReadableStream",
        )
        .unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    if !crate::streams::writable::is_writable_stream(scope, writable_obj) {
        let msg = v8::String::new(
            scope,
            "pipeThrough: transform.writable is not a WritableStream",
        )
        .unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }

    let (prevent_close, prevent_abort, prevent_cancel, signal) =
        match parse_pipe_options_throw(scope, args.get(1)) {
            Ok(v) => v,
            Err(()) => return, // already pushed exception
        };

    if crate::streams::algorithms::is_readable_stream_locked(scope, this) {
        let msg = v8::String::new(scope, "pipeThrough: source is locked").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    if crate::streams::algorithms::is_writable_stream_locked(scope, writable_obj) {
        let msg = v8::String::new(scope, "pipeThrough: transform.writable is locked").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }

    let pipe_promise = crate::streams::pipe::readable_stream_pipe_to(
        scope,
        this,
        writable_obj,
        prevent_close,
        prevent_abort,
        prevent_cancel,
        signal,
    );
    crate::streams::promise_resolve::set_promise_is_handled_to_true(scope, pipe_promise);

    rv.set(readable_obj.into());
}

/// `tee()` per spec §3.2.5.8 — return [branch1, branch2].
fn tee_method_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'s>,
) {
    let this = args.this();
    if !is_readable_stream(scope, this) {
        let msg = v8::String::new(scope, "tee: receiver is not a ReadableStream").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    if crate::streams::algorithms::is_readable_stream_locked(scope, this) {
        let msg = v8::String::new(scope, "tee: stream is locked").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    match crate::streams::tee::readable_stream_default_tee(scope, this, false) {
        Ok([b1, b2]) => {
            let arr = v8::Array::new(scope, 2);
            arr.set_index(scope, 0, b1.into());
            arr.set_index(scope, 1, b2.into());
            rv.set(arr.into());
        }
        Err(msg) => {
            let v8_msg = v8::String::new(scope, &msg).unwrap();
            let exc = v8::Exception::type_error(scope, v8_msg);
            scope.throw_exception(exc);
        }
    }
}

fn stub_values_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    throw_not_implemented(scope, "values");
}

// ---------------------------------------------------------------------------
// pipe options parsing
// ---------------------------------------------------------------------------

/// Parse `{preventClose, preventAbort, preventCancel, signal}` from a
/// JS value. Returns a rejected Promise if the options object is
/// malformed (so pipeTo can `rv.set(p)` and bail).
type PipeOptsParsed<'s> = (bool, bool, bool, Option<v8::Local<'s, v8::Object>>);

fn parse_pipe_options<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    options: v8::Local<v8::Value>,
) -> Result<PipeOptsParsed<'s>, v8::Local<'s, v8::Promise>> {
    // Per WebIDL "optional dictionary" semantics: undefined/null/missing
    // → empty dict.
    if options.is_undefined() || options.is_null() {
        return Ok((false, false, false, None));
    }
    let Ok(obj) = v8::Local::<v8::Object>::try_from(options) else {
        let msg = v8::String::new(scope, "pipeTo: options must be an object").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        return Err(crate::streams::algorithms::rejected_with_promise(
            scope,
            exc.into(),
        ));
    };
    let prevent_close = bool_field(scope, obj, "preventClose");
    let prevent_abort = bool_field(scope, obj, "preventAbort");
    let prevent_cancel = bool_field(scope, obj, "preventCancel");
    // `signal_field` pushes an exception via scope.throw on validation
    // failure; pipeTo's IDL surface returns a rejected Promise instead
    // of throwing. We catch the exception and convert it.
    let exc_g_opt: Option<v8::Global<v8::Value>> = {
        let mut captured: Option<v8::Global<v8::Value>> = None;
        let signal_result = {
            v8::tc_scope!(let tc, scope);
            let r = signal_field(tc, obj);
            if r.is_err() {
                if let Some(exc) = tc.exception() {
                    captured = Some(v8::Global::new(tc, exc));
                }
            }
            r
        };
        match signal_result {
            Ok(s) => return Ok((prevent_close, prevent_abort, prevent_cancel, s)),
            Err(()) => captured.or_else(|| {
                let msg = v8::String::new(scope, "options.signal").unwrap();
                let exc = v8::Exception::type_error(scope, msg);
                let exc_v: v8::Local<v8::Value> = exc;
                Some(v8::Global::new(scope, exc_v))
            }),
        }
    };
    let exc_g = exc_g_opt.unwrap();
    let exc = v8::Local::new(scope, &exc_g);
    Err(crate::streams::algorithms::rejected_with_promise(scope, exc))
}

/// Same as `parse_pipe_options` but for pipeThrough — errors are
/// thrown synchronously instead of returned as rejected promises.
fn parse_pipe_options_throw<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    options: v8::Local<v8::Value>,
) -> Result<PipeOptsParsed<'s>, ()> {
    if options.is_undefined() || options.is_null() {
        return Ok((false, false, false, None));
    }
    let Ok(obj) = v8::Local::<v8::Object>::try_from(options) else {
        let msg = v8::String::new(scope, "pipeThrough: options must be an object").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return Err(());
    };
    let prevent_close = bool_field(scope, obj, "preventClose");
    let prevent_abort = bool_field(scope, obj, "preventAbort");
    let prevent_cancel = bool_field(scope, obj, "preventCancel");
    let signal = match signal_field(scope, obj) {
        Ok(s) => s,
        Err(()) => return Err(()),
    };
    Ok((prevent_close, prevent_abort, prevent_cancel, signal))
}

fn bool_field(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>, name: &str) -> bool {
    let key = v8::String::new(scope, name).unwrap();
    let v = obj.get(scope, key.into()).unwrap_or_else(|| v8::undefined(scope).into());
    v.boolean_value(scope)
}

/// Read `options.signal`. Returns:
///   - Ok(None) when signal is absent (undefined).
///   - Ok(Some(obj)) when signal is an AbortSignal-like Object (duck-
///     typed: must have an `aborted` property and `addEventListener`).
///   - Err(()) when signal is supplied but isn't a valid AbortSignal —
///     the caller pushes a TypeError exception via the scope.
///
/// Per WebIDL: dictionary member `signal: AbortSignal` (non-nullable)
/// rejects null/non-AbortSignal supplied values with TypeError.
fn signal_field<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    obj: v8::Local<v8::Object>,
) -> Result<Option<v8::Local<'s, v8::Object>>, ()> {
    let key = v8::String::new(scope, "signal").unwrap();
    let v = obj.get(scope, key.into()).unwrap_or_else(|| v8::undefined(scope).into());
    if v.is_undefined() {
        return Ok(None);
    }
    let Ok(sig_obj) = v8::Local::<v8::Object>::try_from(v) else {
        // Not even an Object (null counts as Value::null which fails
        // try_from<Object>). TypeError per WebIDL.
        let msg = v8::String::new(scope, "options.signal must be an AbortSignal").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return Err(());
    };
    // Duck-type check: must have `aborted` and `addEventListener`.
    // The WPT tests pass `null` (Object null check rejects above), `true`
    // (not an Object), `AbortSignal` itself (a function — try_from<Object>
    // succeeds since Function ⊂ Object, but it has no `aborted` getter
    // bound to an instance), `-1` (not an Object), etc.
    let aborted_key = v8::String::new(scope, "aborted").unwrap();
    let add_key = v8::String::new(scope, "addEventListener").unwrap();
    let has_aborted = sig_obj.has(scope, aborted_key.into()).unwrap_or(false);
    let has_add = sig_obj
        .get(scope, add_key.into())
        .map(|v| v.is_function())
        .unwrap_or(false);
    if !has_aborted || !has_add {
        let msg = v8::String::new(scope, "options.signal must be an AbortSignal").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return Err(());
    }
    Ok(Some(sig_obj))
}

// ---------------------------------------------------------------------------
// Strategy parsing — Extract* helpers (§7)
// ---------------------------------------------------------------------------

/// Parse a `QueuingStrategy { highWaterMark, size }` object.
/// Returns extracted (highWaterMark, sizeAlgorithm) per §7.2 / §7.3.
pub fn parse_strategy(
    scope: &mut v8::PinScope,
    strategy: v8::Local<v8::Value>,
    default_hwm: f64,
) -> Result<(f64, ctlr::SizeAlgorithm), OpError> {
    if strategy.is_undefined() {
        return Ok((default_hwm, ctlr::SizeAlgorithm::DefaultCount));
    }
    let Ok(obj) = v8::Local::<v8::Object>::try_from(strategy) else {
        return Err(OpError::type_error("ReadableStream: strategy must be an object"));
    };

    let hwm_key = v8::String::new(scope, "highWaterMark").unwrap();
    let hwm_v = obj
        .get(scope, hwm_key.into())
        .unwrap_or_else(|| v8::undefined(scope).into());
    let hwm = if hwm_v.is_undefined() {
        default_hwm
    } else {
        let n = hwm_v
            .number_value(scope)
            .ok_or_else(|| OpError::type_error("highWaterMark must be a number"))?;
        if n.is_nan() {
            return Err(OpError::range_error("highWaterMark must not be NaN"));
        }
        if n < 0.0 {
            return Err(OpError::range_error("highWaterMark must be non-negative"));
        }
        n
    };

    let size_key = v8::String::new(scope, "size").unwrap();
    let size_v = obj
        .get(scope, size_key.into())
        .unwrap_or_else(|| v8::undefined(scope).into());
    let size = if size_v.is_undefined() {
        ctlr::SizeAlgorithm::DefaultCount
    } else {
        let Ok(fn_l) = v8::Local::<v8::Function>::try_from(size_v) else {
            return Err(OpError::type_error("strategy.size must be a function"));
        };
        ctlr::SizeAlgorithm::Js(v8::Global::new(scope, fn_l))
    };

    Ok((hwm, size))
}

/// Same as `parse_strategy` but on error pushes an exception via
/// `scope.throw_exception` and returns `Err(())` so the V8 callback can
/// `return` immediately. Used by the constructor where a throwing
/// strategy getter must propagate as the construction failure (per WPT
/// `constructor.any.js` "underlyingSource argument should be converted
/// after queuingStrategy argument"). Reads `size` BEFORE `highWaterMark`
/// to match the reference implementation's getter ordering.
pub(crate) fn parse_strategy_local(
    scope: &mut v8::PinScope,
    strategy: v8::Local<v8::Value>,
    default_hwm: f64,
) -> Result<(f64, ctlr::SizeAlgorithm), ()> {
    if strategy.is_undefined() {
        return Ok((default_hwm, ctlr::SizeAlgorithm::DefaultCount));
    }
    let Ok(obj) = v8::Local::<v8::Object>::try_from(strategy) else {
        let msg = v8::String::new(scope, "ReadableStream: strategy must be an object").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return Err(());
    };
    // Per WPT constructor.any.js: queuingStrategy is converted at the
    // IDL layer before underlyingSource. Within the strategy dict, the
    // ref impl reads `size` BEFORE `highWaterMark`, so a throwing
    // size-getter wins over a throwing hwm-getter (and over a throwing
    // underlyingSource start-getter).
    let size_key = v8::String::new(scope, "size").unwrap();
    let size_v = match obj.get(scope, size_key.into()) {
        Some(v) => v,
        None => return Err(()),
    };
    let hwm_key = v8::String::new(scope, "highWaterMark").unwrap();
    let hwm_v = match obj.get(scope, hwm_key.into()) {
        Some(v) => v,
        None => return Err(()),
    };

    let hwm = if hwm_v.is_undefined() {
        default_hwm
    } else {
        let Some(n) = hwm_v.number_value(scope) else {
            return Err(());
        };
        if n.is_nan() {
            let msg = v8::String::new(scope, "highWaterMark must not be NaN").unwrap();
            let exc = v8::Exception::range_error(scope, msg);
            scope.throw_exception(exc);
            return Err(());
        }
        if n < 0.0 {
            let msg = v8::String::new(scope, "highWaterMark must be non-negative").unwrap();
            let exc = v8::Exception::range_error(scope, msg);
            scope.throw_exception(exc);
            return Err(());
        }
        n
    };

    let size = if size_v.is_undefined() {
        ctlr::SizeAlgorithm::DefaultCount
    } else {
        let Ok(fn_l) = v8::Local::<v8::Function>::try_from(size_v) else {
            let msg = v8::String::new(scope, "strategy.size must be a function").unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return Err(());
        };
        ctlr::SizeAlgorithm::Js(v8::Global::new(scope, fn_l))
    };
    Ok((hwm, size))
}

// ---------------------------------------------------------------------------
// NativeSource trait — §VIII.1
// ---------------------------------------------------------------------------

/// Native UnderlyingSource — Rust trait that mirrors WebIDL's
/// `UnderlyingSource` dictionary, but typed.
///
/// Per design §VIII.1. Used by `from_native_source` to build a JS-visible
/// ReadableStream from a Rust producer (e.g. fetch body bytes,
/// compression decoder output).
pub trait NativeSource: 'static {
    fn start(
        &mut self,
        _controller: &mut NativeReadableController,
    ) -> Result<(), v8::Global<v8::Value>> {
        Ok(())
    }

    fn pull(
        &mut self,
        controller: &mut NativeReadableController,
    ) -> Pin<Box<dyn Future<Output = Result<(), v8::Global<v8::Value>>> + 'static>>;

    fn cancel(
        &mut self,
        _reason: Option<v8::Global<v8::Value>>,
    ) -> Pin<Box<dyn Future<Output = Result<(), v8::Global<v8::Value>>> + 'static>> {
        Box::pin(async { Ok(()) })
    }
}

/// Thin wrapper around the controller wrapper, exposed to NativeSource
/// trait impls. The `enqueue`/`error`/`close` methods reach into the
/// controller through normal spec algorithm calls.
#[allow(missing_debug_implementations)]
pub struct NativeReadableController {
    /// V8 wrapper of the ReadableStreamDefaultController. Re-localized
    /// on every method call so the Global stays alive across async
    /// boundaries.
    pub(crate) controller_obj: v8::Global<v8::Object>,
}

impl NativeReadableController {
    pub fn enqueue<'s>(
        &mut self,
        scope: &mut v8::PinScope<'s, '_>,
        chunk: v8::Local<'s, v8::Value>,
    ) -> Result<(), v8::Global<v8::Value>> {
        let controller_obj = v8::Local::new(scope, &self.controller_obj);
        ctlr::readable_stream_default_controller_enqueue(scope, controller_obj, chunk)
    }

    pub fn close(&mut self, scope: &mut v8::PinScope) {
        let controller_obj = v8::Local::new(scope, &self.controller_obj);
        ctlr::readable_stream_default_controller_close(scope, controller_obj);
    }

    pub fn error<'s>(
        &mut self,
        scope: &mut v8::PinScope<'s, '_>,
        reason: v8::Local<'s, v8::Value>,
    ) {
        let controller_obj = v8::Local::new(scope, &self.controller_obj);
        ctlr::readable_stream_default_controller_error(scope, controller_obj, reason);
    }

    pub fn desired_size(&self, scope: &mut v8::PinScope) -> Option<f64> {
        let controller_obj = v8::Local::new(scope, &self.controller_obj);
        ctlr::readable_stream_default_controller_get_desired_size(scope, controller_obj)
    }
}

// ---------------------------------------------------------------------------
// Public install — wire onto `globalThis`
// ---------------------------------------------------------------------------

/// Install `globalThis.ReadableStream`, `…DefaultReader`,
/// `…DefaultController`. Test harnesses call this directly; production
/// `setup_globals` wires it once the polyfill cutover (D-19) lands.
pub fn install_native_streams(
    scope: &mut v8::PinScope,
    global: v8::Local<v8::Object>,
) {
    let stream_tmpl = stream_class_template(scope);
    let stream_class_fn = stream_tmpl.get_function(scope).unwrap();
    let key = v8::String::new(scope, "ReadableStream").unwrap();
    global.set(scope, key.into(), stream_class_fn.into());

    crate::streams::readable_default_controller::install(scope, global);
    crate::streams::readable_default_reader::install(scope, global);
}
