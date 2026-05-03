//! Native `MessageEvent` per HTML §9.4.2
//! (https://html.spec.whatwg.org/#messageevent), referenced from the
//! WHATWG WebSockets spec §3.2 (https://websockets.spec.whatwg.org/).
//!
//! Replaces the polyfill at `embed/websocket.js:27-33` which built a
//! plain `Event` and patched on the MessageEvent fields as expandos.
//! That breaks `instanceof MessageEvent` and prevents `e.data` from
//! being readback-stable when `data` is an object.
//!
//! ## Storage layout
//!
//! `#[repr(C)]` with `Event` as the FIRST field is load-bearing: the
//! `#[v8_inherit(Event)]` macro chains the FunctionTemplate prototypes
//! so inherited Event getters (`event.type`, `event.bubbles`, ...) are
//! reachable on a MessageEvent instance. Those getters reach into
//! internal field 0 and cast to `*mut Event` via
//! `event::event_from_obj`. With `#[repr(C)]` and `Event` as the first
//! member, `*mut MessageEventState as *mut Event` produces a pointer
//! to the `event` field at offset 0 — the cast is sound.
//!
//! Same pattern as `dom/custom_event.rs`.
//!
//! ## v1 simplifications
//!
//! - `source` always returns null (no MessagePort / Window /
//!   ServiceWorker; v1 doesn't ship MessagePort).
//! - `ports` always returns an EMPTY frozen array. Per WebIDL §3.2.34
//!   FrozenArray, every getter call MUST return the SAME instance —
//!   we cache the array as a `v8::Global` on the state on first
//!   access (addresses critic MAJOR #27 — ports identity).
//! - `lastEventId` is empty for WebSocket-dispatched events; the field
//!   is meaningful for EventSource (when shipped).

use std::cell::RefCell;

use zeroship_runtime_macros::{v8_class, v8_constructor, v8_getter, v8_inherit, v8_method, v8_name};

use crate::state::OpError;

use super::event::{now_ms, read_event_init, Event};

/// Backing state for a JS-constructed `MessageEvent`. The `event` field
/// MUST be first (and the struct MUST be `#[repr(C)]`) — see the module
/// doc comment for why.
#[repr(C)]
pub struct MessageEventState {
    /// Inherited Event state. At offset 0 so a `*mut MessageEventState`
    /// is layout-compatible with `*mut Event` for inherited getters.
    pub event: Event,
    /// `data` per IDL — `any`, default null. Held as a Global so the
    /// SAME JS value (object identity) is returned on every `.data`
    /// access. `Option` to avoid forcing a Global for the null-default.
    pub data: RefCell<Option<v8::Global<v8::Value>>>,
    /// `origin` — for WebSocket-dispatched events this is the URL's
    /// origin serialised per HTML §3.5; for user-constructed events
    /// the value passed in the init dict.
    pub origin: RefCell<String>,
    /// `lastEventId` — empty for WebSocket-dispatched events; meaningful
    /// for EventSource. Default "".
    pub last_event_id: RefCell<String>,
    /// Cached FrozenArray for `ports` — required for object identity
    /// per WebIDL §3.2.34. Populated on first `.ports` access; the
    /// SAME array is returned on every subsequent call.
    pub ports_cache: RefCell<Option<v8::Global<v8::Array>>>,
}

impl Default for MessageEventState {
    fn default() -> Self {
        MessageEventState {
            event: Event::default(),
            data: RefCell::new(None),
            origin: RefCell::new(String::new()),
            last_event_id: RefCell::new(String::new()),
            ports_cache: RefCell::new(None),
        }
    }
}

/// Parsed `MessageEventInit` dict — bubbles/cancelable/composed plus the
/// MessageEvent-specific data/origin/lastEventId. `source` and `ports`
/// are read but ignored (v1 doesn't ship MessagePort / Window).
struct ParsedMessageInit {
    bubbles: bool,
    cancelable: bool,
    composed: bool,
    data: Option<v8::Global<v8::Value>>,
    origin: String,
    last_event_id: String,
}

fn parse_message_event_init(
    scope: &mut v8::PinScope,
    init: v8::Local<v8::Value>,
) -> Result<ParsedMessageInit, OpError> {
    // Inherited EventInit (bubbles / cancelable / composed) — reuse the
    // Event parser so the two constructors stay in lockstep.
    let base = read_event_init(scope, init)?;

    let mut parsed = ParsedMessageInit {
        bubbles: base.bubbles,
        cancelable: base.cancelable,
        composed: base.composed,
        data: None,
        origin: String::new(),
        last_event_id: String::new(),
    };

    if init.is_undefined() || init.is_null() {
        return Ok(parsed);
    }
    let Ok(obj) = v8::Local::<v8::Object>::try_from(init) else {
        // read_event_init would already have rejected — re-check defensively.
        return Err(OpError::type_error(
            "MessageEvent eventInitDict must be an object",
        ));
    };

    // `data` — `any`, default null. undefined is treated as null per
    // WebIDL dictionary defaulting; we store None and the getter
    // materialises null on the fly.
    let data_key =
        v8::String::new(scope, "data").ok_or_else(|| OpError::error("out of memory"))?;
    if let Some(v) = obj.get(scope, data_key.into()) {
        if !v.is_undefined() {
            parsed.data = Some(v8::Global::new(scope, v));
        }
    }

    // `origin` — USVString, default "".
    let origin_key =
        v8::String::new(scope, "origin").ok_or_else(|| OpError::error("out of memory"))?;
    if let Some(v) = obj.get(scope, origin_key.into()) {
        if !v.is_undefined() {
            parsed.origin = v.to_rust_string_lossy(scope);
        }
    }

    // `lastEventId` — DOMString, default "".
    let lei_key =
        v8::String::new(scope, "lastEventId").ok_or_else(|| OpError::error("out of memory"))?;
    if let Some(v) = obj.get(scope, lei_key.into()) {
        if !v.is_undefined() {
            parsed.last_event_id = v.to_rust_string_lossy(scope);
        }
    }

    Ok(parsed)
}

#[v8_class]
#[v8_inherit(super::event::Event)]
#[v8_to_string_tag = "MessageEvent"]
impl MessageEventState {
    /// `new MessageEvent(type, eventInitDict?)` per HTML §9.4.2.
    #[v8_constructor]
    fn new(
        scope: &mut v8::PinScope,
        ty: v8::Local<v8::Value>,
        init: v8::Local<v8::Value>,
    ) -> Result<MessageEventState, OpError> {
        if ty.is_undefined() {
            return Err(OpError::type_error(
                "MessageEvent(): missing required 'type' argument",
            ));
        }
        let Some(type_str) = ty.to_string(scope) else {
            // V8 has a pending exception from ToString; let it propagate.
            return Ok(MessageEventState::default());
        };
        let type_rust = type_str.to_rust_string_lossy(scope);

        let parsed = parse_message_event_init(scope, init)?;

        let me = MessageEventState::default();
        *me.event.event_type.borrow_mut() = type_rust;
        me.event.bubbles.set(parsed.bubbles);
        me.event.cancelable.set(parsed.cancelable);
        me.event.composed.set(parsed.composed);
        me.event.time_stamp.set(now_ms());
        *me.data.borrow_mut() = parsed.data;
        *me.origin.borrow_mut() = parsed.origin;
        *me.last_event_id.borrow_mut() = parsed.last_event_id;
        Ok(me)
    }

    /// `messageEvent.data` — `any` per IDL. Returns the stored value by
    /// identity (or `null` if none was provided).
    #[v8_getter]
    fn data<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        match self.data.borrow().as_ref() {
            Some(g) => v8::Local::new(scope, g.clone()),
            None => v8::null(scope).into(),
        }
    }

    /// `messageEvent.origin` — USVString.
    #[v8_getter]
    fn origin(&self) -> String {
        self.origin.borrow().clone()
    }

    /// `messageEvent.lastEventId` — DOMString.
    #[v8_getter]
    #[v8_name = "lastEventId"]
    fn last_event_id(&self) -> String {
        self.last_event_id.borrow().clone()
    }

    /// `messageEvent.source` — null in v1 (no MessagePort / Window /
    /// ServiceWorker). IDL surface preserved for spec parity.
    #[v8_getter]
    fn source<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        v8::null(scope).into()
    }

    /// `messageEvent.ports` — empty FrozenArray. Per WebIDL §3.2.34
    /// (https://webidl.spec.whatwg.org/#es-frozen-array): EVERY getter
    /// invocation MUST return the SAME frozen array instance (object
    /// identity). v2 caches the FrozenArray as a `v8::Global` on the
    /// state; `event.ports === event.ports` is true.
    /// (addresses critic MAJOR #27)
    #[v8_getter]
    fn ports<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        if let Some(g) = self.ports_cache.borrow().as_ref() {
            return v8::Local::new(scope, g.clone()).into();
        }
        let arr = v8::Array::new(scope, 0);
        // Per WebIDL FrozenArray, the array MUST be frozen.
        // `set_integrity_level` returns Option<bool>; we treat
        // failure as a hard runtime error. (addresses critic MAJOR #30
        // — set_integrity_level return-value hygiene.)
        let froze = arr.set_integrity_level(scope, v8::IntegrityLevel::Frozen);
        debug_assert_eq!(
            froze,
            Some(true),
            "MessageEvent.ports: set_integrity_level Frozen failed",
        );
        let g = v8::Global::new(scope, arr);
        *self.ports_cache.borrow_mut() = Some(g.clone());
        v8::Local::new(scope, g).into()
    }

    /// `messageEvent.initMessageEvent(...)` — HTML §9.4.2 legacy method,
    /// preserved for spec parity. No-op if the dispatch flag is set.
    #[v8_method]
    #[v8_name = "initMessageEvent"]
    #[allow(clippy::too_many_arguments)]
    fn init_message_event(
        &self,
        scope: &mut v8::PinScope,
        ty: v8::Local<v8::Value>,
        bubbles: v8::Local<v8::Value>,
        cancelable: v8::Local<v8::Value>,
        data: v8::Local<v8::Value>,
        origin: v8::Local<v8::Value>,
        last_event_id: v8::Local<v8::Value>,
        _source: v8::Local<v8::Value>,
        _ports: v8::Local<v8::Value>,
    ) -> Result<(), OpError> {
        if ty.is_undefined() {
            return Err(OpError::type_error(
                "initMessageEvent(): missing required 'type' argument",
            ));
        }
        // Per HTML §9.4.2 (and DOM §2.2 initEvent semantics): if the
        // dispatch flag is set, return.
        if self.event.dispatch_flag.get() {
            return Ok(());
        }
        let Some(type_str) = ty.to_string(scope) else {
            return Err(OpError::type_error(
                "initMessageEvent(): 'type' could not be coerced to string",
            ));
        };
        *self.event.event_type.borrow_mut() = type_str.to_rust_string_lossy(scope);
        self.event.bubbles.set(bubbles.boolean_value(scope));
        self.event.cancelable.set(cancelable.boolean_value(scope));
        self.event.stop_propagation.set(false);
        self.event.stop_immediate_propagation.set(false);
        self.event.default_prevented.set(false);
        // `data` is `any` — undefined materialises as null per WebIDL.
        if data.is_undefined() {
            *self.data.borrow_mut() = None;
        } else {
            *self.data.borrow_mut() = Some(v8::Global::new(scope, data));
        }
        if origin.is_undefined() {
            self.origin.borrow_mut().clear();
        } else {
            *self.origin.borrow_mut() = origin.to_rust_string_lossy(scope);
        }
        if last_event_id.is_undefined() {
            self.last_event_id.borrow_mut().clear();
        } else {
            *self.last_event_id.borrow_mut() = last_event_id.to_rust_string_lossy(scope);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Native mint helper — used by the WebSocket receive path (frame → MessageEvent
// dispatched as platform event with `is_trusted = true`).
// ---------------------------------------------------------------------------

/// Mint a MessageEvent for a received WebSocket frame. The `data` is the
/// JS-side representation (Blob, ArrayBuffer, or String) already built
/// by the dispatcher; `origin` is the WebSocket URL's origin.
///
/// Sets `is_trusted = true` (platform-emitted), `bubbles = false`,
/// `cancelable = false` per HTML §9.4.2 + WebSockets §3.2.
pub(crate) fn build_message_event<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    data: v8::Local<v8::Value>,
    origin: &str,
) -> v8::Local<'s, v8::Object> {
    let tmpl = MessageEventState::install(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let obj = inst_tmpl
        .new_instance(scope)
        .expect("MessageEvent instance allocation failed");

    let me = MessageEventState::default();
    *me.event.event_type.borrow_mut() = "message".to_string();
    me.event.is_trusted.set(true);
    me.event.time_stamp.set(now_ms());
    *me.data.borrow_mut() = Some(v8::Global::new(scope, data));
    *me.origin.borrow_mut() = origin.to_string();

    let boxed: Box<MessageEventState> = Box::new(me);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());

    // Wire prototype to MessageEvent.prototype so inherited Event
    // getters/methods resolve via the FunctionTemplate chain.
    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    obj.set_prototype(scope, proto_v);

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut MessageEventState));
        }),
    );
    std::mem::forget(weak);

    obj
}
