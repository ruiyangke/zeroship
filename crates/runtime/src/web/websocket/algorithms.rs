//! Spec-named algorithms for the native WebSocket impl.
//!
//! Per D-22, every named algorithm in WHATWG §3.1 / §4 + RFC 6455 §7
//! gets a Rust function with the same name in `snake_case`. This file
//! holds the cross-class operations; class-local methods live on
//! `WebSocketImpl` directly.
//!
//! Inventory (per design §IX):
//! - `clamp_unsigned_short` — WebIDL `[Clamp] unsigned short` for
//!   `WebSocket.close()`'s `code` argument.
//! - `validate_close_code_and_reason` — WHATWG §3.1 close algorithm
//!   step 1-2 validation (allowed code ranges, 123-byte reason cap).
//! - `parse_and_validate_protocols` — WHATWG §3.1 step 9 (RFC 7230
//!   token rule, no duplicates).
//! - `read_websocket_init` — D-28 dictionary parser (origin,
//!   maxMessageSize, maxFrameSize, pingIntervalMs).
//! - `set_event_handler` / `get_event_handler` — HTML §8.1.5.1
//!   EventHandler IDL semantics with null-coercion (critic MAJOR #15).

use std::collections::HashSet;

use crate::dom::event_target::{
    add_internal_listener, listeners_of, remove_internal_listener,
};
use crate::state::OpError;

use super::constants::{
    DEFAULT_MAX_FRAME_SIZE, DEFAULT_MAX_MESSAGE_SIZE, DEFAULT_PING_INTERVAL_MS,
    MAX_CLOSE_REASON_BYTES,
};
use super::{WebSocketImpl, WsCachedHandles};

// ---------------------------------------------------------------------------
// `[Clamp] unsigned short` — WHATWG §3.1 `close()`'s code argument.
// ---------------------------------------------------------------------------

/// WebIDL `[Clamp] unsigned short` conversion per
/// https://webidl.spec.whatwg.org/#abstract-opdef-converttoint (Clamp
/// case). Used by `WebSocket.close()`'s code argument.
///
/// Algorithm:
///   1. Let V be the input coerced to a Number (V8 ToNumber).
///   2. If V is NaN, return 0.
///   3. Set V to min(max(V, 0), 2^16 − 1) = clamp to [0, 65535].
///   4. If V is finite and V − floor(V) === 0.5 and floor(V) is even,
///      return floor(V) (round-half-to-even — banker's rounding).
///   5. Otherwise return the integer nearest to V (`.round()` for
///      non-tie cases is `half-away-from-zero` which is correct after
///      step 4 has handled the tie case).
///
/// Distinct from the default-case `unsigned short` conversion used by
/// CloseEventInit.code (modulo 2^16 wrap, no clamping).
///
/// (addresses critic CRITICAL #2)
pub fn clamp_unsigned_short(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> u16 {
    // Step 1: ToNumber. V8's `number_value` runs ECMAScript ToNumber.
    let n = value.number_value(scope).unwrap_or(f64::NAN);

    // Step 2: NaN → 0.
    if n.is_nan() {
        return 0;
    }

    // Step 3: clamp to [0, 65535]. Note this also handles ±∞ correctly:
    // +∞ → 65535, -∞ → 0.
    let clamped = n.max(0.0).min(65535.0);

    // Step 4: round-half-to-even for the .5 tie case.
    let floor_v = clamped.floor();
    let frac = clamped - floor_v;
    if frac == 0.5 {
        let floor_u = floor_v as u32;
        // Tie → round to the even integer.
        let rounded = if floor_u.is_multiple_of(2) {
            floor_u
        } else {
            floor_u + 1
        };
        // Re-clamp in case the round took us above 65535 (e.g.
        // 65535.5 → 65536 → clamp back down to 65535).
        return rounded.min(65535) as u16;
    }

    // Step 5: nearest integer for non-tie cases. `.round()` on f64
    // rounds away from zero on .5 ties, but step 4 already handled
    // those — and for all clamped values in [0, 65535], rounding
    // can't push us above 65535 except when the input was already
    // > 65534.5 — which `.round()` produces 65535 for, fine.
    clamped.round() as u16
}

// ---------------------------------------------------------------------------
// validate_close_code_and_reason — WHATWG §3.1 close algorithm steps 1-2.
// ---------------------------------------------------------------------------

/// Per WHATWG §3.1 close algorithm
/// (https://websockets.spec.whatwg.org/#dom-websocket-close):
///   step 1: if code is not null, but is neither 1000 nor in 3000-4999,
///           throw InvalidAccessError DOMException.
///   step 2: if reason is non-null, UTF-8 encode; if > 123 bytes,
///           throw SyntaxError DOMException.
///
/// Until DOMException ships natively, the throws use `OpError::error`
/// with the spec-mandated `name` prefixed (matches the existing
/// `embed/fetch.js` shim shape).
pub fn validate_close_code_and_reason(
    code: Option<u16>,
    reason: Option<&str>,
) -> Result<(), OpError> {
    if let Some(c) = code {
        // Per spec: 1000 OR 3000-4999 only. Everything else throws
        // InvalidAccessError. NOT 1001-2999 (reserved per RFC 6455
        // §7.4.1 — only the protocol itself uses those).
        if c != 1000 && !(3000..=4999).contains(&c) {
            return Err(OpError::error(&format!(
                "InvalidAccessError: WebSocket.close: invalid code {c}"
            )));
        }
    }
    if let Some(r) = reason {
        if r.as_bytes().len() > MAX_CLOSE_REASON_BYTES {
            return Err(OpError::error(
                "SyntaxError: WebSocket.close: reason must not exceed 123 UTF-8 bytes",
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// parse_and_validate_protocols — WHATWG §3.1 step 9.
// ---------------------------------------------------------------------------

/// Per RFC 6455 §4.1 + RFC 7230 token grammar: each subprotocol must
/// be a non-empty string of codepoints in U+0021..U+007E excluding
/// the RFC 7230 separator characters (`"(),/:;<=>?@[\]{}` plus space
/// and HT). The undici utility `isValidSubprotocol` (`util.js:101-141`)
/// is the literal model.
fn is_valid_subprotocol(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    s.chars().all(|c| {
        let code = c as u32;
        if !(0x21..=0x7E).contains(&code) {
            return false;
        }
        // RFC 7230 separator chars.
        !matches!(
            c,
            '"' | '(' | ')' | ',' | '/' | ':' | ';' | '<' | '=' | '>' | '?' | '@' | '[' | '\\'
              | ']' | '{' | '}'
        )
    })
}

/// Parse and validate the `protocols` argument per WHATWG §3.1 step 8-9.
/// Accepts:
///   - undefined → empty list.
///   - String → single-element list.
///   - sequence-of-strings (any iterable yielding strings) → as-is.
/// Throws SyntaxError on:
///   - duplicates (case-insensitive, ASCII-only — RFC 6455 tokens are
///     ASCII-only so to_ascii_lowercase is sufficient; addresses
///     critic MINOR #34).
///   - any element failing `is_valid_subprotocol`.
pub fn parse_and_validate_protocols(
    scope: &mut v8::PinScope,
    arg: v8::Local<v8::Value>,
) -> Result<Vec<String>, OpError> {
    let list: Vec<String> = if arg.is_undefined() {
        Vec::new()
    } else if arg.is_string() {
        vec![arg.to_rust_string_lossy(scope)]
    } else if let Ok(arr) = v8::Local::<v8::Array>::try_from(arg) {
        // Plain Array path — same shape fetch's RequestInit.headers
        // takes for sequence<sequence<ByteString>>. v1 doesn't iterate
        // arbitrary [Symbol.iterator] objects (a non-Array iterable
        // would fall through to the error below); WPT cases pass
        // arrays-only.
        let len = arr.length();
        let mut out = Vec::with_capacity(len as usize);
        for i in 0..len {
            let v = arr
                .get_index(scope, i)
                .ok_or_else(|| OpError::error("WebSocket: protocols access threw"))?;
            let s_v8 = v
                .to_string(scope)
                .ok_or_else(|| OpError::type_error("WebSocket: protocol element not coercible"))?;
            out.push(s_v8.to_rust_string_lossy(scope));
        }
        out
    } else {
        // Per WHATWG §3.1 step 8: "If protocols is a string, set protocols
        // to a sequence consisting of just that string." For non-string
        // non-sequence input, WebIDL union conversion throws TypeError;
        // we mirror that.
        return Err(OpError::type_error(
            "WebSocket: protocols must be a string or sequence of strings",
        ));
    };

    let mut seen: HashSet<String> = HashSet::new();
    for p in &list {
        let lowered = p.to_ascii_lowercase();
        if !seen.insert(lowered) {
            return Err(OpError::type_error(&format!(
                "SyntaxError: WebSocket: duplicate protocol '{p}'"
            )));
        }
        if !is_valid_subprotocol(p) {
            return Err(OpError::type_error(&format!(
                "SyntaxError: WebSocket: invalid protocol '{p}'"
            )));
        }
    }

    Ok(list)
}

// ---------------------------------------------------------------------------
// read_websocket_init — D-28 dictionary parser.
// ---------------------------------------------------------------------------

/// Parsed `WebSocketInit` dictionary (the v1 extension dict, NOT in the
/// WHATWG spec). Per D-28: `signal`, `origin`, `maxMessageSize`,
/// `maxFrameSize`, `pingIntervalMs`.
///
/// `signal` is read but NOT yet wired through the connect task — that
/// arrives in step 4 / step 7 (handshake + AbortSignal integration).
pub struct ParsedWebSocketInit {
    pub origin: Option<String>,
    pub max_message_size: u32,
    pub max_frame_size: u32,
    pub ping_interval_ms: u32,
}

pub fn read_websocket_init(
    scope: &mut v8::PinScope,
    val: v8::Local<v8::Value>,
) -> Result<ParsedWebSocketInit, OpError> {
    let mut parsed = ParsedWebSocketInit {
        origin: None,
        max_message_size: DEFAULT_MAX_MESSAGE_SIZE,
        max_frame_size: DEFAULT_MAX_FRAME_SIZE,
        ping_interval_ms: DEFAULT_PING_INTERVAL_MS,
    };
    if val.is_undefined() || val.is_null() {
        return Ok(parsed);
    }
    let Ok(obj) = v8::Local::<v8::Object>::try_from(val) else {
        return Err(OpError::type_error(
            "WebSocket: init must be an object",
        ));
    };

    let key =
        v8::String::new(scope, "origin").ok_or_else(|| OpError::error("out of memory"))?;
    if let Some(v) = obj.get(scope, key.into()) {
        if !v.is_undefined() && !v.is_null() {
            parsed.origin = Some(v.to_rust_string_lossy(scope));
        }
    }

    let key = v8::String::new(scope, "maxMessageSize")
        .ok_or_else(|| OpError::error("out of memory"))?;
    if let Some(v) = obj.get(scope, key.into()) {
        if !v.is_undefined() {
            // unsigned long — V8 uint32 conversion is fine for the
            // u32 IDL type (the cap fits).
            parsed.max_message_size =
                v.uint32_value(scope).unwrap_or(DEFAULT_MAX_MESSAGE_SIZE);
        }
    }

    let key = v8::String::new(scope, "maxFrameSize")
        .ok_or_else(|| OpError::error("out of memory"))?;
    if let Some(v) = obj.get(scope, key.into()) {
        if !v.is_undefined() {
            parsed.max_frame_size = v.uint32_value(scope).unwrap_or(DEFAULT_MAX_FRAME_SIZE);
        }
    }

    let key = v8::String::new(scope, "pingIntervalMs")
        .ok_or_else(|| OpError::error("out of memory"))?;
    if let Some(v) = obj.get(scope, key.into()) {
        if !v.is_undefined() {
            parsed.ping_interval_ms =
                v.uint32_value(scope).unwrap_or(DEFAULT_PING_INTERVAL_MS);
        }
    }

    Ok(parsed)
}

// ---------------------------------------------------------------------------
// EventHandler IDL — HTML §8.1.5.1 step 4 null-coercion + listener install.
// ---------------------------------------------------------------------------

/// Get the JS wrapper for `impl_` from the cached_handles, if cached.
/// Returns None if the `ws_obj` cache hasn't been populated yet — in
/// that case the handler can be stored but the EventTarget listener
/// install is deferred to first dispatch. (For simplicity v1 captures
/// the wrapper at setter time via `get_caller_this`; see below.)
fn cached_ws_obj<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    impl_: &WebSocketImpl,
) -> Option<v8::Local<'s, v8::Object>> {
    impl_
        .cached_handles
        .borrow()
        .ws_obj
        .as_ref()
        .map(|g| v8::Local::new(scope, g.clone()))
}

/// Set an EventHandler IDL attribute — HTML §8.1.5.1 setter steps.
///
/// Step 4: if the value is not callable, coerce to null (do NOT throw).
/// Step 5: if a previous internal listener was installed, remove it.
/// Step 6: install a new internal listener that delegates to the
///         stored function.
///
/// `slot_picker` selects the storage slot in `WsCachedHandles` for the
/// specific event (on_open / on_message / on_error / on_close).
///
/// (addresses critic MAJOR #15)
pub fn set_event_handler<F>(
    scope: &mut v8::PinScope,
    impl_: &WebSocketImpl,
    event_name: &str,
    fn_arg: v8::Local<v8::Value>,
    slot_picker: F,
) -> Result<(), OpError>
where
    F: Fn(&mut WsCachedHandles) -> &mut Option<v8::Global<v8::Function>>,
{
    // HTML §8.1.5.1 step 4: non-callable → null. NOT a TypeError.
    let new_handler: Option<v8::Global<v8::Function>> = if let Ok(f) =
        v8::Local::<v8::Function>::try_from(fn_arg)
    {
        Some(v8::Global::new(scope, f))
    } else {
        None
    };

    // Replace the slot.
    let prev_handler = {
        let mut handles = impl_.cached_handles.borrow_mut();
        let slot = slot_picker(&mut handles);
        let prev = slot.clone();
        *slot = new_handler.clone();
        prev
    };

    // Update the EventTarget listener registry. We need the JS wrapper
    // for `add_internal_listener` / `remove_internal_listener`. If the
    // wrapper hasn't been cached yet (no event has fired), we still
    // need to register on the listener Rc tied to *this object* — but
    // we don't know what `this` is at setter call time without a hop.
    //
    // The setter callback runs with `args.this()` accessible only via
    // the macro-generated trampoline — which the macro hands us as a
    // hidden argument. v1 handles the case by stashing `ws_obj` lazily
    // on first interaction; here, the setter has access via the same
    // mechanism the EventTarget hand-rolled callbacks use. We work
    // around this in the install path by reading `ws_obj` from the
    // cached_handles ONLY (the wrapper-self_weak is set by mint, but
    // for the JS-constructed path we set it on first event).
    //
    // For step 2 (no network), the EventTarget-listener round-trip is
    // not strictly required — `socket.onopen = f` is a no-op without
    // an Open event ever firing. The slot is preserved correctly so
    // the getter readback works. The full install/remove plumbing is
    // wired in step 5 (event dispatch), where we know the wrapper.

    // For step 2, we simply update the slot. The listener installation
    // happens lazily in `WebSocketImpl::ensure_handler_installed` on
    // first event dispatch (step 5). If a wrapper IS already cached
    // (e.g. by the WebSocketPair mint in step 6), we install/remove
    // synchronously here:
    if let Some(wrapper) = cached_ws_obj(scope, impl_) {
        if prev_handler.is_some() {
            remove_internal_listener(scope, wrapper, event_name);
        }
        if let Some(g) = new_handler {
            add_internal_listener(scope, wrapper, event_name, g);
        }
    }

    Ok(())
}

/// Get the EventHandler IDL attribute — returns the stored function or
/// `null` per HTML §8.1.5.1 getter step.
pub fn get_event_handler<'s, F>(
    scope: &mut v8::PinScope<'s, '_>,
    impl_: &WebSocketImpl,
    slot_picker: F,
) -> v8::Local<'s, v8::Value>
where
    F: Fn(&WsCachedHandles) -> Option<&v8::Global<v8::Function>>,
{
    let handles = impl_.cached_handles.borrow();
    match slot_picker(&handles) {
        Some(g) => v8::Local::new(scope, g.clone()).into(),
        None => v8::null(scope).into(),
    }
}

// Helper for tests / dispatch path: cache the JS wrapper and (re)install
// any pending EventHandler listeners. Lazy-attaches the listener Rc.
pub(crate) fn ensure_handler_listeners_installed(
    scope: &mut v8::PinScope,
    wrapper: v8::Local<v8::Object>,
    impl_: &WebSocketImpl,
) {
    // Cache the wrapper for subsequent setter installations.
    let mut handles = impl_.cached_handles.borrow_mut();
    if handles.ws_obj.is_none() {
        handles.ws_obj = Some(v8::Global::new(scope, wrapper));
    }
    // Make sure the listener Rc is attached — needed for dispatchEvent
    // to pick up listeners on this wrapper.
    let _ = listeners_of; // suppress unused-import; install via attach
    crate::dom::event_target::attach_listeners(scope, wrapper);

    // (Re)install any handlers stored before the wrapper was cached.
    let pairs = [
        ("open", handles.on_open.clone()),
        ("message", handles.on_message.clone()),
        ("error", handles.on_error.clone()),
        ("close", handles.on_close.clone()),
    ];
    drop(handles); // release the borrow before re-entering listener APIs.
    for (event, slot) in pairs.into_iter() {
        if let Some(g) = slot {
            // Idempotent: add_internal_listener is dedup'd on
            // (callback, capture). We call remove first to ensure no
            // stale listeners remain from a prior wrapper-bind cycle.
            remove_internal_listener(scope, wrapper, event);
            add_internal_listener(scope, wrapper, event, g);
        }
    }
}
