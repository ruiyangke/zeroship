//! Hand-written tests for WebSocket attribute behaviours: binaryType
//! setter (silent no-op on unknown), EventHandler IDL null-coercion,
//! attribute readback stability.
//!
//! Coverage:
//!   - binaryType: silent no-op on unknown values.
//!   - EventHandler IDL: non-callable assignment coerces to null per
//!     HTML §8.1.5.1 step 4 — must NOT throw.

#![cfg(feature = "runtime_native_websocket")]
#![allow(unsafe_code)]

use zeroship_runtime::dom;
use zeroship_runtime::init_v8;
use zeroship_runtime::websocket_native;

fn run_in_v8<F, R>(src: &str, f: F) -> R
where
    F: FnOnce(v8::Local<v8::Value>, &mut v8::PinScope) -> R,
{
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    let global = scope.get_current_context().global(scope);
    dom::install_globals(scope, global);
    websocket_native::install_global(scope, global);

    let src_v8 = v8::String::new(scope, src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    let result = script.run(scope).unwrap();
    f(result, scope)
}

fn js_string(val: v8::Local<v8::Value>, scope: &mut v8::PinScope) -> String {
    val.to_rust_string_lossy(scope)
}

// ---------------------------------------------------------------------------
// binaryType setter
// ---------------------------------------------------------------------------

#[test]
fn binary_type_default_is_blob() {
    // Spec default is "blob" (NOT "arraybuffer" — the polyfill
    // defaulted wrong).
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        ws.binaryType;
        "#,
        js_string,
    );
    assert_eq!(s, "blob");
}

#[test]
fn binary_type_set_blob_round_trip() {
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        ws.binaryType = "blob";
        ws.binaryType;
        "#,
        js_string,
    );
    assert_eq!(s, "blob");
}

#[test]
fn binary_type_set_arraybuffer_round_trip() {
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        ws.binaryType = "arraybuffer";
        ws.binaryType;
        "#,
        js_string,
    );
    assert_eq!(s, "arraybuffer");
}

#[test]
fn binary_type_set_unknown_value_silent_no_op() {
    // WPT `binaryType-wrong-value.any.js`: setter on an unknown value
    // is a silent no-op (current value retained); it must not throw.
    // Matches undici, workerd, and the existing polyfill.
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        ws.binaryType = "arraybuffer";
        ws.binaryType = "not-a-real-value";
        // Value retained from prior set:
        ws.binaryType;
        "#,
        js_string,
    );
    assert_eq!(s, "arraybuffer");
}

#[test]
fn binary_type_set_unknown_does_not_throw() {
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        try {
            ws.binaryType = "garbage";
            ws.binaryType = 42;
            ws.binaryType = null;
            "no throw";
        } catch (e) {
            "threw:" + e.message;
        }
        "#,
        js_string,
    );
    assert_eq!(s, "no throw");
}

// ---------------------------------------------------------------------------
// EventHandler IDL — HTML §8.1.5.1
// ---------------------------------------------------------------------------

#[test]
fn event_handler_setter_accepts_function() {
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        const fn = () => {};
        ws.onmessage = fn;
        // Identity readback per IDL — same function returned.
        ws.onmessage === fn ? "same" : "diff";
        "#,
        js_string,
    );
    assert_eq!(s, "same");
}

#[test]
fn event_handler_setter_null_coerces_non_callable_to_null() {
    // HTML §8.1.5.1 step 4: non-callable assignment → null. NOT a
    // TypeError. Must work for null, undefined, string, number,
    // object — all become null.
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        const observations = [];
        ws.onmessage = "not a function";
        observations.push(ws.onmessage);
        ws.onmessage = 42;
        observations.push(ws.onmessage);
        ws.onmessage = {};
        observations.push(ws.onmessage);
        ws.onmessage = null;
        observations.push(ws.onmessage);
        ws.onmessage = undefined;
        observations.push(ws.onmessage);
        // All observations should be null.
        JSON.stringify(observations.map(o => o === null));
        "#,
        js_string,
    );
    assert_eq!(s, r#"[true,true,true,true,true]"#);
}

#[test]
fn event_handler_setter_no_throw_on_non_callable() {
    // The null-coercion path MUST NOT throw — that's the whole point.
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        try {
            ws.onmessage = "not a function";
            ws.onerror = 42;
            ws.onclose = {};
            ws.onopen = null;
            "no throw";
        } catch (e) {
            "threw:" + e.message;
        }
        "#,
        js_string,
    );
    assert_eq!(s, "no throw");
}

#[test]
fn event_handler_setter_replaces_previous() {
    // Per HTML §8.1.5.1: setter replaces any prior internal listener.
    // The replacement is observable via the function readback (the
    // dispatch test belongs in step 5 once events fire).
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        const fn1 = () => "fn1";
        const fn2 = () => "fn2";
        ws.onmessage = fn1;
        ws.onmessage = fn2;
        ws.onmessage === fn2 ? "fn2" : "other";
        "#,
        js_string,
    );
    assert_eq!(s, "fn2");
}

#[test]
fn event_handler_getter_returns_null_when_unset() {
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        JSON.stringify({
            onopen: ws.onopen,
            onmessage: ws.onmessage,
            onerror: ws.onerror,
            onclose: ws.onclose,
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"onopen":null,"onmessage":null,"onerror":null,"onclose":null}"#
    );
}

#[test]
fn all_event_handlers_independent_slots() {
    // Setting onmessage doesn't affect onerror — slots are independent.
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        const f1 = () => 1;
        const f2 = () => 2;
        ws.onmessage = f1;
        ws.onerror = f2;
        JSON.stringify({
            onmessage: ws.onmessage === f1,
            onerror: ws.onerror === f2,
            onopen: ws.onopen,
            onclose: ws.onclose,
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"onmessage":true,"onerror":true,"onopen":null,"onclose":null}"#
    );
}

// ---------------------------------------------------------------------------
// addEventListener still works (EventTarget mixin)
// ---------------------------------------------------------------------------

#[test]
fn add_event_listener_works() {
    // The EventTarget mixin (via #[v8_inherit(EventTarget)]) gives
    // WebSocket addEventListener / removeEventListener / dispatchEvent.
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        let saw = "no";
        ws.addEventListener("custom-event-for-test", () => {
            saw = "yes";
        });
        ws.dispatchEvent(new Event("custom-event-for-test"));
        saw;
        "#,
        js_string,
    );
    assert_eq!(s, "yes");
}
