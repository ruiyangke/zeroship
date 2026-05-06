//! Native `EventTarget` per DOM §2.7
//! (https://dom.spec.whatwg.org/#interface-eventtarget).
//!
//! v1 simplifications (documented):
//!   - There is no DOM tree, so the capture/bubble phase walk is
//!     collapsed to a single AT_TARGET dispatch — the listener list
//!     for `event.type` is invoked in registration order.
//!   - `event.bubbles` is round-tripped but not honoured (no parent
//!     to bubble to); `event.composed` likewise.
//!   - The listener invariants (DOM §2.7) are honoured exactly:
//!     same callback + same capture flag deduplicates; `once`
//!     removes after one fire; `signal` auto-removes on abort;
//!     `passive` blocks `preventDefault` from taking effect.
//!
//! ## Listener storage and the EventTarget-or-derived-class problem
//!
//! `dispatchEvent` and the cross-class `signal`-removal hook need a
//! listener list keyed by event type. AbortSignal inherits
//! EventTarget (`signal instanceof EventTarget === true`), which
//! means a JS object can be EITHER:
//!   - a vanilla EventTarget — internal field 0 holds Box<EventTarget>;
//!   - a derived class wrapper (e.g. AbortSignal) — internal field 0
//!     holds Box<DerivedClassState>, NOT Box<EventTarget>.
//!
//! Casting the External directly to `&EventTarget` is unsound for
//! the derived-class path. Instead, the listener storage hangs off
//! the JS wrapper as a V8 private symbol:
//!
//!   wrapper [private "__listeners"] -> External(*mut Rc<RefCell<…>>)
//!
//! The construct path (`attach_listeners(scope, obj)`) creates the Rc
//! and attaches it; the lookup path (`listeners_of(scope, obj)`)
//! reads the private symbol and clones the Rc. Both EventTarget's
//! own constructor AND AbortSignal's mint helper call
//! `attach_listeners`. This way the dispatch path is the same for
//! both classes — no unsafe field-offset shenanigans.
//!
//! The Rc is owned by an Outer Box on the heap; a guaranteed
//! finalizer drops the Box when the wrapper is GC'd (just like the
//! `#[v8_class]` macro does for its boxed state).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::state::OpError;

use super::event::{event_from_obj, EVENT_PHASE_AT_TARGET, EVENT_PHASE_NONE};

// ---------------------------------------------------------------------------
// RegisteredListener
// ---------------------------------------------------------------------------

/// A single listener registered on an EventTarget.
///
/// Stored in `listeners[type]` in registration order. The dedup key
/// per DOM §2.7 step 3 is `(callback, capture)` — same callback +
/// capture is a no-op re-registration. Per DOM §2.7 the `passive`
/// and `once` flags from the initial registration are NOT updated
/// on re-registration.
pub struct RegisteredListener {
    /// The callback. Per WebIDL `EventListener` callback interface
    /// it can also be an object with a `handleEvent` method, but in
    /// practice every consumer passes a function — v1 only handles
    /// the function path; an object listener fails the type
    /// coercion at registration. Tracked as a known gap.
    pub callback: v8::Global<v8::Function>,
    /// Capture flag — pure data round-trip (no DOM tree means no
    /// capture phase). Used as part of the dedup key.
    pub capture: bool,
    /// Once flag — listener is removed after one invocation.
    pub once: bool,
    /// Passive flag — `event.preventDefault()` is a no-op while
    /// this listener is running.
    pub passive: bool,
    /// Removed flag — set when the listener is being removed mid-
    /// dispatch. Snapshots taken at the start of dispatch include
    /// this listener; the dispatcher re-checks it on each iteration
    /// to honour mid-dispatch removals.
    pub removed: bool,
}

/// Listener list per type. `Rc<RefCell<...>>` so a single Rc can be
/// (a) hung off the JS wrapper as a private-symbol External and
/// (b) shared with cross-class abort-algorithm closures (the signal
/// removal hook).
pub type ListenersByType = Rc<RefCell<HashMap<String, Vec<RegisteredListener>>>>;

// ---------------------------------------------------------------------------
// EventTarget — minimal JS-facing struct
// ---------------------------------------------------------------------------

/// `EventTarget` instance state. Empty on purpose — the listener Rc
/// hangs off the JS wrapper via a private symbol so derived classes
/// (AbortSignal) get the same lookup path.
///
/// We DON'T use the `#[v8_class]` macro for EventTarget because the
/// IDL methods (addEventListener/removeEventListener/dispatchEvent)
/// must live on the FUNCTION TEMPLATE's prototype_template — not on
/// the JS-level prototype object — so that derived classes
/// (AbortSignal via `#[v8_inherit(EventTarget)]`) inherit them via
/// V8's template-inheritance chain.
///
/// Patching `class_fn.prototype` after install (the way `headers.rs`
/// does for keys/values/entries) doesn't work for derived-class
/// inheritance: V8's `FunctionTemplate::inherit` chains the
/// PROTOTYPE_TEMPLATEs, not the resolved prototype OBJECTS. Properties
/// added to the resolved prototype after `inherit` was wired don't
/// propagate to the derived class. The derived class's
/// `instance.prototype.__proto__ === parent.prototype`, but the
/// parent's prototype is fresh per realm — reading from it picks up
/// only what was on its prototype_template at template-creation time.
///
/// The fix is to install via the prototype_template BEFORE the
/// derived class's `inherit` call resolves the parent template. Our
/// custom `EventTarget::install` does that.
#[derive(Default)]
pub struct EventTarget;

// The `#[v8_class]` cache slot type from the macro convention. We
// emit this manually since we're hand-rolling install (the macro
// would emit it but we're not using the macro here).
#[doc(hidden)]
#[allow(non_camel_case_types)]
pub struct __InstallSlot_EventTarget(::v8::Global<::v8::FunctionTemplate>);

impl EventTarget {
    /// Install the EventTarget FunctionTemplate. Idempotent per
    /// isolate via the `__InstallSlot_EventTarget` cache slot — same
    /// pattern the `#[v8_class]` macro uses, kept compatible so
    /// `#[v8_inherit(EventTarget)]` on AbortSignal resolves the same
    /// template every call.
    ///
    /// The methods (`addEventListener` etc.) are installed on the
    /// PROTOTYPE TEMPLATE so derived-class inheritance picks them up
    /// via V8's `FunctionTemplate::inherit` mechanism (which chains
    /// templates, not resolved prototype objects).
    pub fn install<'s>(
        scope: &mut v8::PinScope<'s, '_>,
    ) -> v8::Local<'s, v8::FunctionTemplate> {
        if let Some(cached) = scope.get_slot::<__InstallSlot_EventTarget>() {
            return v8::Local::new(scope, cached.0.clone());
        }

        let ctor_tmpl = v8::FunctionTemplate::new(scope, et_constructor_callback);
        let class_name = v8::String::new(scope, "EventTarget").unwrap();
        ctor_tmpl.set_class_name(class_name);
        ctor_tmpl
            .instance_template(scope)
            .set_internal_field_count(1);

        let proto_tmpl = ctor_tmpl.prototype_template(scope);

        // addEventListener / removeEventListener / dispatchEvent on
        // the prototype_template → derived classes inherit via the
        // FunctionTemplate::inherit chain (DOM §3.3 AbortSignal :
        // EventTarget).
        install_proto_method(scope, proto_tmpl, "addEventListener", add_event_listener_callback);
        install_proto_method(
            scope,
            proto_tmpl,
            "removeEventListener",
            remove_event_listener_callback,
        );
        install_proto_method(scope, proto_tmpl, "dispatchEvent", dispatch_event_callback);

        // Symbol.toStringTag for spec-correct "[object EventTarget]".
        let tag_sym = v8::Symbol::get_to_string_tag(scope);
        let tag_value = v8::String::new(scope, "EventTarget").unwrap();
        proto_tmpl.set_with_attr(
            tag_sym.into(),
            tag_value.into(),
            v8::PropertyAttribute::READ_ONLY | v8::PropertyAttribute::DONT_ENUM,
        );

        let global = v8::Global::new(scope, ctor_tmpl);
        let local = v8::Local::new(scope, global.clone());
        scope.set_slot(__InstallSlot_EventTarget(global));
        local
    }

    /// Macro-shape `register` (#198). Hand-rolled to match
    /// `Self::install` above, so `register_native_classes!` can drive
    /// EventTarget alongside its `#[v8_class]` siblings.
    pub fn register<'s>(
        scope: &mut v8::PinScope<'s, '_>,
        global: v8::Local<v8::Object>,
    ) {
        let tmpl = Self::install(scope);
        let class_fn = tmpl.get_function(scope).unwrap();
        let key = v8::String::new(scope, "EventTarget").unwrap();
        global.set(scope, key.into(), class_fn.into());
    }
}

// Hand-rolled constructor for `new EventTarget()`. Allocates the
// boxed state, attaches via internal field 0, registers the GC
// finalizer.
fn et_constructor_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let this = args.this();
    let boxed = Box::new(EventTarget);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    this.set_internal_field(0, ext.into());
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        this,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut EventTarget));
        }),
    );
    std::mem::forget(weak);
}

fn install_proto_method<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    proto_tmpl: v8::Local<v8::ObjectTemplate>,
    name: &str,
    callback: impl v8::MapFnTo<v8::FunctionCallback>,
) {
    let key = v8::String::new(scope, name).unwrap();
    let fn_tmpl = v8::FunctionTemplate::new(scope, callback);
    proto_tmpl.set(key.into(), fn_tmpl.into());
}

// ---------------------------------------------------------------------------
// Listener-options parser
// ---------------------------------------------------------------------------

/// Parse the third argument of addEventListener / removeEventListener.
struct ListenerOptions {
    capture: bool,
    once: bool,
    passive: bool,
    /// The AbortSignal V8 object, if a `signal` member was provided.
    signal: Option<v8::Global<v8::Object>>,
}

impl Default for ListenerOptions {
    fn default() -> Self {
        ListenerOptions {
            capture: false,
            once: false,
            passive: false,
            signal: None,
        }
    }
}

/// Per DOM §2.7 "flatten more": addEventListener flattens `capture`,
/// `once`, `passive`, AND `signal` from the options dict.
fn read_listener_options(
    scope: &mut v8::PinScope,
    val: v8::Local<v8::Value>,
) -> Result<ListenerOptions, OpError> {
    if val.is_undefined() || val.is_null() {
        return Ok(ListenerOptions::default());
    }
    if val.is_boolean() {
        return Ok(ListenerOptions {
            capture: val.boolean_value(scope),
            once: false,
            passive: false,
            signal: None,
        });
    }
    let Ok(obj) = v8::Local::<v8::Object>::try_from(val) else {
        return Err(OpError::type_error(
            "addEventListener: options must be an object or boolean",
        ));
    };

    let capture = read_bool_prop(scope, obj, "capture")?;
    let once = read_bool_prop(scope, obj, "once")?;
    let passive = read_bool_prop(scope, obj, "passive")?;

    let signal_key = v8::String::new(scope, "signal").unwrap();
    let signal_v = obj
        .get(scope, signal_key.into())
        .ok_or_else(|| OpError::error("options.signal access threw"))?;
    let signal = if signal_v.is_undefined() {
        None
    } else if signal_v.is_null() {
        // Per WPT AddEventListenerOptions-signal: `signal: null` is a
        // TypeError.
        return Err(OpError::type_error(
            "addEventListener: 'signal' may not be null",
        ));
    } else if let Ok(signal_obj) = v8::Local::<v8::Object>::try_from(signal_v) {
        Some(v8::Global::new(scope, signal_obj))
    } else {
        return Err(OpError::type_error(
            "addEventListener: 'signal' must be an AbortSignal",
        ));
    };

    Ok(ListenerOptions {
        capture,
        once,
        passive,
        signal,
    })
}

/// Per DOM §2.7 "flatten" (NOT "flatten more"): removeEventListener
/// only reads `capture` from the options dict. WPT's
/// AddEventListenerOptions-passive specifically asserts that the
/// `passive` getter is NOT invoked by removeEventListener (since
/// passive isn't a removal-key part).
fn read_remove_options(
    scope: &mut v8::PinScope,
    val: v8::Local<v8::Value>,
) -> Result<bool, OpError> {
    if val.is_undefined() || val.is_null() {
        return Ok(false);
    }
    if val.is_boolean() {
        return Ok(val.boolean_value(scope));
    }
    let Ok(obj) = v8::Local::<v8::Object>::try_from(val) else {
        return Err(OpError::type_error(
            "removeEventListener: options must be an object or boolean",
        ));
    };
    read_bool_prop(scope, obj, "capture")
}

fn read_bool_prop(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    key: &str,
) -> Result<bool, OpError> {
    let key_v8 =
        v8::String::new(scope, key).ok_or_else(|| OpError::error("out of memory"))?;
    let val = obj
        .get(scope, key_v8.into())
        .ok_or_else(|| OpError::error("property access threw"))?;
    Ok(val.boolean_value(scope))
}

// ---------------------------------------------------------------------------
// Listener-Rc plumbing — private-symbol-backed
// ---------------------------------------------------------------------------

/// Per-isolate cached private symbol for the listeners Rc. We can't
/// use `v8::Symbol::for_api(scope, "__zs_listeners")` (which would be
/// global to the realm and observable from JS) — Symbol.for is
/// public. Instead we use a `Private` symbol stored in the isolate's
/// slot system.
struct ListenersSymSlot(v8::Global<v8::Private>);

fn get_or_create_listeners_sym<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::Private> {
    if let Some(s) = scope.get_slot::<ListenersSymSlot>() {
        return v8::Local::new(scope, s.0.clone());
    }
    let name = v8::String::new(scope, "zs::dom::listeners").unwrap();
    let sym = v8::Private::new(scope, Some(name));
    let global = v8::Global::new(scope, sym);
    let local = v8::Local::new(scope, global.clone());
    scope.set_slot(ListenersSymSlot(global));
    local
}

/// Build a fresh listener Rc, attach it to `obj` via the private
/// symbol, and register a finalizer that drops the Box when the
/// wrapper is GC'd. Idempotent: if `obj` already has the symbol set,
/// returns the existing Rc.
pub fn attach_listeners(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
) -> ListenersByType {
    let sym = get_or_create_listeners_sym(scope);

    if let Some(existing) = obj.get_private(scope, sym) {
        if let Ok(ext) = v8::Local::<v8::External>::try_from(existing) {
            let ptr = ext.value() as *const ListenersByType;
            if !ptr.is_null() {
                // SAFETY: we placed this pointer ourselves. The
                // Box's lifetime is tied to the wrapper via a
                // guaranteed finalizer (see below).
                let rc: &ListenersByType = unsafe { &*ptr };
                return rc.clone();
            }
        }
    }

    let listeners: ListenersByType = Rc::new(RefCell::new(HashMap::new()));
    let boxed = Box::new(listeners.clone());
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;

    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_private(scope, sym, ext.into());

    // Finalizer: drop the Box on GC / isolate teardown. Same shape
    // as the macro's `gen_box_and_install_finalizer`.
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut ListenersByType));
        }),
    );
    std::mem::forget(weak);

    listeners
}

/// Read the listeners Rc attached to `obj`. Returns `None` if the
/// wrapper hasn't had `attach_listeners` called on it (i.e. it isn't
/// an EventTarget or derived class).
pub fn listeners_of(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
) -> Option<ListenersByType> {
    let sym = get_or_create_listeners_sym(scope);
    let existing = obj.get_private(scope, sym)?;
    let ext = v8::Local::<v8::External>::try_from(existing).ok()?;
    let ptr = ext.value() as *const ListenersByType;
    if ptr.is_null() {
        return None;
    }
    // SAFETY: see attach_listeners.
    let rc: &ListenersByType = unsafe { &*ptr };
    Some(rc.clone())
}

// ---------------------------------------------------------------------------
// dispatch_event — the public dispatch helper
// ---------------------------------------------------------------------------

/// Invoke listeners for `event.type` registered on `target_obj`.
/// Sets `event.target` / `event.currentTarget` to `target_obj` for
/// the duration of dispatch. Returns `true` if no listener called
/// `preventDefault()`, `false` otherwise.
///
/// Per DOM §2.7, the listener list is snapshotted at the start of
/// dispatch — listeners added MID-dispatch are NOT invoked, and
/// listeners removed mid-dispatch ARE skipped via the `removed`
/// flag (re-checked against the live RefCell on each iteration).
pub fn dispatch_event(
    scope: &mut v8::PinScope,
    target_obj: v8::Local<v8::Object>,
    event_obj: v8::Local<v8::Object>,
) -> bool {
    let Some(ev) = event_from_obj(scope, event_obj) else {
        return true;
    };
    let target_for_event = v8::Global::new(scope, target_obj);

    // Set up event state for dispatch.
    ev.dispatch_flag.set(true);
    ev.event_phase.set(EVENT_PHASE_AT_TARGET);
    if ev.target.borrow().is_none() {
        *ev.target.borrow_mut() = Some(target_for_event.clone());
    }
    *ev.current_target.borrow_mut() = Some(target_for_event);

    let event_type = ev.event_type.borrow().clone();
    let listeners_rc = match listeners_of(scope, target_obj) {
        Some(l) => l,
        None => {
            ev.dispatch_flag.set(false);
            ev.event_phase.set(EVENT_PHASE_NONE);
            *ev.current_target.borrow_mut() = None;
            return true;
        }
    };

    // Snapshot the listener list so listeners added MID-dispatch
    // aren't invoked; we re-check the `removed` flag on each
    // iteration to honour mid-dispatch removals.
    let snapshot: Vec<(v8::Global<v8::Function>, bool, bool, bool)> = {
        let map = listeners_rc.borrow();
        match map.get(&event_type) {
            Some(list) => list
                .iter()
                .map(|l| (l.callback.clone(), l.capture, l.once, l.passive))
                .collect(),
            None => Vec::new(),
        }
    };

    for (cb_global, capture, once, passive) in snapshot {
        // Re-check live state for `removed`.
        let still_present = {
            let map = listeners_rc.borrow();
            map.get(&event_type)
                .map(|list| {
                    list.iter().any(|l| {
                        !l.removed && l.callback == cb_global && l.capture == capture
                    })
                })
                .unwrap_or(false)
        };
        if !still_present {
            continue;
        }

        // Per DOM §2.7 invoke step 6: if `once`, remove the listener
        // BEFORE invoking the callback. This way a re-entrant
        // dispatchEvent from within the callback sees the listener
        // as already removed (matches WPT's "once nested" test).
        if once {
            let mut map = listeners_rc.borrow_mut();
            if let Some(list) = map.get_mut(&event_type) {
                for l in list.iter_mut() {
                    if !l.removed && l.callback == cb_global && l.capture == capture {
                        l.removed = true;
                        break;
                    }
                }
                list.retain(|l| !l.removed);
            }
        }

        let prior_passive = ev.in_passive_listener.get();
        if passive {
            ev.in_passive_listener.set(true);
        }

        // Invoke the listener with `this = target_obj`. Errors
        // are swallowed via TryCatch (DOM §2.7 step 4.4 says
        // "report an exception"; in v1 we have no console.error
        // routing to a window-level handler, so we just absorb it
        // — the exception doesn't propagate out of dispatchEvent).
        let cb_local = v8::Local::new(scope, cb_global.clone());
        {
            v8::tc_scope!(let tc, scope);
            cb_local.call(tc, target_obj.into(), &[event_obj.into()]);
            let _ = tc.exception();
        }

        ev.in_passive_listener.set(prior_passive);

        if ev.stop_immediate_propagation.get() {
            break;
        }
    }

    // Clear dispatch state per spec. `target` is NOT cleared (the
    // spec keeps it set on the event after dispatch).
    ev.dispatch_flag.set(false);
    ev.event_phase.set(EVENT_PHASE_NONE);
    *ev.current_target.borrow_mut() = None;

    !ev.default_prevented.get()
}

// ---------------------------------------------------------------------------
// Internal listener install/remove — used by EventHandler IDL attributes
// (onopen / onabort / etc.) per HTML §8.1.5.1.
//
// Same semantics as `addEventListener` / `removeEventListener` but
// callable from Rust. The listener is added with capture=false,
// once=false, passive=false (matching the EventHandler IDL contract).
// ---------------------------------------------------------------------------

/// Install a Rust-driven listener on `target_obj` for `event_name`.
/// Idempotent on `(callback, capture)` pair — same dedup as
/// `addEventListener`. Used by EventHandler IDL setters
/// (`socket.onmessage = f`) so dispatchEvent finds the handler.
///
/// Note: this attaches a listener Rc (idempotent) on first call.
pub fn add_internal_listener(
    scope: &mut v8::PinScope,
    target_obj: v8::Local<v8::Object>,
    event_name: &str,
    cb: v8::Global<v8::Function>,
) {
    let listeners_rc = attach_listeners(scope, target_obj);
    let already_present = {
        let map = listeners_rc.borrow();
        map.get(event_name)
            .map(|list| {
                list.iter()
                    .any(|l| !l.removed && !l.capture && l.callback == cb)
            })
            .unwrap_or(false)
    };
    if !already_present {
        listeners_rc
            .borrow_mut()
            .entry(event_name.to_string())
            .or_default()
            .push(RegisteredListener {
                callback: cb,
                capture: false,
                once: false,
                passive: false,
                removed: false,
            });
    }
}

/// Remove ALL non-capture listeners with the given event_name from the
/// target. Used by EventHandler IDL setters when REPLACING a previously
/// installed handler — the IDL contract is "remove the previous internal
/// listener installed by this attribute, install the new one". Since
/// each EventHandler attribute has at most one internal listener at a
/// time, removing all non-capture matches for the event_name is sound
/// and matches undici's behaviour
/// (`undici/lib/web/websocket/websocket.js:355-445`).
pub fn remove_internal_listener(
    scope: &mut v8::PinScope,
    target_obj: v8::Local<v8::Object>,
    event_name: &str,
) {
    let Some(listeners_rc) = listeners_of(scope, target_obj) else {
        return;
    };
    let mut map = listeners_rc.borrow_mut();
    if let Some(list) = map.get_mut(event_name) {
        list.retain(|l| l.capture);
    }
}

// #198 — `install_global` removed; bind happens via the macro-emitted
// `EventTarget::register` invoked from `dom::install_globals`'s
// `register_native_classes!` list.

// ---------------------------------------------------------------------------
// Hand-rolled callbacks
// ---------------------------------------------------------------------------

fn add_event_listener_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let this = args.this();
    // Idempotent: `attach_listeners` returns the existing Rc if one
    // is already attached. Constructed-via-`new` wrappers don't
    // get listeners attached in the macro path, so we do it
    // lazily here on first interaction. (For AbortSignal, the
    // mint helper attaches at construction.)
    let listeners_rc = attach_listeners(scope, this);

    let ty = args.get(0);
    let callback = args.get(1);
    let options = args.get(2);

    if ty.is_undefined() {
        throw_type_error(scope, "addEventListener: missing 'type' argument");
        return;
    }
    let Some(type_str) = ty.to_string(scope) else {
        throw_type_error(
            scope,
            "addEventListener: 'type' could not be coerced to string",
        );
        return;
    };
    let type_rust = type_str.to_rust_string_lossy(scope);

    // Per DOM §2.7 the options dictionary is flattened FIRST (which
    // invokes its getters — observable for feature-detection tests
    // like AddEventListenerOptions-passive). The null-callback
    // short-circuit comes AFTER.
    let opts = match read_listener_options(scope, options) {
        Ok(o) => o,
        Err(e) => {
            throw_op_error(scope, &e);
            return;
        }
    };

    if callback.is_null() || callback.is_undefined() {
        return;
    }
    let Ok(cb_fn) = v8::Local::<v8::Function>::try_from(callback) else {
        throw_type_error(
            scope,
            "addEventListener: callback must be a function or null",
        );
        return;
    };

    if let Some(signal_global) = &opts.signal {
        let signal_obj = v8::Local::new(scope, signal_global.clone());
        if super::abort_signal::is_aborted(scope, signal_obj) {
            return;
        }
    }

    let cb_global = v8::Global::new(scope, cb_fn);
    let already_present = {
        let map = listeners_rc.borrow();
        map.get(&type_rust)
            .map(|list| {
                list.iter().any(|l| {
                    !l.removed && l.capture == opts.capture && l.callback == cb_global
                })
            })
            .unwrap_or(false)
    };
    if !already_present {
        listeners_rc
            .borrow_mut()
            .entry(type_rust.clone())
            .or_default()
            .push(RegisteredListener {
                callback: cb_global.clone(),
                capture: opts.capture,
                once: opts.once,
                passive: opts.passive,
                removed: false,
            });
    }

    if let Some(signal_global) = opts.signal {
        let signal_local = v8::Local::new(scope, signal_global.clone());
        let listeners_for_cb = listeners_rc.clone();
        let type_for_cb = type_rust;
        let cb_for_cb = cb_global;
        let capture_for_cb = opts.capture;
        super::abort_signal::add_abort_algorithm(
            scope,
            signal_local,
            Box::new(move || {
                let mut map = listeners_for_cb.borrow_mut();
                if let Some(list) = map.get_mut(&type_for_cb) {
                    list.retain(|l| {
                        !(l.callback == cb_for_cb && l.capture == capture_for_cb)
                    });
                }
            }),
        );
    }
}

fn remove_event_listener_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let this = args.this();
    // For removeEventListener, missing private-symbol means there's
    // never been an addEventListener — silently no-op.
    let Some(listeners_rc) = listeners_of(scope, this) else {
        return;
    };

    let ty = args.get(0);
    let callback = args.get(1);
    let options = args.get(2);

    if ty.is_undefined() {
        throw_type_error(scope, "removeEventListener: missing 'type' argument");
        return;
    }
    let Some(type_str) = ty.to_string(scope) else {
        throw_type_error(
            scope,
            "removeEventListener: 'type' could not be coerced to string",
        );
        return;
    };
    let type_rust = type_str.to_rust_string_lossy(scope);

    // Per DOM §2.7 the "flatten" used by removeEventListener only
    // reads `capture`. Reading happens BEFORE the callback null check
    // so that side-effecting getters fire (matches WPT's
    // AddEventListenerOptions-passive `removeEventListener should
    // not support passive` test which checks the `passive` getter
    // is NOT invoked — i.e. only `capture` is read).
    let capture = match read_remove_options(scope, options) {
        Ok(c) => c,
        Err(e) => {
            throw_op_error(scope, &e);
            return;
        }
    };

    if callback.is_null() || callback.is_undefined() {
        return;
    }
    let Ok(cb_fn) = v8::Local::<v8::Function>::try_from(callback) else {
        return;
    };

    let cb_global = v8::Global::new(scope, cb_fn);
    let mut map = listeners_rc.borrow_mut();
    if let Some(list) = map.get_mut(&type_rust) {
        list.retain(|l| !(l.capture == capture && l.callback == cb_global));
    }
}

fn dispatch_event_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    // Even with no listeners ever registered, dispatchEvent on a
    // valid EventTarget should return true (no preventDefault
    // possible). Attach lazily.
    attach_listeners(scope, this);

    let event = args.get(0);
    let Ok(event_obj) = v8::Local::<v8::Object>::try_from(event) else {
        throw_type_error(scope, "dispatchEvent: event must be an Event object");
        return;
    };
    let Some(ev) = event_from_obj(scope, event_obj) else {
        throw_type_error(scope, "dispatchEvent: event is not an Event instance");
        return;
    };

    if ev.dispatch_flag.get() {
        throw_invalid_state(scope, "dispatchEvent: event is already being dispatched");
        return;
    }

    let result = dispatch_event(scope, this, event_obj);
    rv.set(v8::Boolean::new(scope, result).into());
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

fn throw_type_error(scope: &mut v8::PinScope, msg: &str) {
    let m = v8::String::new(scope, msg).unwrap();
    let exc = v8::Exception::type_error(scope, m);
    scope.throw_exception(exc);
}

fn throw_invalid_state(scope: &mut v8::PinScope, msg: &str) {
    // Spec name: InvalidStateError DOMException. We don't have a
    // native DOMException yet, so throw a generic Error with the
    // message — matches what the JS polyfill did.
    let m = v8::String::new(scope, msg).unwrap();
    let exc = v8::Exception::error(scope, m);
    scope.throw_exception(exc);
}

fn throw_op_error(scope: &mut v8::PinScope, err: &OpError) {
    // JsValue path is the user-exception passthrough: rethrow the
    // captured value verbatim so `catch` blocks observe the original
    // (Error subclass, e.code, custom props all preserved).
    if let crate::state::OpErrorKind::JsValue(global) = &err.kind {
        let local = v8::Local::new(scope, global);
        scope.throw_exception(local);
        return;
    }
    let m = v8::String::new(scope, &err.message).unwrap();
    let exc = match &err.kind {
        crate::state::OpErrorKind::TypeError => v8::Exception::type_error(scope, m),
        crate::state::OpErrorKind::RangeError => v8::Exception::range_error(scope, m),
        _ => v8::Exception::error(scope, m),
    };
    scope.throw_exception(exc);
}
