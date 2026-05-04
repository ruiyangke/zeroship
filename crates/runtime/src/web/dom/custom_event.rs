//! Native `CustomEvent` per DOM §2.4
//! (https://dom.spec.whatwg.org/#interface-customevent).
//!
//! ```idl
//! [Exposed=*]
//! interface CustomEvent : Event {
//!     constructor(DOMString type, optional CustomEventInit eventInitDict = {});
//!     readonly attribute any detail;
//!     undefined initCustomEvent(DOMString type, optional boolean bubbles = false,
//!                               optional boolean cancelable = false,
//!                               optional any detail = null);
//! };
//! dictionary CustomEventInit : EventInit {
//!     any detail = null;
//! };
//! ```
//!
//! Replaces the 27-LOC `embed/events.js` polyfill that wrapped a
//! native `Event` and patched on `.detail`. The polyfill couldn't
//! preserve `instanceof CustomEvent` reliably (it overrode the
//! prototype on a synthesised `new Event(...)` instance) and
//! couldn't make `e.detail` immutable.
//!
//! ## Storage layout
//!
//! `#[repr(C)]` with `Event` as the FIRST field is load-bearing: the
//! `#[v8_inherit(Event)]` macro chains the FunctionTemplate prototypes
//! so inherited Event getters (`event.type`, `event.bubbles`, ...)
//! are reachable on a CustomEvent instance. Those getters reach into
//! internal field 0 and cast to `*mut Event` via
//! `event::event_from_obj`. Internal field 0 of a CustomEvent wrapper
//! actually holds `*mut CustomEvent`. With `#[repr(C)]` and Event as
//! the first member, `*mut CustomEvent as *mut Event` produces a
//! pointer to the `event` field at offset 0 — the cast is sound and
//! the inherited methods read the right Event state.
//!
//! `EventTarget.dispatchEvent` similarly invokes `event_from_obj` on
//! the event arg to set target/currentTarget/eventPhase. With the
//! repr(C) layout, dispatching a CustomEvent works without any
//! special-casing in the dispatcher.
//!
//! ## detail storage
//!
//! `detail` is `any` per IDL — held as a `v8::Global<v8::Value>` so
//! the same JS identity is returned on every `.detail` access (per
//! WebIDL the field is read-only and readback-stable).

use std::cell::RefCell;

use zeroship_runtime_macros::{
    v8_class, v8_constructor, v8_getter, v8_inherit, v8_method, v8_name, WebIdlDict,
};

use crate::state::OpError;

use super::event::{now_ms, Event};

/// Backing state for a JS-constructed `CustomEvent`. The `event` field
/// MUST be first (and the struct MUST be `#[repr(C)]`) — see the module
/// doc comment for why.
#[repr(C)]
pub struct CustomEvent {
    /// Inherited Event state. Stored at offset 0 so a `*mut CustomEvent`
    /// is layout-compatible with `*mut Event` for the inherited Event
    /// getters/methods that cast internal field 0 directly.
    pub event: Event,
    /// `detail` per CustomEventInit IDL — `any`, default null. Held as
    /// a Global so the SAME JS value (object identity) is returned on
    /// every `.detail` access. Stored as `Option` to avoid forcing a
    /// `v8::Global` for the null-default case (we materialise null on
    /// the fly in the getter — same observable shape).
    pub detail: RefCell<Option<v8::Global<v8::Value>>>,
}

impl Default for CustomEvent {
    fn default() -> Self {
        CustomEvent {
            event: Event::default(),
            detail: RefCell::new(None),
        }
    }
}

/// `CustomEventInit` per DOM §2.4. Inherits from `EventInit` per
/// the IDL, but the macro doesn't support derive-with-inheritance,
/// so the parent fields (`bubbles`/`cancelable`/`composed`) are
/// inlined here. `detail` rides as an `Option<Local<Value>>` so the
/// caller can either `.map(...)` to a `Global` (constructor path) or
/// observe `None` for the missing/undefined case (per IDL default
/// = null, materialised on the JS surface in the `.detail` getter).
#[derive(Default, Debug, WebIdlDict)]
struct CustomEventInit<'s> {
    bubbles: bool,
    cancelable: bool,
    composed: bool,
    detail: Option<v8::Local<'s, v8::Value>>,
}

#[v8_class]
#[v8_inherit(super::event::Event)]
impl CustomEvent {
    /// `new CustomEvent(type, eventInitDict?)` per DOM §2.4. Reads
    /// `bubbles`/`cancelable`/`composed` (inherited EventInit) plus
    /// `detail` (CustomEvent-specific) from the init dict and stores
    /// them on the embedded Event + the detail slot.
    ///
    /// Mirrors `Event::new`'s handling of a failing ToString on `type`:
    /// if V8 has a pending exception we yield by returning Ok(default).
    #[v8_constructor]
    fn new(
        scope: &mut v8::PinScope,
        ty: v8::Local<v8::Value>,
        init: v8::Local<v8::Value>,
    ) -> Result<CustomEvent, OpError> {
        if ty.is_undefined() {
            return Err(OpError::type_error(
                "CustomEvent(): missing required 'type' argument",
            ));
        }
        let Some(type_str) = ty.to_string(scope) else {
            // V8 has a pending exception from ToString; let it propagate.
            return Ok(CustomEvent::default());
        };
        let type_rust = type_str.to_rust_string_lossy(scope);

        // Single dict parse covers inherited EventInit fields
        // (bubbles/cancelable/composed) plus CustomEventInit's own
        // `detail`. Per IDL default `detail = null`, and the dict
        // converter maps both "missing key" and "undefined value" to
        // None — same observable shape as the previous hand-roll.
        let init_dict = CustomEventInit::from_v8(scope, init)?;

        let detail_global: Option<v8::Global<v8::Value>> = init_dict
            .detail
            .map(|v| v8::Global::new(scope, v));

        let ce = CustomEvent::default();
        *ce.event.event_type.borrow_mut() = type_rust;
        ce.event.bubbles.set(init_dict.bubbles);
        ce.event.cancelable.set(init_dict.cancelable);
        ce.event.composed.set(init_dict.composed);
        ce.event.time_stamp.set(now_ms());
        *ce.detail.borrow_mut() = detail_global;

        Ok(ce)
    }

    /// `customEvent.detail` — `any` per IDL. Returns the stored value
    /// by identity (or `null` if none was provided).
    #[v8_getter]
    fn detail<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        match self.detail.borrow().as_ref() {
            Some(g) => v8::Local::new(scope, g.clone()),
            None => v8::null(scope).into(),
        }
    }

    /// `customEvent.initCustomEvent(type, bubbles?, cancelable?, detail?)`
    /// — DOM §2.4, legacy method preserved for spec parity. No-op if the
    /// event has already been dispatched (matches `Event.initEvent`).
    /// `type` is a required arg per IDL — calling without it must throw
    /// TypeError (WPT `dom/events/CustomEvent.html` subtest 2).
    #[v8_method]
    #[v8_name = "initCustomEvent"]
    fn init_custom_event(
        &self,
        scope: &mut v8::PinScope,
        ty: v8::Local<v8::Value>,
        bubbles: v8::Local<v8::Value>,
        cancelable: v8::Local<v8::Value>,
        detail: v8::Local<v8::Value>,
    ) -> Result<(), OpError> {
        // Required arg per WebIDL: throw TypeError when called with no
        // args. (WebIDL marks missing required args as a TypeError; the
        // macro can't see arg-count from a `Local<Value>` shape, so we
        // detect "no arg" via `is_undefined()` — the same heuristic
        // Event::new uses for its required `type` arg.)
        if ty.is_undefined() {
            return Err(OpError::type_error(
                "initCustomEvent(): missing required 'type' argument",
            ));
        }
        // Per DOM "If this's dispatch flag is set, then return."
        if self.event.dispatch_flag.get() {
            return Ok(());
        }
        let Some(type_str) = ty.to_string(scope) else {
            return Err(OpError::type_error(
                "initCustomEvent(): 'type' could not be coerced to string",
            ));
        };
        *self.event.event_type.borrow_mut() = type_str.to_rust_string_lossy(scope);
        self.event.bubbles.set(bubbles.boolean_value(scope));
        self.event.cancelable.set(cancelable.boolean_value(scope));
        // Reset transient flags per the spec's call-through to "initialize".
        self.event.stop_propagation.set(false);
        self.event.stop_immediate_propagation.set(false);
        self.event.default_prevented.set(false);
        // `detail` arg is `any` — undefined materialises as null per
        // WebIDL. Otherwise hold the Global by identity.
        if detail.is_undefined() {
            *self.detail.borrow_mut() = None;
        } else {
            *self.detail.borrow_mut() = Some(v8::Global::new(scope, detail));
        }
        Ok(())
    }
}
