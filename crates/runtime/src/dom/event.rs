//! Native `Event` per DOM §2.2 (https://dom.spec.whatwg.org/#interface-event).
//!
//! v1 simplifications (documented inline):
//!   - There is no DOM tree, so capture/bubble are no-ops. `eventPhase`
//!     is `NONE (0)` before dispatch and `AT_TARGET (2)` during dispatch.
//!   - `composedPath()` returns `[target]` after dispatch, `[]` otherwise.
//!   - Trusted vs untrusted: all events constructed from JS are
//!     untrusted (`isTrusted === false`); platform-emitted events
//!     (e.g. AbortSignal's "abort") will set `isTrusted = true` via
//!     a Rust-only path that bypasses the JS constructor.
//!
//! These simplifications match what runtimes like Cloudflare Workers
//! and Deno expose in non-DOM contexts; user code that depends on
//! true DOM event-tree semantics is out of scope for the platform.
//!
//! Storage rule (§XIII.4): all slots live in Rust on the boxed
//! `EventState` in internal field 0. The `currentTarget` and `target`
//! fields are stored as `Option<v8::Global<v8::Object>>` for JS
//! identity preservation.

use std::cell::{Cell, RefCell};

use zeroship_runtime_macros::v8_class;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_constructor, v8_getter, v8_method, v8_name};

use crate::state::OpError;

// ---------------------------------------------------------------------------
// Event phase constants — DOM §2.2.
// ---------------------------------------------------------------------------

/// `Event.NONE` — DOM `eventPhase` constant. Set before dispatch.
pub const EVENT_PHASE_NONE: u32 = 0;
/// `Event.CAPTURING_PHASE` — never observed in v1 (no DOM tree).
pub const EVENT_PHASE_CAPTURING: u32 = 1;
/// `Event.AT_TARGET` — set during dispatch.
pub const EVENT_PHASE_AT_TARGET: u32 = 2;
/// `Event.BUBBLING_PHASE` — never observed in v1 (no DOM tree).
pub const EVENT_PHASE_BUBBLING: u32 = 3;

// ---------------------------------------------------------------------------
// Event struct
// ---------------------------------------------------------------------------

/// State for a JS-constructed `Event`. Backs the V8 wrapper via
/// internal field 0.
pub struct Event {
    /// Event type (e.g. "abort"). DOMString — UTF-16 code units; we
    /// hold it as a `String` (UTF-8) since all WHATWG event types
    /// in practice are ASCII. Per spec, the type is set once at
    /// construction (or via `initEvent`) and read-only afterwards.
    pub event_type: RefCell<String>,
    /// `bubbles` flag (DOM §2.2). v1 has no DOM tree, so this is
    /// pure data round-trip.
    pub bubbles: Cell<bool>,
    /// `cancelable` flag.
    pub cancelable: Cell<bool>,
    /// `composed` flag — for shadow-DOM event retargeting; pure data
    /// round-trip in v1.
    pub composed: Cell<bool>,
    /// `defaultPrevented` flag. Set by `preventDefault()` iff the
    /// event is `cancelable`.
    pub default_prevented: Cell<bool>,
    /// `[[stop propagation]]` flag — set by `stopPropagation()`.
    pub stop_propagation: Cell<bool>,
    /// `[[stop immediate propagation]]` flag — set by
    /// `stopImmediatePropagation()`. Causes `dispatchEvent` to bail
    /// out of the listener loop after the current listener.
    pub stop_immediate_propagation: Cell<bool>,
    /// `isTrusted` — true iff the event was emitted by the platform
    /// (e.g. AbortSignal's "abort" event). Always false for events
    /// constructed via `new Event(...)`.
    pub is_trusted: Cell<bool>,
    /// `eventPhase` — see EVENT_PHASE_* constants.
    pub event_phase: Cell<u32>,
    /// `[[dispatch flag]]` — set during dispatchEvent, cleared after.
    pub dispatch_flag: Cell<bool>,
    /// `[[in passive listener flag]]` — true while a passive listener
    /// is running. `preventDefault()` is a no-op while this is set.
    pub in_passive_listener: Cell<bool>,
    /// `target` — set on first dispatch. `Option` because pre-dispatch
    /// it's null per spec.
    pub target: RefCell<Option<v8::Global<v8::Object>>>,
    /// `currentTarget` — the EventTarget currently dispatching the
    /// event. Set during dispatch, cleared after.
    pub current_target: RefCell<Option<v8::Global<v8::Object>>>,
    /// `timeStamp` — milliseconds since Unix epoch (high-resolution
    /// time isn't available without a context).
    pub time_stamp: Cell<f64>,
}

impl Default for Event {
    fn default() -> Self {
        Event {
            event_type: RefCell::new(String::new()),
            bubbles: Cell::new(false),
            cancelable: Cell::new(false),
            composed: Cell::new(false),
            default_prevented: Cell::new(false),
            stop_propagation: Cell::new(false),
            stop_immediate_propagation: Cell::new(false),
            is_trusted: Cell::new(false),
            event_phase: Cell::new(EVENT_PHASE_NONE),
            dispatch_flag: Cell::new(false),
            in_passive_listener: Cell::new(false),
            target: RefCell::new(None),
            current_target: RefCell::new(None),
            time_stamp: Cell::new(0.0),
        }
    }
}

// ---------------------------------------------------------------------------
// EventInit dictionary parser
// ---------------------------------------------------------------------------

#[derive(Default, Debug)]
struct EventInit {
    bubbles: bool,
    cancelable: bool,
    composed: bool,
}

fn read_event_init(
    scope: &mut v8::PinScope,
    val: v8::Local<v8::Value>,
) -> Result<EventInit, OpError> {
    if val.is_undefined() || val.is_null() {
        return Ok(EventInit::default());
    }
    let Ok(obj) = v8::Local::<v8::Object>::try_from(val) else {
        return Err(OpError::type_error("Event eventInitDict must be an object"));
    };
    let bubbles = read_bool_prop(scope, obj, "bubbles")?;
    let cancelable = read_bool_prop(scope, obj, "cancelable")?;
    let composed = read_bool_prop(scope, obj, "composed")?;
    Ok(EventInit {
        bubbles,
        cancelable,
        composed,
    })
}

fn read_bool_prop(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    key: &str,
) -> Result<bool, OpError> {
    let key_v8 = v8::String::new(scope, key)
        .ok_or_else(|| OpError::error("out of memory"))?;
    let val = obj
        .get(scope, key_v8.into())
        .ok_or_else(|| OpError::error("property access threw"))?;
    Ok(val.boolean_value(scope))
}

// ---------------------------------------------------------------------------
// time helper — DOM `event.timeStamp` is "MS since Unix epoch with
// reduced precision" outside browsers; in non-browser contexts we
// return a monotonic-ish number.
// ---------------------------------------------------------------------------

fn now_ms() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Event IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
impl Event {
    /// `new Event(type: DOMString, eventInitDict?: EventInit)` — DOM §2.2.
    /// The constructor stores type + flags from the init dict; all other
    /// state defaults to spec-minimum (no target, no currentTarget,
    /// `isTrusted = false`).
    #[v8_constructor]
    fn new(
        scope: &mut v8::PinScope,
        ty: v8::Local<v8::Value>,
        init: v8::Local<v8::Value>,
    ) -> Result<Event, OpError> {
        if ty.is_undefined() {
            return Err(OpError::type_error(
                "Event(): missing required 'type' argument",
            ));
        }
        // Per WebIDL DOMString conversion of `type`. Symbols throw
        // TypeError per ECMA-262 ToString.
        let Some(type_str) = ty.to_string(scope) else {
            return Err(OpError::type_error(
                "Event(): 'type' could not be coerced to string",
            ));
        };
        let type_rust = type_str.to_rust_string_lossy(scope);
        let init_dict = read_event_init(scope, init)?;
        let ev = Event::default();
        *ev.event_type.borrow_mut() = type_rust;
        ev.bubbles.set(init_dict.bubbles);
        ev.cancelable.set(init_dict.cancelable);
        ev.composed.set(init_dict.composed);
        ev.time_stamp.set(now_ms());
        Ok(ev)
    }

    /// `event.type` getter — DOM §2.2.
    #[v8_getter]
    #[v8_name = "type"]
    fn type_(&self) -> String {
        self.event_type.borrow().clone()
    }

    /// `event.bubbles` getter.
    #[v8_getter]
    fn bubbles(&self) -> bool {
        self.bubbles.get()
    }

    /// `event.cancelable` getter.
    #[v8_getter]
    fn cancelable(&self) -> bool {
        self.cancelable.get()
    }

    /// `event.composed` getter.
    #[v8_getter]
    fn composed(&self) -> bool {
        self.composed.get()
    }

    /// `event.defaultPrevented` getter.
    #[v8_getter]
    #[v8_name = "defaultPrevented"]
    fn default_prevented(&self) -> bool {
        self.default_prevented.get()
    }

    /// `event.isTrusted` getter — `false` for JS-constructed events,
    /// `true` for platform-emitted ones (e.g. AbortSignal's "abort").
    #[v8_getter]
    #[v8_name = "isTrusted"]
    fn is_trusted(&self) -> bool {
        self.is_trusted.get()
    }

    /// `event.eventPhase` getter — `NONE (0)` outside dispatch,
    /// `AT_TARGET (2)` during dispatch in v1 (no DOM tree).
    #[v8_getter]
    #[v8_name = "eventPhase"]
    fn event_phase(&self) -> u32 {
        self.event_phase.get()
    }

    /// `event.timeStamp` getter — ms since Unix epoch at construction.
    /// Returns f64 so JS sees a high-resolution number.
    #[v8_getter]
    #[v8_name = "timeStamp"]
    fn time_stamp(&self) -> f64 {
        self.time_stamp.get()
    }

    /// `event.target` getter — the EventTarget the event was dispatched
    /// at. Null pre-dispatch. The wrapper-identity is preserved via
    /// `v8::Global<v8::Object>`.
    #[v8_getter]
    fn target<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        match self.target.borrow().as_ref() {
            Some(g) => v8::Local::new(scope, g.clone()).into(),
            None => v8::null(scope).into(),
        }
    }

    /// `event.srcElement` — legacy alias for `event.target` (DOM §2.2,
    /// `srcElement` is the legacy IE name preserved by the spec for
    /// compat). Same getter.
    #[v8_getter]
    #[v8_name = "srcElement"]
    fn src_element<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        match self.target.borrow().as_ref() {
            Some(g) => v8::Local::new(scope, g.clone()).into(),
            None => v8::null(scope).into(),
        }
    }

    /// `event.currentTarget` getter — the target currently dispatching.
    /// Null outside dispatch.
    #[v8_getter]
    #[v8_name = "currentTarget"]
    fn current_target<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        match self.current_target.borrow().as_ref() {
            Some(g) => v8::Local::new(scope, g.clone()).into(),
            None => v8::null(scope).into(),
        }
    }

    /// `event.returnValue` — legacy IE getter/setter alias. Per DOM
    /// §2.2 it's `!defaultPrevented` for the getter; setter to `false`
    /// equates to `preventDefault()`. v1 only implements the getter.
    #[v8_getter]
    #[v8_name = "returnValue"]
    fn return_value(&self) -> bool {
        !self.default_prevented.get()
    }

    /// `event.composedPath()` — DOM §2.2. v1 has no DOM tree, so
    /// returns `[target]` after dispatch (or `[]` if pre-dispatch
    /// or target is null).
    #[v8_method]
    #[v8_name = "composedPath"]
    fn composed_path<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        let arr = v8::Array::new(scope, 0);
        if let Some(g) = self.target.borrow().as_ref() {
            let target_local = v8::Local::new(scope, g.clone());
            arr.set_index(scope, 0, target_local.into());
        }
        arr.into()
    }

    /// `event.stopPropagation()` — DOM §2.2. Sets the stop-propagation
    /// flag. In v1 this only affects whether dependent signal events
    /// continue to fire (no DOM tree); the flag is stored for spec
    /// compliance.
    #[v8_method]
    #[v8_name = "stopPropagation"]
    fn stop_propagation_method(&self) {
        self.stop_propagation.set(true);
    }

    /// `event.stopImmediatePropagation()` — DOM §2.2. Sets BOTH the
    /// stop-propagation and stop-immediate-propagation flags. The
    /// latter causes `dispatchEvent` to bail out of the listener
    /// loop immediately after the current listener returns.
    #[v8_method]
    #[v8_name = "stopImmediatePropagation"]
    fn stop_immediate_propagation_method(&self) {
        self.stop_propagation.set(true);
        self.stop_immediate_propagation.set(true);
    }

    /// `event.preventDefault()` — DOM §2.2. Sets `defaultPrevented` to
    /// true iff the event is cancelable AND we're not in a passive
    /// listener.
    #[v8_method]
    #[v8_name = "preventDefault"]
    fn prevent_default(&self) {
        if self.cancelable.get() && !self.in_passive_listener.get() {
            self.default_prevented.set(true);
        }
    }

    /// `event.initEvent(type, bubbles?, cancelable?)` — DOM §2.2,
    /// legacy method preserved for compat. No-op if the event has
    /// already been dispatched. Otherwise resets the type/bubbles/
    /// cancelable fields and clears the propagation/canceled flags.
    #[v8_method]
    #[v8_name = "initEvent"]
    fn init_event(
        &self,
        scope: &mut v8::PinScope,
        ty: v8::Local<v8::Value>,
        bubbles: v8::Local<v8::Value>,
        cancelable: v8::Local<v8::Value>,
    ) -> Result<(), OpError> {
        // Per spec: if dispatch flag is set, return.
        if self.dispatch_flag.get() {
            return Ok(());
        }
        let Some(type_str) = ty.to_string(scope) else {
            return Err(OpError::type_error(
                "initEvent(): 'type' could not be coerced to string",
            ));
        };
        *self.event_type.borrow_mut() = type_str.to_rust_string_lossy(scope);
        // `bubbles`/`cancelable` default to false when undefined.
        self.bubbles.set(bubbles.boolean_value(scope));
        self.cancelable.set(cancelable.boolean_value(scope));
        // Reset transient flags per spec.
        self.stop_propagation.set(false);
        self.stop_immediate_propagation.set(false);
        self.default_prevented.set(false);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Constants on the constructor (Event.NONE / Event.CAPTURING_PHASE / ...)
// ---------------------------------------------------------------------------

/// Install `Event.NONE / .CAPTURING_PHASE / .AT_TARGET / .BUBBLING_PHASE`
/// as integer-valued data properties on the constructor function and
/// the prototype (per WebIDL §3.7.5 "constants on interfaces").
pub fn install_event_constants<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    ctor_fn: v8::Local<v8::Function>,
) {
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = ctor_fn.get(scope, proto_key.into()).unwrap();
    let proto: v8::Local<v8::Object> = proto_v.try_into().unwrap();

    for (name, val) in [
        ("NONE", EVENT_PHASE_NONE),
        ("CAPTURING_PHASE", EVENT_PHASE_CAPTURING),
        ("AT_TARGET", EVENT_PHASE_AT_TARGET),
        ("BUBBLING_PHASE", EVENT_PHASE_BUBBLING),
    ] {
        let key = v8::String::new(scope, name).unwrap();
        let v = v8::Integer::new_from_unsigned(scope, val);
        // Per WebIDL §3.7.5 the descriptor is { writable: false,
        // enumerable: true, configurable: false }. PropertyAttribute
        // bits: READ_ONLY = !writable, DONT_DELETE = !configurable.
        // We use `set` (which produces a writable+enumerable+
        // configurable data property) — close enough for v1; spec-
        // strict descriptors can come later.
        ctor_fn.set(scope, key.into(), v.into());
        proto.set(scope, key.into(), v.into());
    }
}

// ---------------------------------------------------------------------------
// Internal helpers — used by EventTarget.dispatchEvent and AbortSignal.
// ---------------------------------------------------------------------------

/// Get the `Event` boxed state from a V8 object via internal field 0.
/// Returns `None` if the object isn't an Event wrapper (no internal
/// field, or the field isn't an External).
///
/// SAFETY: caller must ensure `obj` is a JS Event wrapper; the
/// External holds a `*mut Event` we cast back. The unsafe re-borrow
/// is sound because (a) V8 isolates are per-thread and (b) the boxed
/// state is reachable via the wrapper's GC root.
pub(crate) fn event_from_obj<'a>(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
) -> Option<&'a Event> {
    let ext = obj
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())?;
    let ptr = ext.value() as *mut Event;
    if ptr.is_null() {
        return None;
    }
    // SAFETY: see function comment.
    Some(unsafe { &*ptr })
}

/// Mint a fresh "abort" Event for AbortSignal. Bypasses the JS
/// constructor so `isTrusted = true`. The event is `bubbles = false`,
/// `cancelable = false` per the DOM spec entry for AbortSignal's
/// abort event.
pub(crate) fn build_abort_event<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::Object> {
    let tmpl = Event::install(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let obj = inst_tmpl
        .new_instance(scope)
        .expect("Event instance allocation failed");

    let ev = Event::default();
    *ev.event_type.borrow_mut() = "abort".to_string();
    ev.is_trusted.set(true);
    ev.time_stamp.set(now_ms());

    let boxed: Box<Event> = Box::new(ev);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());

    // Wire prototype to Event.prototype so the methods/getters from
    // the FunctionTemplate are reachable.
    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    obj.set_prototype(scope, proto_v);

    // Finalizer: reclaim the Box on GC or isolate teardown.
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut Event));
        }),
    );
    std::mem::forget(weak);

    obj
}
