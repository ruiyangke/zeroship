//! Native `CloseEvent` per WHATWG WebSockets §3.2
//! (https://websockets.spec.whatwg.org/#closeevent).
//!
//! Replaces the polyfill at `embed/websocket.js:35-41` which built a
//! plain `Event` and patched on the CloseEvent fields as expandos.
//!
//! ## Storage layout
//!
//! `#[repr(C)]` with `Event` as the FIRST field is load-bearing — same
//! pattern as `dom/custom_event.rs` and `dom/message_event.rs`.
//!
//! ## CloseEventInit.code conversion
//!
//! Per WebIDL `unsigned short code = 0` (NO `[Clamp]` attribute) — the
//! conversion follows ConvertToInt's default case
//! (https://webidl.spec.whatwg.org/#abstract-opdef-converttoint):
//!   1. NaN / ±∞ → 0.
//!   2. Truncate toward zero.
//!   3. Modulo 2^16 (signed wrap).
//!
//! `[Clamp]` is reserved for `WebSocket.close()`'s `code` argument
//! (see `websocket/algorithms.rs::clamp_unsigned_short`).
//!
//! Examples:
//!   - `new CloseEvent("close", { code: 1.5 })` → code === 1 (truncate)
//!   - `new CloseEvent("close", { code: -1 })` → code === 65535 (modulo)
//!   - `new CloseEvent("close", { code: NaN })` → code === 0

use std::cell::{Cell, RefCell};

use zeroship_runtime_macros::{
    v8_class, WebIdlDict,
};

use crate::state::OpError;

use super::event::{now_ms, Event};

#[repr(C)]
pub struct CloseEventState {
    /// Inherited Event state — at offset 0 (#[repr(C)]) so `*mut CloseEventState`
    /// is layout-compatible with `*mut Event`.
    pub event: Event,
    /// `wasClean` — boolean. Default false per IDL.
    pub was_clean: Cell<bool>,
    /// `code` — unsigned short. Default 0 per IDL.
    pub code: Cell<u16>,
    /// `reason` — USVString. Default "" per IDL.
    pub reason: RefCell<String>,
}

impl Default for CloseEventState {
    fn default() -> Self {
        CloseEventState {
            event: Event::default(),
            was_clean: Cell::new(false),
            code: Cell::new(0),
            reason: RefCell::new(String::new()),
        }
    }
}

/// WebIDL `unsigned short` conversion per
/// https://webidl.spec.whatwg.org/#abstract-opdef-converttoint
/// (default case, no extended attributes):
///   1. Let V be the input coerced to a Number (V8 ToNumber).
///   2. If V is NaN, +0, −0, +∞, or −∞: return 0.
///   3. Let V be sign(V) × floor(|V|).
///   4. Let V be V modulo 2^16 (signed → unsigned wrap).
///
/// Used by CloseEventInit.code parsing. Distinct from `[Clamp]`, which
/// applies to `WebSocket.close()`'s code argument.
pub(crate) fn convert_unsigned_short_modulo(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
) -> u16 {
    let n = value.number_value(scope).unwrap_or(0.0);
    if !n.is_finite() {
        return 0; // NaN / ±∞
    }
    // sign × floor(|V|) — truncate toward zero.
    let truncated = n.trunc();
    // Modulo 2^16 with signed → unsigned wrap. Cast through i64 to
    // capture negatives, then bitmask to u16.
    let as_i64 = truncated as i64;
    (as_i64 as u32 & 0xFFFF) as u16
}

/// `CloseEventInit` per WHATWG WebSockets §3.2. Inherits from EventInit
/// per IDL; the macro doesn't support derive-with-inheritance, so the
/// parent fields are inlined here.
///
/// `code` rides as a `Option<v8::Local<Value>>` because the spec
/// `unsigned short` conversion (ConvertToInt default case — NaN → 0,
/// truncate toward zero, modulo 2^16) doesn't match either of the
/// macro's primitive integer impls (`u32` is ToUint32 modulo 2^32;
/// `i32` is ToInt32 modulo 2^32 signed). The constructor applies
/// `convert_unsigned_short_modulo` post-parse to honour the spec.
#[derive(Default, Debug, WebIdlDict)]
struct CloseEventInit<'s> {
    bubbles: bool,
    cancelable: bool,
    composed: bool,
    #[webidl_name = "wasClean"]
    was_clean: bool,
    code: Option<v8::Local<'s, v8::Value>>,
    reason: String,
}

#[v8_class]
#[v8_inherit(super::event::Event)]
#[v8_to_string_tag = "CloseEvent"]
impl CloseEventState {
    /// `new CloseEvent(type, eventInitDict?)` per WHATWG §3.2.
    #[v8_constructor]
    fn new(
        scope: &mut v8::PinScope,
        ty: v8::Local<v8::Value>,
        init: v8::Local<v8::Value>,
    ) -> Result<CloseEventState, OpError> {
        if ty.is_undefined() {
            return Err(OpError::type_error(
                "CloseEvent(): missing required 'type' argument",
            ));
        }
        let Some(type_str) = ty.to_string(scope) else {
            return Ok(CloseEventState::default());
        };
        let type_rust = type_str.to_rust_string_lossy(scope);

        let parsed = CloseEventInit::from_v8(scope, init)?;
        let code_u16 = match parsed.code {
            Some(v) => convert_unsigned_short_modulo(scope, v),
            None => 0,
        };

        let ce = CloseEventState::default();
        *ce.event.event_type.borrow_mut() = type_rust;
        ce.event.bubbles.set(parsed.bubbles);
        ce.event.cancelable.set(parsed.cancelable);
        ce.event.composed.set(parsed.composed);
        ce.event.time_stamp.set(now_ms());
        ce.was_clean.set(parsed.was_clean);
        ce.code.set(code_u16);
        *ce.reason.borrow_mut() = parsed.reason;
        Ok(ce)
    }

    /// `closeEvent.wasClean` getter.
    #[v8_getter]
    #[v8_name = "wasClean"]
    fn was_clean(&self) -> bool {
        self.was_clean.get()
    }

    /// `closeEvent.code` getter.
    #[v8_getter]
    fn code(&self) -> u32 {
        self.code.get() as u32
    }

    /// `closeEvent.reason` getter.
    #[v8_getter]
    fn reason(&self) -> String {
        self.reason.borrow().clone()
    }
}

// ---------------------------------------------------------------------------
// Native mint helper — used by the WebSocket receive path (Close frame
// or connection-failed → CloseEvent dispatched as platform event with
// `is_trusted = true`).
// ---------------------------------------------------------------------------

/// Mint a CloseEvent for the WebSocket close path. `code`/`reason`/`was_clean`
/// come from the receive loop's frame parse or a connection-failed
/// dispatch. Sets `is_trusted = true`, `bubbles = false`, `cancelable = false`
/// per WHATWG §3.2.
pub(crate) fn build_close_event<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    code: u16,
    reason: &str,
    was_clean: bool,
) -> v8::Local<'s, v8::Object> {
    let tmpl = CloseEventState::install(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let obj = inst_tmpl
        .new_instance(scope)
        .expect("CloseEvent instance allocation failed");

    let ce = CloseEventState::default();
    *ce.event.event_type.borrow_mut() = "close".to_string();
    ce.event.is_trusted.set(true);
    ce.event.time_stamp.set(now_ms());
    ce.was_clean.set(was_clean);
    ce.code.set(code);
    *ce.reason.borrow_mut() = reason.to_string();

    let boxed: Box<CloseEventState> = Box::new(ce);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());

    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    obj.set_prototype(scope, proto_v);

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut CloseEventState));
        }),
    );
    std::mem::forget(weak);

    obj
}
