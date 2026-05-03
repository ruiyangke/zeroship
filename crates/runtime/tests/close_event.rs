//! Hand-written tests for the native `CloseEvent` class.
//!
//! Covers:
//!   - Construction + dictionary parsing
//!   - code conversion: ConvertToInt default case (modulo, NOT [Clamp])
//!     — addresses critic MAJOR #28.
//!   - wasClean / reason defaults
//!   - instanceof CloseEvent && instanceof Event

#![allow(unsafe_code)]

use zeroship_runtime::dom;
use zeroship_runtime::init_v8;

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

    let src_v8 = v8::String::new(scope, src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    let result = script.run(scope).unwrap();
    f(result, scope)
}

fn js_string(val: v8::Local<v8::Value>, scope: &mut v8::PinScope) -> String {
    val.to_rust_string_lossy(scope)
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

#[test]
fn close_event_construct_basic() {
    let s = run_in_v8(
        r#"
        const e = new CloseEvent("close");
        JSON.stringify({
            type: e.type,
            wasClean: e.wasClean,
            code: e.code,
            reason: e.reason,
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"type":"close","wasClean":false,"code":0,"reason":""}"#
    );
}

#[test]
fn close_event_construct_full_init() {
    let s = run_in_v8(
        r#"
        const e = new CloseEvent("close", {
            wasClean: true,
            code: 1000,
            reason: "Goodbye",
            bubbles: true,
        });
        JSON.stringify({
            wasClean: e.wasClean,
            code: e.code,
            reason: e.reason,
            bubbles: e.bubbles,
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"wasClean":true,"code":1000,"reason":"Goodbye","bubbles":true}"#
    );
}

#[test]
fn close_event_missing_type_throws() {
    let s = run_in_v8(
        r#"
        try {
            new CloseEvent();
            "no throw";
        } catch (e) {
            e instanceof TypeError ? "TypeError" : "OtherError";
        }
        "#,
        js_string,
    );
    assert_eq!(s, "TypeError");
}

// ---------------------------------------------------------------------------
// `code` conversion — ConvertToInt default case (NOT [Clamp]).
// Per WebIDL: NaN/inf → 0; truncate toward zero; modulo 2^16.
// (addresses critic MAJOR #28)
// ---------------------------------------------------------------------------

#[test]
fn close_event_code_truncates_toward_zero() {
    // 1.5 → trunc → 1 (not 2; not "round half").
    let s = run_in_v8(
        r#"
        new CloseEvent("close", { code: 1.5 }).code;
        "#,
        js_string,
    );
    assert_eq!(s, "1");
}

#[test]
fn close_event_code_negative_wraps_modulo() {
    // -1 → trunc → -1; mod 2^16 → 65535. NOT clamped to 0.
    let s = run_in_v8(
        r#"
        new CloseEvent("close", { code: -1 }).code;
        "#,
        js_string,
    );
    assert_eq!(s, "65535");
}

#[test]
fn close_event_code_nan_is_zero() {
    let s = run_in_v8(
        r#"
        new CloseEvent("close", { code: NaN }).code;
        "#,
        js_string,
    );
    assert_eq!(s, "0");
}

#[test]
fn close_event_code_infinity_is_zero() {
    let s = run_in_v8(
        r#"
        JSON.stringify({
            posInf: new CloseEvent("close", { code: Infinity }).code,
            negInf: new CloseEvent("close", { code: -Infinity }).code,
        });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"posInf":0,"negInf":0}"#);
}

#[test]
fn close_event_code_overflow_modulo() {
    // 65537 → mod 2^16 → 1.
    let s = run_in_v8(
        r#"
        new CloseEvent("close", { code: 65537 }).code;
        "#,
        js_string,
    );
    assert_eq!(s, "1");
}

#[test]
fn close_event_code_undefined_default_zero() {
    let s = run_in_v8(
        r#"
        JSON.stringify({
            noDict: new CloseEvent("close").code,
            emptyDict: new CloseEvent("close", {}).code,
            undef: new CloseEvent("close", { code: undefined }).code,
        });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"noDict":0,"emptyDict":0,"undef":0}"#);
}

// ---------------------------------------------------------------------------
// instanceof checks
// ---------------------------------------------------------------------------

#[test]
fn close_event_instanceof_event() {
    let s = run_in_v8(
        r#"
        const e = new CloseEvent("close");
        JSON.stringify({
            isCloseEvent: e instanceof CloseEvent,
            isEvent: e instanceof Event,
        });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"isCloseEvent":true,"isEvent":true}"#);
}

// ---------------------------------------------------------------------------
// Symbol.toStringTag
// ---------------------------------------------------------------------------

#[test]
fn close_event_to_string_tag() {
    let s = run_in_v8(
        r#"
        Object.prototype.toString.call(new CloseEvent("close"));
        "#,
        js_string,
    );
    assert_eq!(s, "[object CloseEvent]");
}

// ---------------------------------------------------------------------------
// Inherited Event surface
// ---------------------------------------------------------------------------

#[test]
fn close_event_inherited_event_methods() {
    let s = run_in_v8(
        r#"
        const e = new CloseEvent("close", { cancelable: true });
        e.preventDefault();
        JSON.stringify({
            type: e.type,
            isTrusted: e.isTrusted,
            defaultPrevented: e.defaultPrevented,
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"type":"close","isTrusted":false,"defaultPrevented":true}"#
    );
}

// ---------------------------------------------------------------------------
// Dispatch via EventTarget
// ---------------------------------------------------------------------------

#[test]
fn close_event_dispatch_through_event_target() {
    let s = run_in_v8(
        r#"
        const t = new EventTarget();
        let saw = null;
        t.addEventListener("close", (e) => {
            saw = {
                isCloseEvent: e instanceof CloseEvent,
                code: e.code,
                reason: e.reason,
                wasClean: e.wasClean,
            };
        });
        const e = new CloseEvent("close", { code: 1000, reason: "ok", wasClean: true });
        t.dispatchEvent(e);
        JSON.stringify(saw);
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"isCloseEvent":true,"code":1000,"reason":"ok","wasClean":true}"#
    );
}
