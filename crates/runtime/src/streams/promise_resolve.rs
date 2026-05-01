//! Promise plumbing for stream algorithms (D-3, §VII.3, §VII.4).
//!
//! Two responsibilities:
//!
//! 1. **`enqueue_microtask`** — schedule a Rust closure on the V8
//!    microtask queue. Per design D-12, this uses
//!    `Isolate::enqueue_microtask(Local<Function>)` directly. NOT
//!    `Promise.resolve().then(…)` — that path is observable from
//!    userland Promise-prototype tampering and adds an extra microtask
//!    hop, which WPT `readable-streams/tee.any.js` notices.
//!
//! 2. **`upon_promise`** / **`react_to_promise_with`** /
//!    **`set_promise_is_handled_to_true`** — spec-faithful
//!    PerformPromiseThen helpers. Per critic #17, the spec's
//!    `uponPromise(p, onF, onR)` is the DOUBLE-then pattern that forwards
//!    rejections from `onF`/`onR` to a default rethrow handler so genuine
//!    bugs in our Rust callback layer surface as unhandled-promise events.

use std::cell::RefCell;
use std::rc::Rc;

// ---------------------------------------------------------------------------
// enqueue_microtask — closure → V8 microtask
// ---------------------------------------------------------------------------

/// Type alias for the closure we schedule. `FnOnce` because the closure
/// fires exactly once per microtask. `'static` because V8 owns the
/// FunctionTemplate's lifetime once the microtask is queued.
type MicrotaskClosure = Box<dyn FnOnce(&mut v8::PinScope) + 'static>;

/// Holder shape stashed in an `External`. The `Option` lets the V8 callback
/// `take()` the closure exactly once, even though the holder might be
/// reachable from a re-entrant V8 path.
type MicrotaskHolder = RefCell<Option<MicrotaskClosure>>;

/// Schedule a Rust closure on V8's default microtask queue. Per D-12.
///
/// Rationale: stream spec algorithms say "queue a microtask to run X".
/// Implementing this as `Promise.resolve().then(X)` is observable to
/// userland (a tampered `Promise.prototype.then` could intercept the
/// reaction) and adds two microtask hops (resolve + then), whereas
/// `queueMicrotask` is one hop. WPT `readable-streams/tee.any.js`
/// "should not pull more chunks than were specified" counts hops; the
/// difference shows up as a test failure.
pub fn enqueue_microtask<F: FnOnce(&mut v8::PinScope) + 'static>(
    scope: &mut v8::PinScope,
    cb: F,
) {
    let holder: Rc<MicrotaskHolder> = Rc::new(RefCell::new(Some(Box::new(cb))));
    let raw = Rc::into_raw(holder) as *mut std::ffi::c_void;
    let ext = v8::External::new(scope, raw);

    // FunctionTemplate with the holder pinned in External::data. The
    // template is one-shot — we only call get_function once, attach via
    // enqueue_microtask, and never re-use it.
    let tmpl = v8::FunctionTemplate::builder(microtask_callback)
        .data(ext.into())
        .build(scope);
    let func = tmpl.get_function(scope).unwrap();

    // The real V8 API: Isolate::enqueue_microtask(Local<Function>).
    // PinScope deref's to Isolate's mutable handle.
    scope.enqueue_microtask(func);
}

fn microtask_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let data = args.data();
    let Ok(ext) = v8::Local::<v8::External>::try_from(data) else {
        return;
    };
    let raw = ext.value() as *const MicrotaskHolder;
    // SAFETY: `raw` was produced by `Rc::into_raw` in `enqueue_microtask`.
    // The microtask fires exactly once (V8 dequeues it from the
    // MicrotaskQueue and discards), so reconstructing the Rc here and
    // dropping it at the end of the callback is the matching `from_raw`.
    let rc: Rc<MicrotaskHolder> = unsafe { Rc::from_raw(raw) };
    let cb_opt = rc.borrow_mut().take();
    if let Some(cb) = cb_opt {
        cb(scope);
    }
    // rc dropped here → External's pointer is dangling, but V8 does NOT
    // re-fire the microtask; the Function/External pair is GC-reaped.
}

// ---------------------------------------------------------------------------
// upon_promise — spec uponPromise(p, onF, onR)
// ---------------------------------------------------------------------------

type PromiseCallback = Box<dyn FnOnce(&mut v8::PinScope, v8::Local<v8::Value>) + 'static>;

type PromiseCallbackHolder = RefCell<Option<PromiseCallback>>;

/// Spec `uponPromise(p, onF, onR)`:
///
/// ```text
/// PerformPromiseThen(
///   PerformPromiseThen(p, onF, onR),
///   undefined,
///   rethrowAssertionErrorRejection)
/// ```
///
/// The double-then pattern forwards any rejection from `onF`/`onR` to
/// the V8 default unhandled-rejection path, which is exactly what we
/// want for stream-internal bookkeeping bugs. Per critic #17.
///
/// Either callback may be `None` (the spec equivalent of passing
/// `undefined` for that handler — V8 propagates the value/rejection
/// through unchanged).
pub fn upon_promise<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    promise: v8::Local<'s, v8::Promise>,
    on_fulfilled: Option<PromiseCallback>,
    on_rejected: Option<PromiseCallback>,
) {
    let mid = chain_then(scope, promise, on_fulfilled, on_rejected);
    // Outer then: forward unhandled rejections to V8's default.
    chain_then(
        scope,
        mid,
        None,
        Some(Box::new(rethrow_assertion_error_rejection)),
    );
}

/// Spec `setPromiseIsHandledToTrue(p)` — used by pipeTo to swallow the
/// pipeLoop's rejection (shutdown handlers handle errors via the
/// installed forward/backward paths).
pub fn set_promise_is_handled_to_true<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    promise: v8::Local<'s, v8::Promise>,
) {
    // Implementation: PerformPromiseThen(p, undefined, rethrow…). Same
    // shape as upon_promise's outer then, but with no fulfillment handler.
    chain_then(
        scope,
        promise,
        None,
        Some(Box::new(rethrow_assertion_error_rejection)),
    );
}

/// Spec `reactToPromiseWith(p, onF, onR)` — single-then variant where the
/// callbacks return a new value (as a `v8::Global<v8::Value>`) that
/// becomes the chained promise's resolution. (UponPromise discards the
/// callbacks' return values; this helper preserves them.)
///
/// Used by tee/pipeTo for fulfillment-mapped chains where the next stage
/// depends on the prior callback's return value (e.g. cancelPromise.then(
/// onF)).
///
/// Closures return `v8::Global<v8::Value>` (not Local) because the trait
/// object can't carry a scope-tied lifetime. The callback materializes
/// the Global to a Local at the V8 reaction site.
pub fn react_to_promise_with<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    promise: v8::Local<'s, v8::Promise>,
    on_fulfilled: Option<PromiseValueCallback>,
    on_rejected: Option<PromiseValueCallback>,
) -> v8::Local<'s, v8::Promise> {
    chain_then_with_value(scope, promise, on_fulfilled, on_rejected)
}

/// Default rejection handler — re-throws so V8's unhandled-rejection
/// machinery surfaces the error. Stream algorithms install this as the
/// outer then's rejection handler so internal bugs aren't silently
/// swallowed.
fn rethrow_assertion_error_rejection(scope: &mut v8::PinScope, reason: v8::Local<v8::Value>) {
    // The simplest "rethrow" is to throw the value as a V8 exception;
    // the surrounding promise reaction job will mark this promise as
    // rejected with that value, which then propagates to V8's
    // unhandled-rejection callback registered at isolate setup. (See
    // dispatch.rs / runtime.rs for the existing handler.)
    scope.throw_exception(reason);
}

// ---------------------------------------------------------------------------
// chain_then — internal PerformPromiseThen wrapper
// ---------------------------------------------------------------------------

/// PerformPromiseThen(p, onF, onR) — fulfillment/rejection that DISCARDS
/// the callback's return value (uponPromise semantics).
fn chain_then<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    promise: v8::Local<'s, v8::Promise>,
    on_fulfilled: Option<PromiseCallback>,
    on_rejected: Option<PromiseCallback>,
) -> v8::Local<'s, v8::Promise> {
    let on_f = on_fulfilled.map(|cb| build_oneshot_callback_void(scope, cb));
    let on_r = on_rejected.map(|cb| build_oneshot_callback_void(scope, cb));

    match (on_f, on_r) {
        (Some(f), Some(r)) => promise.then2(scope, f, r).unwrap(),
        (Some(f), None) => promise.then(scope, f).unwrap(),
        (None, Some(r)) => {
            // V8's `Promise::then2` requires both; for the
            // rejection-only case build a no-op fulfillment that
            // forwards the value.
            let f_template = v8::FunctionTemplate::new(scope, identity_callback);
            let f = f_template.get_function(scope).unwrap();
            promise.then2(scope, f, r).unwrap()
        }
        (None, None) => promise,
    }
}

/// PerformPromiseThen(p, onF, onR) — fulfillment/rejection that USES
/// the callback's return value (reactToPromiseWith semantics). The
/// returned Promise resolves with whatever the callback returned.
fn chain_then_with_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    promise: v8::Local<'s, v8::Promise>,
    on_fulfilled: Option<PromiseValueCallback>,
    on_rejected: Option<PromiseValueCallback>,
) -> v8::Local<'s, v8::Promise> {
    let on_f = on_fulfilled.map(|cb| build_oneshot_callback_value(scope, cb));
    let on_r = on_rejected.map(|cb| build_oneshot_callback_value(scope, cb));

    match (on_f, on_r) {
        (Some(f), Some(r)) => promise.then2(scope, f, r).unwrap(),
        (Some(f), None) => promise.then(scope, f).unwrap(),
        (None, Some(r)) => {
            let f_template = v8::FunctionTemplate::new(scope, identity_callback);
            let f = f_template.get_function(scope).unwrap();
            promise.then2(scope, f, r).unwrap()
        }
        (None, None) => promise,
    }
}

/// Identity callback — returns its input unchanged. Used as a no-op
/// fulfillment handler when `chain_then` was called with only an
/// `on_rejected`. Spec: PerformPromiseThen with `undefined` for one
/// handler is equivalent to "pass through the value".
fn identity_callback(
    _scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    rv.set(args.get(0));
}

// ---------------------------------------------------------------------------
// One-shot callback construction (closure → v8::Local<Function>)
// ---------------------------------------------------------------------------

/// Build a one-shot V8 Function from a Rust closure that returns nothing
/// (uponPromise semantics — return value discarded).
fn build_oneshot_callback_void<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    cb: PromiseCallback,
) -> v8::Local<'s, v8::Function> {
    let holder: Rc<PromiseCallbackHolder> = Rc::new(RefCell::new(Some(cb)));
    let raw = Rc::into_raw(holder) as *mut std::ffi::c_void;
    let ext = v8::External::new(scope, raw);
    let tmpl = v8::FunctionTemplate::builder(promise_callback_void)
        .data(ext.into())
        .build(scope);
    tmpl.get_function(scope).unwrap()
}

fn promise_callback_void(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let data = args.data();
    let Ok(ext) = v8::Local::<v8::External>::try_from(data) else {
        return;
    };
    let raw = ext.value() as *const PromiseCallbackHolder;
    let rc: Rc<PromiseCallbackHolder> = unsafe { Rc::from_raw(raw) };
    let cb_opt = rc.borrow_mut().take();
    if let Some(cb) = cb_opt {
        cb(scope, args.get(0));
    }
    // rc dropped → holder dropped. The Function is one-shot.
}

/// Build a one-shot V8 Function from a Rust closure that returns a value
/// (reactToPromiseWith semantics — chained promise resolves with the
/// returned value).
///
/// The closure can't return `v8::Local` because the trait object would
/// have to carry the scope's lifetime, which is incompatible with
/// `'static`. We require the closure to return a `v8::Global<v8::Value>`
/// and re-localize it at the V8 reaction callsite.
pub type PromiseValueCallback = Box<
    dyn FnOnce(&mut v8::PinScope, v8::Local<v8::Value>) -> v8::Global<v8::Value> + 'static,
>;
type PromiseValueCallbackHolder = RefCell<Option<PromiseValueCallback>>;

fn build_oneshot_callback_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    cb: PromiseValueCallback,
) -> v8::Local<'s, v8::Function> {
    let holder: Rc<PromiseValueCallbackHolder> = Rc::new(RefCell::new(Some(cb)));
    let raw = Rc::into_raw(holder) as *mut std::ffi::c_void;
    let ext = v8::External::new(scope, raw);
    let tmpl = v8::FunctionTemplate::builder(promise_callback_value)
        .data(ext.into())
        .build(scope);
    tmpl.get_function(scope).unwrap()
}

fn promise_callback_value(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let data = args.data();
    let Ok(ext) = v8::Local::<v8::External>::try_from(data) else {
        return;
    };
    let raw = ext.value() as *const PromiseValueCallbackHolder;
    let rc: Rc<PromiseValueCallbackHolder> = unsafe { Rc::from_raw(raw) };
    let cb_opt = rc.borrow_mut().take();
    if let Some(cb) = cb_opt {
        let result_g = cb(scope, args.get(0));
        let result_l = v8::Local::new(scope, &result_g);
        rv.set(result_l);
    }
}
