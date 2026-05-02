//! `ReadableStreamAsyncIterator` — spec §3.4.6 + design §IV.
//!
//! IDL surface (§3.2):
//! ```webidl
//! [Exposed=*]
//! interface ReadableStream {
//!   ...
//!   async iterable<any>(optional ReadableStreamIteratorOptions options = {});
//! };
//!
//! dictionary ReadableStreamIteratorOptions {
//!   boolean preventCancel = false;
//! };
//! ```
//!
//! WebIDL §3.7.10 expands `async iterable` into:
//!   - `Symbol.asyncIterator` on the prototype (calls `values()` with no args)
//!   - `values(options)` returns a fresh async iterator object
//!
//! The iterator object exposes `next()` and `return(value)` (no `throw`),
//! both async; its `[[Prototype]]` is `%AsyncIteratorPrototype%`. Its
//! `@@toStringTag` is `"ReadableStream Async Iterator"`.
//!
//! ## Storage (D-2 + D-8)
//!
//! Per design D-8, "finished" is **derived** from `reader.[[stream]] === undefined`
//! post-release — there is **no** `is_finished` slot on the iterator. The
//! iterator stores:
//!
//! - `reader` (the default reader bound to the source stream) — V8 priv sym
//!   `[[reader]]` on the iterator wrapper.
//! - `prevent_cancel` (boolean from options) — a Rust field on
//!   `Box<AsyncIterState>` in internal field 0.
//!
//! ## Spec algorithms shipped here
//!
//! - **`values(options)`** (§3.2.5.9): acquire a default reader on the stream,
//!   build an iterator wrapper bound to (reader, preventCancel), return it.
//! - **`next()`** (§3.4.6 step 4 / WebIDL `next iteration result`):
//!   - if `reader.[[stream]] === undefined` → resolved `{value: undefined, done: true}`.
//!   - else issue a Native ReadRequest; chunkSteps fulfill with `{value, done:false}`,
//!     closeSteps release the reader and fulfill with `{value: undefined, done: true}`,
//!     errorSteps release the reader and reject.
//! - **`return(value)`** (§3.4.6 step 5):
//!   - if `reader.[[stream]] === undefined` → resolved `{value, done: true}`.
//!   - else if `preventCancel` → release reader, resolve `{value, done: true}`.
//!   - else: cancel the source via `ReadableStreamReaderGenericCancel(reader, value)`,
//!     release the reader, then map cancelPromise's fulfillment to `{value, done: true}`
//!     (or rejection passes through).
//!
//! Per WebIDL §3.7.10, the implementation algorithm for `next()` returns a
//! "next iteration result" promise; the WebIDL machinery wraps it as
//! `{value, done}`. Since we're not running the IDL machinery, we materialize
//! the iterator-result object directly inside the implementation.

use std::cell::Cell;

use crate::streams::algorithms;
use crate::streams::readable_default_reader::{
    self as default_reader, ReadRequest, ReadRequestKind, ReadRequestNative,
};
use crate::streams::slots;

// ---------------------------------------------------------------------------
// Slot names (private to this module)
// ---------------------------------------------------------------------------

/// `ReadableStreamAsyncIterator.[[reader]]` — the default reader the
/// iterator drives.
const ITER_READER: &str = "[[asyncIter.reader]]";

/// Tag bit so `is_async_iterator` can confirm the wrapper type without
/// risking a false positive against another single-internal-field class.
const ITER_TAG: &str = "[[asyncIter.tag]]";

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// `Box<AsyncIterState>` lives in the iterator wrapper's internal field 0.
///
/// Per D-8: NO `is_finished` slot — finished is derived from
/// `reader.[[stream]] === undefined`.
#[allow(missing_debug_implementations)]
pub struct AsyncIterState {
    /// `[[preventCancel]]` from the options dict (§3.2.5.9 step 5).
    pub prevent_cancel: Cell<bool>,
    /// `[[ongoingPromise]]` per ref impl `ReadableStreamAsyncIterator-impl.js`.
    ///
    /// Each call to `next()` / `return()` chains onto this promise so
    /// concurrent invocations are processed in order. Initially undefined.
    /// Stored as Cell<Option<Global<Promise>>> for interior mutability.
    pub ongoing_promise: std::cell::RefCell<Option<v8::Global<v8::Promise>>>,
}

impl AsyncIterState {
    fn new(prevent_cancel: bool) -> Self {
        Self {
            prevent_cancel: Cell::new(prevent_cancel),
            ongoing_promise: std::cell::RefCell::new(None),
        }
    }
}

// ---------------------------------------------------------------------------
// V8 wrapper helpers
// ---------------------------------------------------------------------------

fn is_async_iterator(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>) -> bool {
    let has_state = obj
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
        .map(|e| !e.value().is_null())
        .unwrap_or(false);
    if !has_state {
        return false;
    }
    !slots::slot_is_empty(scope, obj, ITER_TAG)
}

fn with_state<R>(
    scope: &mut v8::PinScope,
    iter: v8::Local<v8::Object>,
    f: impl FnOnce(&AsyncIterState) -> R,
) -> Option<R> {
    let raw_v8_field = iter.get_internal_field(scope, 0)?;
    let ext = v8::Local::<v8::External>::try_from(raw_v8_field).ok()?;
    let ptr = ext.value() as *const AsyncIterState;
    if ptr.is_null() {
        return None;
    }
    // SAFETY: External pointer set during `build` to a Box<AsyncIterState>;
    // dropped only by the V8 weak finalizer.
    let inst = unsafe { &*ptr };
    Some(f(inst))
}

/// Returns the AsyncIteratorPrototype intrinsic for this realm.
///
/// `%AsyncIteratorPrototype%` is the prototype of all async iterators (and
/// async generator instances). It's not directly exposed on `globalThis`
/// but is reachable by walking the prototype chain of an async-generator
/// instance: `async function*(){}.prototype.__proto__ ===
/// %AsyncIteratorPrototype%`.
fn async_iterator_prototype<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> Option<v8::Local<'s, v8::Value>> {
    let src = v8::String::new(
        scope,
        "Object.getPrototypeOf(Object.getPrototypeOf(async function*(){}).prototype)",
    )?;
    let script = v8::Script::compile(scope, src, None)?;
    script.run(scope)
}

// ---------------------------------------------------------------------------
// Iterator prototype object
// ---------------------------------------------------------------------------
//
// Per spec §3.4.6 + WebIDL §3.7.10.4, the iterator object's prototype has
// EXACTLY two own properties: `next` and `return` (both data properties).
// It also has `@@toStringTag` (DONT_ENUM, so it's hidden from
// `Object.getOwnPropertyNames`). It does NOT have a `constructor` property
// (unlike a typical class — there's no public constructor or static class
// for the iterator).
//
// Because `FunctionTemplate.prototype_template()` always installs a
// `constructor` accessor, we build the iterator prototype as a plain
// `Object` and walk its prototype to `%AsyncIteratorPrototype%`. The
// instance template stays a separate ObjectTemplate (for the internal
// field), with its prototype set explicitly at allocation time.

/// Build (or retrieve from a cached realm slot) the iterator prototype
/// object: a plain object with own properties `next`, `return`,
/// `@@toStringTag`, whose `[[Prototype]]` is `%AsyncIteratorPrototype%`.
///
/// Cached on the global object via a private symbol so repeated `values()`
/// calls share one prototype identity (test
/// `Object.getPrototypeOf(s.values()) === Object.getPrototypeOf(s.values())`).
fn iterator_prototype<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::Object> {
    const CACHE_SLOT: &str = "[[asyncIter.proto]]";
    let global = scope.get_current_context().global(scope);
    let cached = slots::read_slot(scope, global, CACHE_SLOT);
    if let Ok(obj) = v8::Local::<v8::Object>::try_from(cached) {
        return obj;
    }

    let proto = v8::Object::new(scope);
    if let Some(aip) = async_iterator_prototype(scope) {
        proto.set_prototype(scope, aip);
    }

    // Install next + return as data properties (writable, configurable,
    // enumerable per spec). We use FunctionTemplate so the function has a
    // proper `.name` property; then convert to a Function and set on proto.
    install_iter_proto_method(scope, proto, "next", next_method_callback, 0);
    install_iter_proto_method(scope, proto, "return", return_method_callback, 1);

    // @@toStringTag — DONT_ENUM, READ_ONLY per spec.
    let tag_sym = v8::Symbol::get_to_string_tag(scope);
    let tag_value = v8::String::new(scope, "ReadableStream Async Iterator").unwrap();
    proto
        .define_own_property(
            scope,
            tag_sym.into(),
            tag_value.into(),
            v8::PropertyAttribute::READ_ONLY | v8::PropertyAttribute::DONT_ENUM,
        )
        .unwrap_or(false);

    // Cache.
    slots::write_slot(scope, global, CACHE_SLOT, proto.into());
    proto
}

/// Install `name` as an own data property of `proto` using a fresh
/// FunctionTemplate (which gives the function a `.name` and `.length`).
fn install_iter_proto_method(
    scope: &mut v8::PinScope,
    proto: v8::Local<v8::Object>,
    name: &str,
    cb: impl v8::MapFnTo<v8::FunctionCallback>,
    length: usize,
) {
    let name_v = v8::String::new(scope, name).unwrap();
    let tmpl = v8::FunctionTemplate::builder(cb)
        .length(length as i32)
        .build(scope);
    tmpl.set_class_name(name_v);
    let func = tmpl.get_function(scope).unwrap();
    // Set the function's `.name` to match the property key (V8 picks up
    // class_name from the FunctionTemplate, but make sure).
    let func_name_key = v8::String::new(scope, "name").unwrap();
    func.define_own_property(
        scope,
        func_name_key.into(),
        name_v.into(),
        v8::PropertyAttribute::READ_ONLY | v8::PropertyAttribute::DONT_ENUM,
    );
    proto
        .define_own_property(scope, name_v.into(), func.into(), v8::PropertyAttribute::NONE)
        .unwrap_or(false);
}

/// Build the iterator instance template — a plain ObjectTemplate with one
/// internal field (for the Box<AsyncIterState>). Cached on the realm so
/// the wrapper class identity is stable.
fn iterator_instance_template<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::ObjectTemplate> {
    // ObjectTemplates can't be cached across HandleScopes via priv-sym
    // (priv-sym holds a Value, not an ObjectTemplate). We rebuild per call;
    // the cost is one alloc plus a single set_internal_field_count call,
    // negligible compared to the rest of the iterator setup.
    let tmpl = v8::ObjectTemplate::new(scope);
    tmpl.set_internal_field_count(1);
    tmpl
}

// ---------------------------------------------------------------------------
// build — the internal "create iterator" path used by values()
// ---------------------------------------------------------------------------

/// Build a fresh async iterator wrapper bound to `reader` + `prevent_cancel`.
fn build<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    reader: v8::Local<v8::Object>,
    prevent_cancel: bool,
) -> v8::Local<'s, v8::Object> {
    let inst_tmpl = iterator_instance_template(scope);
    let iter = inst_tmpl.new_instance(scope).unwrap();

    // Set [[Prototype]] to the iterator-class prototype object (which in
    // turn inherits from %AsyncIteratorPrototype%).
    let proto = iterator_prototype(scope);
    iter.set_prototype(scope, proto.into());

    // Allocate state.
    let state = AsyncIterState::new(prevent_cancel);
    let boxed = Box::new(state);
    let raw_ptr = Box::into_raw(boxed);
    let raw_addr = raw_ptr as usize;
    let ext = v8::External::new(scope, raw_ptr as *mut std::ffi::c_void);
    iter.set_internal_field(0, ext.into());
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        iter,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut AsyncIterState));
        }),
    );
    std::mem::forget(weak);

    // Wire the slots.
    let tag = v8::Boolean::new(scope, true);
    slots::write_slot(scope, iter, ITER_TAG, tag.into());
    slots::write_slot(scope, iter, ITER_READER, reader.into());

    iter
}

// ---------------------------------------------------------------------------
// values() — public entry from ReadableStream
// ---------------------------------------------------------------------------

/// `ReadableStream.values(options)` per spec §3.2.5.9.
///
/// Steps:
///   1. Let stream be `this`. (Receiver check: caller verified.)
///   2. Let preventCancel be `options.preventCancel` (default false).
///   3. Let reader be `AcquireReadableStreamDefaultReader(stream)`.
///   4. Construct iterator bound to (reader, preventCancel).
///   5. Return iterator.
pub fn create_async_iterator<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
    prevent_cancel: bool,
) -> Result<v8::Local<'s, v8::Object>, String> {
    // Spec step 3: AcquireReadableStreamDefaultReader. Errors propagate
    // (e.g. "stream is locked").
    let reader = default_reader::acquire_readable_stream_default_reader(scope, stream)?;
    Ok(build(scope, reader, prevent_cancel))
}

// ---------------------------------------------------------------------------
// next() — the iterator's primary method
// ---------------------------------------------------------------------------

fn next_method_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'s>,
) {
    let this = args.this();
    let p = sequenced_call(scope, this, IterOp::Next, v8::undefined(scope).into());
    rv.set(p.into());
}

/// Identity-then on a Promise. Equivalent to `p.then(x => x)`. Adds one
/// microtask hop between `p`'s fulfillment and the returned promise's
/// fulfillment, matching the timing of an `async`-declared method body.
///
/// Rejections pass through unchanged (we don't supply on_rejected, so V8's
/// PerformPromiseThen forwards the rejection from `p` to the chained
/// promise without invoking any handler — that's the "no rejection
/// handler" pass-through behavior of `Promise.prototype.then(onF)`).
fn then_identity<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    p: v8::Local<'s, v8::Promise>,
) -> v8::Local<'s, v8::Promise> {
    crate::streams::promise_resolve::react_to_promise_with(
        scope,
        p,
        Some(Box::new(|scope, v| v8::Global::new(scope, v))),
        None,
    )
}

/// Discriminator for the deferred operation a sequenced call must run.
#[derive(Clone, Copy)]
enum IterOp {
    Next,
    Return,
}

/// Sequence an iterator operation through `[[ongoingPromise]]`.
///
/// Per ref impl `ReadableStreamAsyncIterator-impl.js`:
///
/// ```text
/// async next() {
///   this._ongoingPromise = this._ongoingPromise
///     ? transformPromiseWith(this._ongoingPromise, () => this._nextSteps(), () => this._nextSteps())
///     : this._nextSteps();
///   return this._ongoingPromise;
/// }
/// ```
///
/// Same shape for `return`. The chained promise becomes the new
/// `ongoingPromise` so the next call starts only after this one resolves.
///
/// Both fulfillment AND rejection of the prior promise must trigger the
/// next operation (the spec doesn't propagate prior errors into next's
/// rejection — each call is independent in outcome but ordered in time).
fn sequenced_call<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    iter: v8::Local<v8::Object>,
    op: IterOp,
    arg: v8::Local<'s, v8::Value>,
) -> v8::Local<'s, v8::Promise> {
    if !is_async_iterator(scope, iter) {
        let msg_text = match op {
            IterOp::Next => "ReadableStreamAsyncIterator.next: invalid receiver",
            IterOp::Return => "ReadableStreamAsyncIterator.return: invalid receiver",
        };
        let msg = v8::String::new(scope, msg_text).unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        return algorithms::rejected_with_promise(scope, exc.into());
    }

    let ongoing = with_state(scope, iter, |s| s.ongoing_promise.borrow().clone()).flatten();

    let new_promise = match ongoing {
        // No prior op — run inline (with one microtask hop for async-method
        // shape) and store as the new ongoing promise.
        None => {
            let inner = run_op(scope, iter, op, arg);
            then_identity(scope, inner)
        }
        // Chain on the prior — when prior settles (either way), run our
        // op. transformPromiseWith semantics: each handler returns the new
        // result, which becomes the chained promise's resolution.
        Some(prior_g) => {
            let prior = v8::Local::new(scope, &prior_g);
            let iter_g = v8::Global::new(scope, iter);
            let iter_g2 = iter_g.clone();
            let arg_g = v8::Global::new(scope, arg);
            let arg_g2 = arg_g.clone();
            crate::streams::promise_resolve::react_to_promise_with(
                scope,
                prior,
                Some(Box::new(move |scope, _v| {
                    let iter = v8::Local::new(scope, &iter_g);
                    let arg = v8::Local::new(scope, &arg_g);
                    let p = run_op(scope, iter, op, arg);
                    let p_v: v8::Local<v8::Value> = p.into();
                    v8::Global::new(scope, p_v)
                })),
                Some(Box::new(move |scope, _e| {
                    let iter = v8::Local::new(scope, &iter_g2);
                    let arg = v8::Local::new(scope, &arg_g2);
                    let p = run_op(scope, iter, op, arg);
                    let p_v: v8::Local<v8::Value> = p.into();
                    v8::Global::new(scope, p_v)
                })),
            )
        }
    };

    // Store new_promise as the new ongoing promise.
    let new_g = v8::Global::new(scope, new_promise);
    with_state(scope, iter, |s| {
        *s.ongoing_promise.borrow_mut() = Some(new_g.clone());
    });

    // When new_promise settles, clear ongoing if it still points at this
    // promise (it may have been replaced by a later call already).
    let iter_g3 = v8::Global::new(scope, iter);
    let new_g_for_clear = new_g.clone();
    crate::streams::promise_resolve::upon_promise(
        scope,
        new_promise,
        Some(Box::new(move |scope, _v| {
            let iter = v8::Local::new(scope, &iter_g3);
            with_state(scope, iter, |s| {
                let mut ongoing = s.ongoing_promise.borrow_mut();
                if let Some(cur) = ongoing.as_ref() {
                    if cur == &new_g_for_clear {
                        *ongoing = None;
                    }
                }
            });
        })),
        None,
    );

    new_promise
}

/// Run the actual implementation for `op` (no sequencing logic — just the
/// inline next-iteration / return-iteration steps).
fn run_op<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    iter: v8::Local<v8::Object>,
    op: IterOp,
    arg: v8::Local<'s, v8::Value>,
) -> v8::Local<'s, v8::Promise> {
    match op {
        IterOp::Next => next_impl(scope, iter),
        IterOp::Return => return_impl(scope, iter, arg),
    }
}

/// `next()` per spec §3.4.6 + WebIDL §3.7.10.4 "next iteration result":
///
///   1. Let promise = a new Promise.
///   2. Let reader be this.[[reader]].
///   3. If reader.[[stream]] is undefined → resolve promise with
///      `{value: undefined, done: true}`. Return promise.
///   4. Else: issue a Native ReadRequest:
///      - chunkSteps(chunk): resolve promise with `{value: chunk, done: false}`.
///      - closeSteps:        release reader; resolve promise with
///                           `{value: undefined, done: true}`.
///      - errorSteps(reason): release reader; reject promise with reason.
fn next_impl<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    this: v8::Local<v8::Object>,
) -> v8::Local<'s, v8::Promise> {
    if !is_async_iterator(scope, this) {
        let msg = v8::String::new(scope, "ReadableStreamAsyncIterator.next: invalid receiver")
            .unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        return algorithms::rejected_with_promise(scope, exc.into());
    }

    let reader_v = slots::read_slot(scope, this, ITER_READER);
    let Ok(reader) = v8::Local::<v8::Object>::try_from(reader_v) else {
        // Should not happen: ITER_READER is set at build time and never
        // cleared. Defensive: resolve with done.
        return resolved_iter_result(scope, v8::undefined(scope).into(), true);
    };

    // Per spec / ref impl: detect "finished" via reader.[[stream]] undefined.
    if slots::slot_is_empty(scope, reader, slots::STREAM) {
        return resolved_iter_result(scope, v8::undefined(scope).into(), true);
    }

    // Allocate the result Promise, then issue a native read request.
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let resolver_g = v8::Global::new(scope, resolver);
    let reader_g = v8::Global::new(scope, reader);

    let request = ReadRequest {
        kind: ReadRequestKind::Native(Box::new(NextReadRequest {
            resolver: resolver_g,
            reader: reader_g,
        })),
    };

    // Re-derive `stream` from `reader.[[stream]]` (still valid above).
    let stream_v = slots::read_slot(scope, reader, slots::STREAM);
    let Ok(stream) = v8::Local::<v8::Object>::try_from(stream_v) else {
        return resolved_iter_result(scope, v8::undefined(scope).into(), true);
    };

    default_reader::readable_stream_default_reader_read(scope, reader, stream, request);
    promise
}

/// The native ReadRequest that backs `iter.next()`.
struct NextReadRequest {
    /// Resolver of the Promise returned to JS by `next()`.
    resolver: v8::Global<v8::PromiseResolver>,
    /// Reader to release on close/error.
    reader: v8::Global<v8::Object>,
}

impl ReadRequestNative for NextReadRequest {
    fn chunk_steps<'s>(
        self: Box<Self>,
        scope: &mut v8::PinScope<'s, '_>,
        chunk: v8::Local<'s, v8::Value>,
    ) {
        // Resolve with {value: chunk, done: false}.
        let resolver = v8::Local::new(scope, &self.resolver);
        let result = make_iter_result(scope, chunk, false);
        resolver.resolve(scope, result.into());
    }

    fn close_steps(self: Box<Self>, scope: &mut v8::PinScope) {
        // 1. ReleaseReader so subsequent next() short-circuits to {done:true}.
        let reader = v8::Local::new(scope, &self.reader);
        default_reader::readable_stream_reader_generic_release(scope, reader);
        // 2. Resolve with {value: undefined, done: true}.
        let resolver = v8::Local::new(scope, &self.resolver);
        let undef: v8::Local<v8::Value> = v8::undefined(scope).into();
        let result = make_iter_result(scope, undef, true);
        resolver.resolve(scope, result.into());
    }

    fn error_steps<'s>(
        self: Box<Self>,
        scope: &mut v8::PinScope<'s, '_>,
        reason: v8::Local<'s, v8::Value>,
    ) {
        let reader = v8::Local::new(scope, &self.reader);
        default_reader::readable_stream_reader_generic_release(scope, reader);
        let resolver = v8::Local::new(scope, &self.resolver);
        resolver.reject(scope, reason);
    }
}

// ---------------------------------------------------------------------------
// return(value) — for-await-of's break/early termination path
// ---------------------------------------------------------------------------

fn return_method_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'s>,
) {
    let this = args.this();
    let value = args.get(0);
    let p = sequenced_call(scope, this, IterOp::Return, value);
    rv.set(p.into());
}

/// `return(value)` per spec §3.4.6 step 5 / ref impl
/// `ReadableStreamAsyncIterator-impl.js`:
///
///   1. Let reader be this.[[reader]].
///   2. If reader.[[stream]] is undefined → return promiseResolvedWith
///      `{value, done: true}`.
///   3. Let preventCancel be this.[[preventCancel]].
///   4. If preventCancel is true:
///        a. ReadableStreamReaderGenericRelease(reader).
///        b. Return promiseResolvedWith `{value, done: true}`.
///   5. Else:
///        a. cancelPromise = ReadableStreamReaderGenericCancel(reader, value).
///        b. ReadableStreamReaderGenericRelease(reader).
///        c. Return cancelPromise.then(_ => {value, done: true}).
fn return_impl<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    this: v8::Local<v8::Object>,
    value: v8::Local<'s, v8::Value>,
) -> v8::Local<'s, v8::Promise> {
    if !is_async_iterator(scope, this) {
        let msg = v8::String::new(scope, "ReadableStreamAsyncIterator.return: invalid receiver")
            .unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        return algorithms::rejected_with_promise(scope, exc.into());
    }

    let reader_v = slots::read_slot(scope, this, ITER_READER);
    let Ok(reader) = v8::Local::<v8::Object>::try_from(reader_v) else {
        return resolved_iter_result(scope, value, true);
    };

    if slots::slot_is_empty(scope, reader, slots::STREAM) {
        // Already finished — mirror spec: resolve {value, done: true}.
        return resolved_iter_result(scope, value, true);
    }

    let prevent_cancel = with_state(scope, this, |s| s.prevent_cancel.get()).unwrap_or(false);

    if prevent_cancel {
        // Release reader, no cancel. Resolve {value, done: true}.
        default_reader::readable_stream_reader_generic_release(scope, reader);
        return resolved_iter_result(scope, value, true);
    }

    // Cancel stream via reader; release; map fulfillment to {value, done: true}.
    let stream_v = slots::read_slot(scope, reader, slots::STREAM);
    let Ok(stream) = v8::Local::<v8::Object>::try_from(stream_v) else {
        // Reader was somehow detached racily — short-circuit.
        return resolved_iter_result(scope, value, true);
    };
    let cancel_promise = default_reader::readable_stream_reader_generic_cancel(
        scope, reader, stream, value,
    );
    default_reader::readable_stream_reader_generic_release(scope, reader);

    // Map cancelPromise.then(_ => {value, done: true}). Rejections pass through.
    let value_g = v8::Global::new(scope, value);
    crate::streams::promise_resolve::react_to_promise_with(
        scope,
        cancel_promise,
        Some(Box::new(move |scope, _v| {
            let v = v8::Local::new(scope, &value_g);
            let result = make_iter_result(scope, v, true);
            let result_v: v8::Local<v8::Value> = result.into();
            v8::Global::new(scope, result_v)
        })),
        None,
    )
}

// ---------------------------------------------------------------------------
// Iterator-result helpers
// ---------------------------------------------------------------------------

/// Allocate a fresh iterator-result object `{value, done}` per ECMA-262
/// CreateIterResultObject. Plain object with `Object.prototype` as
/// `[[Prototype]]`, two own data properties.
fn make_iter_result<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<v8::Value>,
    done: bool,
) -> v8::Local<'s, v8::Object> {
    let result = v8::Object::new(scope);
    let value_key = v8::String::new(scope, "value").unwrap();
    let done_key = v8::String::new(scope, "done").unwrap();
    result.set(scope, value_key.into(), value);
    result.set(scope, done_key.into(), v8::Boolean::new(scope, done).into());
    result
}

fn resolved_iter_result<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<v8::Value>,
    done: bool,
) -> v8::Local<'s, v8::Promise> {
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let result = make_iter_result(scope, value, done);
    resolver.resolve(scope, result.into());
    promise
}

// ---------------------------------------------------------------------------
// Public install — pre-build the iterator prototype on the realm
// ---------------------------------------------------------------------------
//
// Per spec: there is no `ReadableStreamAsyncIterator` constructor on
// globalThis (it's an "anonymous" interface created by the WebIDL
// `async iterable<T>` declaration). We just pre-populate the cached
// prototype object so the first `values()` call doesn't pay for setup.

/// No-op-equivalent install: pre-warms the iterator prototype cache so
/// `values()` is allocation-light. Idempotent.
pub fn install(scope: &mut v8::PinScope, _global: v8::Local<v8::Object>) {
    // Pre-build the iterator prototype object (cached on global).
    let _ = iterator_prototype(scope);
}
