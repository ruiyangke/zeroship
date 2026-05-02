//! Native `AbortSignal` per DOM §3.3
//! (https://dom.spec.whatwg.org/#interface-AbortSignal).
//!
//! Replaces the JS polyfill that lived in `embed/fetch.js:438-538`.
//! The polyfill had the right shape for fetch cancellation but
//! cut corners on every spec corner that matters elsewhere:
//!
//!   - `signal instanceof EventTarget === false` (no native base
//!     class) — breaks duck-typing in libraries like langgraph.
//!   - `addEventListener` ignored `once: true`, `passive: true`,
//!     and the `signal: AbortSignal` removal pattern.
//!   - `AbortSignal.timeout(ms)` had no GC-retention strategy — a
//!     pending timeout's signal could be reclaimed before the
//!     timer fired (CRITICAL-9).
//!   - `AbortSignal.any([s1, s2])` registered abort listeners on
//!     each input but didn't flatten through transitive
//!     `AbortSignal.any` returns per DOM §3.3.4 (CRITICAL-8).
//!   - The "signal abort" algorithm fired listeners BEFORE running
//!     abort algorithms, which the spec specifically calls out as
//!     wrong (MAJOR-40).
//!
//! This implementation honours all of the above. See design fetch-
//! native §IX.4 (signal abort algorithm), §IX.5 (timeout GC),
//! §IX.6 (any flattening).
//!
//! ## Storage layout (§XIII.3)
//!
//! Per the design's single-source slot rule:
//!   - [[aborted]]: Rust `Cell<bool>` on the boxed state.
//!   - [[reason]]: Rust `RefCell<Option<v8::Global<v8::Value>>>`.
//!   - [[abort algorithms]]: Rust `RefCell<Vec<Box<dyn FnOnce()>>>`.
//!   - [[dependent signals]] / [[source signals]]: Rust
//!     `RefCell<Vec<v8::Global<v8::Object>>>` — strong refs on both
//!     sides because `Weak` to a JS wrapper retains nothing past GC,
//!     and the spec's flattening (§3.3.4 step 4) requires the
//!     source-signals walk to find every transitive ancestor. The
//!     bidirectional pair is freed when the head signal is GC'd
//!     (the boxed state's drop releases all the Globals).
//!   - [[timer id]]: Rust `Cell<Option<u32>>` — for AbortSignal.timeout.
//!
//! The listener Rc lives on the JS wrapper as a private symbol
//! (see `super::event_target::attach_listeners`); AbortSignal's
//! mint helper attaches it at construction.

use std::cell::{Cell, RefCell};

use zeroship_runtime_macros::v8_class;
#[allow(unused_imports)]
use zeroship_runtime_macros::{
    v8_constructor, v8_getter, v8_inherit, v8_method, v8_name,
};

use crate::state::{OpError, SharedState, TimerCallback};

use super::event::build_abort_event;
use super::event_target::{attach_listeners, dispatch_event};

// ---------------------------------------------------------------------------
// AbortSignal state
// ---------------------------------------------------------------------------

/// Backing state for an AbortSignal JS wrapper. Stored in internal
/// field 0 as `Box<AbortSignal>`.
pub struct AbortSignal {
    /// `aborted` flag — DOM §3.3.
    pub aborted: Cell<bool>,
    /// `reason` — the value passed to `controller.abort(reason)` or
    /// constructed by `AbortSignal.timeout` ("TimeoutError" DOMException-
    /// shaped) / the default "AbortError". Stored as a JS Global so
    /// the SAME object is returned on every `signal.reason` access
    /// (per spec, `[[abortReason]]` is JS-identity-preserving).
    pub reason: RefCell<Option<v8::Global<v8::Value>>>,
    /// "Abort algorithms" list per DOM §3.3.1 — Rust-side callbacks
    /// the implementation registers internally (e.g. fetch's
    /// controller-terminate, EventTarget's signal-listener-removal).
    /// Distinct from event listeners. Drained when the signal aborts.
    pub abort_algorithms: RefCell<Vec<Box<dyn FnOnce()>>>,
    /// "Source signals" set (DOM §3.3.4 step 4) — the signals that,
    /// when aborted, abort this signal. Bidirectional with
    /// `dependent_signals`. Used by AbortSignal.any to flatten
    /// transitive dependents (CRITICAL-8).
    pub source_signals: RefCell<Vec<v8::Global<v8::Object>>>,
    /// "Dependent signals" set — the signals that this signal aborts
    /// when it itself aborts. Bidirectional with `source_signals`.
    pub dependent_signals: RefCell<Vec<v8::Global<v8::Object>>>,
    /// "Dependent" boolean flag (DOM §3.3.4) — true iff this signal
    /// was returned by AbortSignal.any().
    pub is_dependent: Cell<bool>,
    /// `onabort` event-handler IDL attribute. Per WHATWG HTML §3.2.7,
    /// EventHandler attributes are "event handler IDL attributes":
    /// setting one (a) replaces any prior handler-via-this-attribute,
    /// (b) installs an addEventListener-equivalent invocation. We
    /// implement the simpler model: `onabort = fn` registers a
    /// single internal listener that calls `fn` when "abort" fires;
    /// reading `onabort` returns the stored function (or null).
    pub onabort: RefCell<Option<v8::Global<v8::Function>>>,
    /// AbortSignal.timeout: the timer ID we can cancel on GC, plus
    /// the strong-self-ref for GC retention (CRITICAL-9). The Rc is
    /// held by SharedState's `timeout_pinned` map; the ID lets us
    /// remove ourselves on abort.
    pub timer_id: Cell<Option<u32>>,
}

impl Default for AbortSignal {
    fn default() -> Self {
        AbortSignal {
            aborted: Cell::new(false),
            reason: RefCell::new(None),
            abort_algorithms: RefCell::new(Vec::new()),
            source_signals: RefCell::new(Vec::new()),
            dependent_signals: RefCell::new(Vec::new()),
            is_dependent: Cell::new(false),
            onabort: RefCell::new(None),
            timer_id: Cell::new(None),
        }
    }
}

// ---------------------------------------------------------------------------
// AbortSignal IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
#[v8_inherit(super::event_target::EventTarget)]
impl AbortSignal {
    /// AbortSignal has no public constructor per DOM §3.3 — instances
    /// are minted via `AbortController#signal`, `AbortSignal.abort()`,
    /// `AbortSignal.timeout()`, or `AbortSignal.any()`. We still
    /// emit a default constructor so the macro's install codegen
    /// works; calling `new AbortSignal()` from JS yields a usable-
    /// but-never-aborted instance, which the spec also tolerates
    /// (other implementations behave the same way for observability).
    #[v8_constructor]
    fn new() -> AbortSignal {
        AbortSignal::default()
    }

    /// `signal.aborted` getter — DOM §3.3.
    #[v8_getter]
    fn aborted(&self) -> bool {
        self.aborted.get()
    }

    /// `signal.reason` getter — DOM §3.3. Returns `undefined` if not
    /// aborted, the stored reason otherwise.
    #[v8_getter]
    fn reason<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        match self.reason.borrow().as_ref() {
            Some(g) => v8::Local::new(scope, g.clone()),
            None => v8::undefined(scope).into(),
        }
    }

    /// `signal.throwIfAborted()` — DOM §3.3. Throws `signal.reason`
    /// (the actual stored value) if aborted; no-op otherwise.
    #[v8_method]
    #[v8_name = "throwIfAborted"]
    fn throw_if_aborted(&self, scope: &mut v8::PinScope) {
        if !self.aborted.get() {
            return;
        }
        let exc = match self.reason.borrow().as_ref() {
            Some(g) => v8::Local::new(scope, g.clone()),
            None => build_abort_error(scope).into(),
        };
        scope.throw_exception(exc);
    }
}

// ---------------------------------------------------------------------------
// Mint helpers
// ---------------------------------------------------------------------------

/// Build a fresh AbortSignal JS wrapper. Used by AbortController's
/// constructor and the static factory methods. Returns the wrapper
/// AND the Rust state pointer (so the static factory can fill in
/// state before the wrapper escapes to user code).
pub(crate) fn mint_abort_signal<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> (v8::Local<'s, v8::Object>, *mut AbortSignal) {
    let tmpl = AbortSignal::install(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let obj = inst_tmpl
        .new_instance(scope)
        .expect("AbortSignal instance allocation failed");

    let state = AbortSignal::default();
    let boxed: Box<AbortSignal> = Box::new(state);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());

    // Wire prototype to AbortSignal.prototype so methods/getters and
    // the inherited EventTarget chain resolve.
    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    obj.set_prototype(scope, proto_v);

    // Attach the listener Rc — see event_target.rs for why this lives
    // on the wrapper rather than nested inside the Rust state.
    attach_listeners(scope, obj);

    // Finalizer reclaims the Box on GC / isolate teardown.
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut AbortSignal));
        }),
    );
    std::mem::forget(weak);

    (obj, raw)
}

/// Get the boxed `AbortSignal` from a V8 wrapper. Returns `None` if
/// the object isn't an AbortSignal (no internal field, or the field
/// isn't an External).
pub(crate) fn signal_from_obj<'a>(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
) -> Option<&'a AbortSignal> {
    let ext = obj
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())?;
    let ptr = ext.value() as *mut AbortSignal;
    if ptr.is_null() {
        return None;
    }
    // SAFETY: The boxed state's lifetime is tied to the wrapper via
    // a guaranteed finalizer (mint_abort_signal). The single-threaded
    // isolate invariant means concurrent access is impossible.
    Some(unsafe { &*ptr })
}

/// Public view: is `obj` an aborted AbortSignal? Returns false for
/// non-AbortSignal objects. Used by EventTarget.addEventListener's
/// `signal` short-circuit (DOM §2.7 step 3).
pub fn is_aborted(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>) -> bool {
    signal_from_obj(scope, obj).map(|s| s.aborted.get()).unwrap_or(false)
}

/// Public view: register a Rust callback to fire on abort. Used by
/// EventTarget's `signal`-removal hook. The callback runs SYNCHRONOUSLY
/// during the `signal abort` algorithm (DOM §3.3.1 step 5.1) BEFORE
/// the "abort" event is fired — this is the spec-mandated ordering
/// (MAJOR-40).
///
/// If `obj` isn't an AbortSignal, this is a silent no-op.
pub fn add_abort_algorithm(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    cb: Box<dyn FnOnce()>,
) {
    let Some(signal) = signal_from_obj(scope, obj) else {
        return;
    };
    if signal.aborted.get() {
        // Per DOM §3.3.1 step 1, signal_abort returns immediately
        // when already aborted; algorithms registered AFTER an
        // abort never fire. Silently drop.
        return;
    }
    signal.abort_algorithms.borrow_mut().push(cb);
}

// ---------------------------------------------------------------------------
// Build helpers — DOMException-shaped error reasons
// ---------------------------------------------------------------------------

/// Build the default abort reason: a DOMException-shaped Error with
/// `name = "AbortError"`. The JS polyfill's DOMException shim uses
/// the same shape; we mirror it here.
///
/// Real DOMException is a `#[v8_class]` we'll add in the next chunk
/// (Body / Request / Response). For now, build a vanilla Error and
/// patch on the spec-correct properties.
pub(crate) fn build_abort_error<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::Object> {
    build_dom_exception(scope, "The operation was aborted.", "AbortError", 20)
}

/// Build a "TimeoutError" DOMException-shaped error for
/// `AbortSignal.timeout` (DOM §3.3 step 5).
pub(crate) fn build_timeout_error<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::Object> {
    build_dom_exception(scope, "The operation timed out.", "TimeoutError", 23)
}

fn build_dom_exception<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    message: &str,
    name: &str,
    code: u32,
) -> v8::Local<'s, v8::Object> {
    let msg = v8::String::new(scope, message).unwrap();
    let err = v8::Exception::error(scope, msg);
    let err_obj: v8::Local<v8::Object> = err.try_into().unwrap();
    let name_key = v8::String::new(scope, "name").unwrap();
    let name_val = v8::String::new(scope, name).unwrap();
    err_obj.set(scope, name_key.into(), name_val.into());
    let code_key = v8::String::new(scope, "code").unwrap();
    let code_val = v8::Integer::new_from_unsigned(scope, code);
    err_obj.set(scope, code_key.into(), code_val.into());
    err_obj
}

// ---------------------------------------------------------------------------
// signal_abort — DOM §3.3.1 "to signal abort"
// ---------------------------------------------------------------------------

/// "To signal abort an AbortSignal signal with reason" — DOM §3.3.1.
///
/// Per spec ordering (CRITICAL-7):
///   1. If signal is aborted, return.
///   2. Set signal's reason.
///   3. Collect non-aborted dependent signals AND set their reason.
///   4. Run abort steps for `signal` (algorithms first, then "abort"
///      event via dispatchEvent).
///   5. For each collected dependent, run abort steps in collection
///      order.
///
/// The `dependentSignalsToAbort` list is collected BEFORE step 4
/// (the signal's algorithms / event) so that an abort algorithm
/// running on the head signal can't reach into dependent_signals
/// during step 5. We also iterate dependents inline rather than
/// recursing into signal_abort — per spec, dependents have their
/// abort steps run, NOT the full signal_abort algorithm.
pub fn signal_abort(
    scope: &mut v8::PinScope,
    signal_obj: v8::Local<v8::Object>,
    reason: v8::Local<v8::Value>,
) {
    let Some(signal) = signal_from_obj(scope, signal_obj) else {
        return;
    };
    if signal.aborted.get() {
        return;
    }
    let reason_global = v8::Global::new(scope, reason);

    // Step 2: set reason BEFORE collecting dependents (so the next
    // loop can clone it for cascading).
    signal.aborted.set(true);
    *signal.reason.borrow_mut() = Some(reason_global.clone());

    // Step 3: collect non-aborted dependents AND set their reason
    // in the same pass. We do NOT recurse into signal_abort — that's
    // the second pass (step 5).
    let mut deps_to_abort: Vec<v8::Local<v8::Object>> = Vec::new();
    let dep_globals = signal.dependent_signals.borrow().clone();
    for dep_global in dep_globals {
        let dep_obj = v8::Local::new(scope, dep_global);
        let Some(dep) = signal_from_obj(scope, dep_obj) else {
            continue;
        };
        if !dep.aborted.get() {
            dep.aborted.set(true);
            *dep.reason.borrow_mut() = Some(reason_global.clone());
            deps_to_abort.push(dep_obj);
        }
    }

    // Step 4: run abort steps for `signal`.
    run_abort_steps(scope, signal_obj);

    // Step 5: run abort steps for each collected dependent (in
    // collection order).
    for dep_obj in deps_to_abort {
        run_abort_steps(scope, dep_obj);
    }
}

/// "Abort steps" sub-algorithm: run abort algorithms, clear them,
/// then fire the "abort" event via the EventTarget surface (MAJOR-40
/// — algorithms FIRST, event SECOND).
fn run_abort_steps(scope: &mut v8::PinScope, signal_obj: v8::Local<v8::Object>) {
    // Take and run abort algorithms (FnOnce — consumed).
    let algorithms = {
        let Some(signal) = signal_from_obj(scope, signal_obj) else {
            return;
        };
        std::mem::take(&mut *signal.abort_algorithms.borrow_mut())
    };
    for algorithm in algorithms {
        algorithm();
    }

    // Clear timer-pinning if this signal had a pending timeout.
    {
        let Some(signal) = signal_from_obj(scope, signal_obj) else {
            return;
        };
        if let Some(timer_id) = signal.timer_id.take() {
            // Drop the strong ref held by SharedState. Safe to
            // ignore the result if state is unset (test paths
            // run without the runtime pump).
            if let Some(st) = scope.get_slot::<SharedState>().cloned() {
                st.borrow_mut().timeout_pinned_signals.remove(&timer_id);
            }
        }
    }

    // Fire "abort" event AFTER algorithms (MAJOR-40).
    let event = build_abort_event(scope);
    dispatch_event(scope, signal_obj, event);
}

// ---------------------------------------------------------------------------
// Static factories — AbortSignal.abort / .timeout / .any
// ---------------------------------------------------------------------------

/// `AbortSignal.abort(reason?)` — DOM §3.3. Returns an already-aborted
/// signal with the given reason (or a fresh AbortError if reason is
/// undefined).
pub fn abort_static<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    reason: v8::Local<v8::Value>,
) -> v8::Local<'s, v8::Object> {
    let (signal_obj, raw) = mint_abort_signal(scope);
    // SAFETY: raw was just allocated by mint_abort_signal; the
    // wrapper holds a strong ref via the External + finalizer.
    let signal: &AbortSignal = unsafe { &*raw };

    // Per spec, the signal returned by AbortSignal.abort is
    // ALREADY aborted — set state directly without running the
    // signal_abort algorithm (we don't need to fire event / run
    // dependents because there are none yet — step 1 of any
    // listener registered later is "signal is aborted, return").
    let resolved_reason: v8::Local<v8::Value> = if reason.is_undefined() {
        build_abort_error(scope).into()
    } else {
        reason
    };
    signal.aborted.set(true);
    *signal.reason.borrow_mut() = Some(v8::Global::new(scope, resolved_reason));

    signal_obj
}

/// `AbortSignal.timeout(ms)` — DOM §3.3. Returns a fresh signal that
/// aborts after `ms` milliseconds with a "TimeoutError" DOMException
/// reason.
///
/// GC retention (CRITICAL-9): per DOM step 3, "for the duration of
/// this timeout, if signal has any event listeners registered for
/// its abort event, there must be a strong reference from global to
/// signal." We pin the wrapper in `SharedState::timeout_pinned_signals`
/// keyed by timer ID for the entire duration of the timeout —
/// regardless of listener-registered state. The pin is dropped when
/// the timer fires (in `run_abort_steps`).
///
/// The simpler "always-pin" strategy is a slight over-approximation
/// of the spec ("only pin while listeners are registered"), but the
/// extra retention is bounded by the timeout duration and the spec's
/// intent — keep the timer effective.
pub fn timeout_static<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    ms: u64,
) -> v8::Local<'s, v8::Object> {
    let (signal_obj, raw) = mint_abort_signal(scope);
    let signal: &AbortSignal = unsafe { &*raw };

    // Schedule via the existing timer infrastructure: register a
    // dummy callback that calls signal_abort when fired. The native
    // timer pump (dispatch::fire_timer_callback) calls the callback
    // with no args; we hijack that to run our abort algorithm.
    //
    // The callback closure can't safely capture `&AbortSignal` (it
    // dies between turns of the event loop) or even the `*mut`
    // (the External's pointer is stable but we'd be reaching past
    // the v8 wrapper). Instead, we capture the signal's V8 Global
    // and re-resolve the boxed state by reading internal field 0
    // when the timer fires.

    let Some(state) = scope.get_slot::<SharedState>().cloned() else {
        // No runtime pump → no timer. Tests that don't drive the
        // event loop won't see the abort fire, but this is safe.
        return signal_obj;
    };

    let signal_global = v8::Global::new(scope, signal_obj);
    let timer_id = {
        let mut s = state.borrow_mut();
        let id = s.next_timer_id;
        s.next_timer_id += 1;
        // Pin the signal wrapper so it survives GC while the timer
        // is pending.
        s.timeout_pinned_signals.insert(id, signal_global.clone());
        // We need a callback. We can't synthesize a v8::Function in
        // this scope and store as a Global without entering V8;
        // instead, we use the signal_global as a marker and arm a
        // separate side-channel hook.
        //
        // The cleanest plumbing is: store the signal Global in a
        // dedicated map (timeout_pinned_signals) and have the timer
        // pump check this map by ID. We'd need to extend the timer
        // dispatch path — see runtime.rs. For v1 we take the
        // simpler approach of installing a synthesized JS function
        // that, when called, resolves the signal and runs
        // signal_abort.
        let abort_fn_global = build_timeout_callback_function(scope, &signal_global);
        s.timer_callbacks.insert(
            id,
            TimerCallback {
                callback: abort_fn_global,
                interval: None,
            },
        );
        if let Some(req_id) = s.executing_request_id {
            s.timer_owner.insert(id, req_id);
        }
        s.spawned_timers.push(crate::state::SpawnedTimer {
            id,
            delay: std::time::Duration::from_millis(ms),
            interval: None,
        });
        id
    };
    signal.timer_id.set(Some(timer_id));

    signal_obj
}

/// Build the V8 Function that fires when the timeout elapses. The
/// function captures the signal's Global as External data and, when
/// called, runs `signal_abort` on it with a TimeoutError reason.
fn build_timeout_callback_function<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    signal_global: &v8::Global<v8::Object>,
) -> v8::Global<v8::Function> {
    // We can't directly attach a Rust closure as a v8::Function via
    // FunctionTemplate (templates are designed for sync C++ ABI
    // callbacks, not closures). We instead build the function with
    // a static callback + an External holding the signal pointer.
    //
    // The callback resolves the signal from the External's data,
    // builds a TimeoutError reason, and runs signal_abort.

    // Box up the signal Global so the External holds a stable ptr.
    let boxed = Box::new(signal_global.clone());
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;

    let data_ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    let tmpl = v8::FunctionTemplate::builder(timeout_fired_callback)
        .data(data_ext.into())
        .build(scope);
    let func = tmpl.get_function(scope).unwrap();
    let global = v8::Global::new(scope, func);

    // Free the boxed Global when the function is GC'd. We attach
    // the finalizer to the function object itself.
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        func,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut v8::Global<v8::Object>));
        }),
    );
    std::mem::forget(weak);

    global
}

/// Static callback invoked when the timeout fires. Reads the boxed
/// signal Global from the External data, builds a TimeoutError, and
/// runs signal_abort.
fn timeout_fired_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let data = args.data();
    let Ok(ext) = v8::Local::<v8::External>::try_from(data) else {
        return;
    };
    let ptr = ext.value() as *mut v8::Global<v8::Object>;
    if ptr.is_null() {
        return;
    }
    // SAFETY: the External was minted in build_timeout_callback_function
    // and is kept alive by the wrapper's finalizer. We re-borrow as
    // immutable; the v8::Global is read-only here.
    let signal_global = unsafe { &*ptr };
    let signal_obj = v8::Local::new(scope, signal_global.clone());

    let reason = build_timeout_error(scope);
    signal_abort(scope, signal_obj, reason.into());
}

/// `AbortSignal.any(signals)` — DOM §3.3.4 "create a dependent abort
/// signal". Returns a fresh signal that aborts when any of the input
/// signals abort, with `reason` set to the first input's reason.
///
/// CRITICAL-8: transitive flattening. If an input is itself a
/// dependent signal (returned by an earlier `AbortSignal.any` call),
/// we copy its `source_signals` into the result's `source_signals`
/// rather than referencing the dependent. This way, aborting an
/// original source aborts every transitively-dependent signal in
/// O(depth=1) rather than chasing a chain.
pub fn any_static<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    signals_arg: v8::Local<v8::Value>,
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    let signal_objs = parse_sequence_of_signals(scope, signals_arg)?;

    let (result_obj, result_raw) = mint_abort_signal(scope);
    let result: &AbortSignal = unsafe { &*result_raw };

    // Step 2: short-circuit if any input signal is already aborted.
    for &input_obj in &signal_objs {
        let Some(input) = signal_from_obj(scope, input_obj) else {
            continue;
        };
        if input.aborted.get() {
            // Inherit the reason; `result.aborted = true` directly
            // (no signal_abort needed — listeners aren't registered
            // yet, dependents aren't either).
            let reason_global = input.reason.borrow().clone();
            result.aborted.set(true);
            *result.reason.borrow_mut() = reason_global;
            return Ok(result_obj);
        }
    }

    // Step 3.
    result.is_dependent.set(true);

    // Step 4: bidirectional pairing. For each input that's already a
    // dependent signal, splice in its source signals (transitive
    // flattening). For non-dependent inputs, add directly.
    for &input_obj in &signal_objs {
        let Some(input) = signal_from_obj(scope, input_obj) else {
            continue;
        };
        if input.is_dependent.get() {
            // Copy source_signals from the dependent input.
            let srcs = input.source_signals.borrow().clone();
            for src_global in srcs {
                let src_obj = v8::Local::new(scope, src_global);
                add_source_dependent_pair(scope, src_obj, result_obj);
            }
        } else {
            add_source_dependent_pair(scope, input_obj, result_obj);
        }
    }

    Ok(result_obj)
}

/// Bidirectional pair: `src.dependent_signals.push(dep)` AND
/// `dep.source_signals.push(src)`. Called from
/// `add_source_dependent_pair` for each non-dependent input in
/// AbortSignal.any.
fn add_source_dependent_pair(
    scope: &mut v8::PinScope,
    src_obj: v8::Local<v8::Object>,
    dep_obj: v8::Local<v8::Object>,
) {
    let Some(src) = signal_from_obj(scope, src_obj) else {
        return;
    };
    let Some(dep) = signal_from_obj(scope, dep_obj) else {
        return;
    };
    src.dependent_signals
        .borrow_mut()
        .push(v8::Global::new(scope, dep_obj));
    dep.source_signals
        .borrow_mut()
        .push(v8::Global::new(scope, src_obj));
}

/// Parse the `signals` argument to AbortSignal.any. Per WebIDL
/// `sequence<AbortSignal>` — must be iterable; each yielded value
/// must be an AbortSignal-shaped object (we don't enforce the
/// type strictly, just object-ness; the static factory's behaviour
/// no-ops on non-AbortSignals via `signal_from_obj`'s `None` branch).
fn parse_sequence_of_signals<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    val: v8::Local<v8::Value>,
) -> Result<Vec<v8::Local<'s, v8::Object>>, OpError> {
    if val.is_undefined() || val.is_null() {
        return Err(OpError::type_error(
            "AbortSignal.any: signals must be a sequence",
        ));
    }
    // Use Symbol.iterator to drive the iteration.
    let Ok(seq_obj) = v8::Local::<v8::Object>::try_from(val) else {
        return Err(OpError::type_error(
            "AbortSignal.any: signals must be a sequence",
        ));
    };
    let sym_iter = v8::Symbol::get_iterator(scope);
    let iter_method_v = seq_obj
        .get(scope, sym_iter.into())
        .ok_or_else(|| OpError::type_error("AbortSignal.any: not iterable"))?;
    let Ok(iter_fn) = v8::Local::<v8::Function>::try_from(iter_method_v) else {
        return Err(OpError::type_error("AbortSignal.any: not iterable"));
    };
    let iter_v = iter_fn
        .call(scope, seq_obj.into(), &[])
        .ok_or_else(|| OpError::type_error("AbortSignal.any: iterator threw"))?;
    let Ok(iter_obj) = v8::Local::<v8::Object>::try_from(iter_v) else {
        return Err(OpError::type_error("AbortSignal.any: iterator not an object"));
    };
    let next_key = v8::String::new(scope, "next").unwrap();
    let next_v = iter_obj
        .get(scope, next_key.into())
        .ok_or_else(|| OpError::type_error("AbortSignal.any: iter.next access threw"))?;
    let Ok(next_fn) = v8::Local::<v8::Function>::try_from(next_v) else {
        return Err(OpError::type_error("AbortSignal.any: iter.next not callable"));
    };
    let done_key = v8::String::new(scope, "done").unwrap();
    let value_key = v8::String::new(scope, "value").unwrap();

    let mut out: Vec<v8::Local<v8::Object>> = Vec::new();
    loop {
        let step_v = next_fn
            .call(scope, iter_obj.into(), &[])
            .ok_or_else(|| OpError::type_error("AbortSignal.any: iter.next() threw"))?;
        let Ok(step_obj) = v8::Local::<v8::Object>::try_from(step_v) else {
            return Err(OpError::type_error(
                "AbortSignal.any: iter.next() did not return an object",
            ));
        };
        let done_v = step_obj
            .get(scope, done_key.into())
            .ok_or_else(|| OpError::type_error("AbortSignal.any: step.done access threw"))?;
        if done_v.boolean_value(scope) {
            break;
        }
        let value_v = step_obj
            .get(scope, value_key.into())
            .ok_or_else(|| OpError::type_error("AbortSignal.any: step.value access threw"))?;
        let value_obj = v8::Local::<v8::Object>::try_from(value_v).map_err(|_| {
            OpError::type_error("AbortSignal.any: signals[i] must be an AbortSignal")
        })?;
        out.push(value_obj);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// install_global — wire up AbortSignal on globalThis with the static
// factory methods (`abort` / `timeout` / `any`) installed on the
// constructor function, NOT the prototype.
// ---------------------------------------------------------------------------

pub fn install_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    let tmpl = AbortSignal::install(scope);
    let class_fn = tmpl.get_function(scope).unwrap();

    // Static methods on the constructor function.
    install_static(scope, class_fn, "abort", abort_static_callback);
    install_static(scope, class_fn, "timeout", timeout_static_callback);
    install_static(scope, class_fn, "any", any_static_callback);

    // `onabort` is a WebIDL `attribute EventHandler` — an accessor
    // pair on the prototype. The `#[v8_class]` macro doesn't support
    // same-name getter+setter pairs (set_accessor_property calls
    // each separately, which V8 rejects), so we install the pair
    // via `Object.defineProperty` on the resolved prototype.
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    let proto: v8::Local<v8::Object> = proto_v.try_into().unwrap();

    let onabort_key = v8::String::new(scope, "onabort").unwrap();
    let getter_tmpl = v8::FunctionTemplate::new(scope, onabort_getter_callback);
    let setter_tmpl = v8::FunctionTemplate::new(scope, onabort_setter_callback);
    let getter_fn = getter_tmpl.get_function(scope).unwrap();
    let setter_fn = setter_tmpl.get_function(scope).unwrap();
    let mut desc = v8::PropertyDescriptor::new_from_get_set(getter_fn.into(), setter_fn.into());
    desc.set_configurable(true);
    desc.set_enumerable(true);
    proto.define_property(scope, onabort_key.into(), &desc);

    let key = v8::String::new(scope, "AbortSignal").unwrap();
    global.set(scope, key.into(), class_fn.into());
}

fn install_static<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    ctor_fn: v8::Local<v8::Function>,
    name: &str,
    callback: impl v8::MapFnTo<v8::FunctionCallback>,
) {
    let tmpl = v8::FunctionTemplate::new(scope, callback);
    let func = tmpl.get_function(scope).unwrap();
    let key = v8::String::new(scope, name).unwrap();
    ctor_fn.set(scope, key.into(), func.into());
}

// ---------------------------------------------------------------------------
// onabort accessor pair
// ---------------------------------------------------------------------------

fn onabort_getter_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    let Some(signal) = signal_from_obj(scope, this) else {
        // Per WebIDL "the brand check", reading onabort on a non-
        // AbortSignal should throw a TypeError. We follow.
        let m = v8::String::new(scope, "Illegal invocation").unwrap();
        let exc = v8::Exception::type_error(scope, m);
        scope.throw_exception(exc);
        return;
    };
    match signal.onabort.borrow().as_ref() {
        Some(fn_global) => rv.set(v8::Local::new(scope, fn_global.clone()).into()),
        None => rv.set(v8::null(scope).into()),
    }
}

fn onabort_setter_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let this = args.this();
    let Some(signal) = signal_from_obj(scope, this) else {
        // Brand check.
        let m = v8::String::new(scope, "Illegal invocation").unwrap();
        let exc = v8::Exception::type_error(scope, m);
        scope.throw_exception(exc);
        return;
    };

    let value = args.get(0);

    // First, remove the previous onabort listener (if any) from the
    // EventTarget's listener list. We track the listener by holding
    // the previous v8::Global in `signal.onabort`.
    let prev = signal.onabort.borrow_mut().take();
    if let Some(prev_global) = &prev {
        // Remove from the listener list.
        let listeners_rc = match super::event_target::listeners_of(scope, this) {
            Some(l) => l,
            None => super::event_target::attach_listeners(scope, this),
        };
        let mut map = listeners_rc.borrow_mut();
        if let Some(list) = map.get_mut("abort") {
            list.retain(|l| !(l.callback == *prev_global && !l.capture));
        }
    }

    // Per WHATWG HTML §3.2.7 "event handler IDL attribute" setter:
    // if the new value is callable, install it. If null/undefined,
    // leave nothing (already done by the take above). If a non-
    // callable value (object, etc.), the setter is a no-op (per
    // HTML's "process the activation behavior" steps that ignore
    // non-callable assignments).
    let Ok(fn_local) = v8::Local::<v8::Function>::try_from(value) else {
        return;
    };

    // Per spec, if the signal has already aborted, setting onabort
    // doesn't fire it. Same as addEventListener("abort", fn) on an
    // already-aborted signal — we go through the normal addListener
    // path which short-circuits in DOM §2.7 step 3 (signal aborted
    // → return).
    let cb_global = v8::Global::new(scope, fn_local);
    let listeners_rc = match super::event_target::listeners_of(scope, this) {
        Some(l) => l,
        None => super::event_target::attach_listeners(scope, this),
    };

    // If the signal is already aborted, addEventListener-equivalent
    // is a no-op per DOM §2.7. We DO still store the function in
    // `onabort` so the getter returns it (per HTML's "the function's
    // value" rule).
    if !signal.aborted.get() {
        listeners_rc
            .borrow_mut()
            .entry("abort".to_string())
            .or_default()
            .push(super::event_target::RegisteredListener {
                callback: cb_global.clone(),
                capture: false,
                once: false,
                passive: false,
                removed: false,
            });
    }

    *signal.onabort.borrow_mut() = Some(cb_global);
}

// ---------------------------------------------------------------------------
// Hand-rolled static-method callbacks
// ---------------------------------------------------------------------------

fn abort_static_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let reason = args.get(0);
    let signal = abort_static(scope, reason);
    rv.set(signal.into());
}

fn timeout_static_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let ms_v = args.get(0);
    // Per WebIDL [EnforceRange] unsigned long long: out-of-range
    // throws TypeError. We're permissive here and just clamp; v1
    // doesn't expose the EnforceRange surface in the macro.
    let ms = if ms_v.is_number() {
        let n = ms_v.number_value(scope).unwrap_or(0.0);
        if n.is_nan() || n < 0.0 {
            0
        } else {
            n as u64
        }
    } else {
        let s = ms_v.to_rust_string_lossy(scope);
        s.parse::<u64>().unwrap_or(0)
    };
    let signal = timeout_static(scope, ms);
    rv.set(signal.into());
}

fn any_static_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let signals_arg = args.get(0);
    match any_static(scope, signals_arg) {
        Ok(signal) => rv.set(signal.into()),
        Err(e) => {
            let m = v8::String::new(scope, &e.message).unwrap();
            let exc = match e.kind {
                crate::state::OpErrorKind::TypeError => v8::Exception::type_error(scope, m),
                crate::state::OpErrorKind::RangeError => v8::Exception::range_error(scope, m),
                _ => v8::Exception::error(scope, m),
            };
            scope.throw_exception(exc);
        }
    }
}
