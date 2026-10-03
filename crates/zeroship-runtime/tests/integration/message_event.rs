//! Hand-written tests for the native `MessageEvent` class.
//!
//! Same pattern as `custom_event.rs`. Covers:
//!   - Construction + dictionary parsing
//!   - data identity (object readback-stability)
//!   - origin / lastEventId defaults
//!   - source always null
//!   - ports identity (FrozenArray cache)
//!   - instanceof MessageEvent && instanceof Event
//!   - initMessageEvent legacy method

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
fn message_event_construct_basic() {
    let s = run_in_v8(
        r#"
        const e = new MessageEvent("message");
        JSON.stringify({
            type: e.type,
            data: e.data,
            origin: e.origin,
            lastEventId: e.lastEventId,
            source: e.source,
            bubbles: e.bubbles,
            cancelable: e.cancelable,
            composed: e.composed,
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"type":"message","data":null,"origin":"","lastEventId":"","source":null,"bubbles":false,"cancelable":false,"composed":false}"#
    );
}

#[test]
fn message_event_construct_full_init() {
    let s = run_in_v8(
        r#"
        const e = new MessageEvent("message", {
            data: "hello",
            origin: "wss://example.com",
            lastEventId: "1",
            bubbles: true,
            cancelable: true,
            composed: true,
        });
        JSON.stringify({
            data: e.data,
            origin: e.origin,
            lastEventId: e.lastEventId,
            bubbles: e.bubbles,
            cancelable: e.cancelable,
            composed: e.composed,
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"data":"hello","origin":"wss://example.com","lastEventId":"1","bubbles":true,"cancelable":true,"composed":true}"#
    );
}

#[test]
fn message_event_missing_type_throws() {
    let s = run_in_v8(
        r#"
        try {
            new MessageEvent();
            "no throw";
        } catch (e) {
            e instanceof TypeError ? "TypeError" : "OtherError";
        }
        "#,
        js_string,
    );
    assert_eq!(s, "TypeError");
}

#[test]
fn message_event_data_object_identity() {
    // Per WebIDL: `data` is `any` and reads-back the SAME JS value
    // (object identity) every time.
    let s = run_in_v8(
        r#"
        const obj = { x: 1 };
        const e = new MessageEvent("message", { data: obj });
        // Mutate via the original ref; e.data must observe the change.
        obj.x = 42;
        // Identity check + readback check.
        const sameRef = e.data === obj;
        const x = e.data.x;
        JSON.stringify({ sameRef, x });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"sameRef":true,"x":42}"#);
}

#[test]
fn message_event_data_undefined_is_null() {
    // Per WebIDL dictionary defaulting: `data: undefined` → null (the
    // member's default is null).
    let s = run_in_v8(
        r#"
        const e = new MessageEvent("message", { data: undefined });
        JSON.stringify({ data: e.data, isNull: e.data === null });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"data":null,"isNull":true}"#);
}

// ---------------------------------------------------------------------------
// instanceof checks
// ---------------------------------------------------------------------------

#[test]
fn message_event_instanceof_event() {
    // Per HTML §9.4.2: `MessageEvent : Event` — instanceof Event MUST
    // be true. The polyfill's expando-based "plain Event with .data"
    // failed this for `instanceof MessageEvent`; the native class
    // passes both checks.
    let s = run_in_v8(
        r#"
        const e = new MessageEvent("message");
        JSON.stringify({
            isMessageEvent: e instanceof MessageEvent,
            isEvent: e instanceof Event,
        });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"isMessageEvent":true,"isEvent":true}"#);
}

// ---------------------------------------------------------------------------
// ports identity
// ---------------------------------------------------------------------------

#[test]
fn message_event_ports_returns_same_frozen_array() {
    // Per WebIDL §3.2.34 FrozenArray: every getter call returns the
    // SAME instance.
    let s = run_in_v8(
        r#"
        const e = new MessageEvent("message");
        const a = e.ports;
        const b = e.ports;
        JSON.stringify({
            sameRef: a === b,
            length: a.length,
            isFrozen: Object.isFrozen(a),
            isArray: Array.isArray(a),
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"sameRef":true,"length":0,"isFrozen":true,"isArray":true}"#
    );
}

// ---------------------------------------------------------------------------
// initMessageEvent legacy
// ---------------------------------------------------------------------------

#[test]
fn message_event_init_message_event_resets_fields() {
    let s = run_in_v8(
        r#"
        const e = new MessageEvent("message");
        e.initMessageEvent("custom", true, false, "data!", "https://o", "lid", null, []);
        JSON.stringify({
            type: e.type,
            bubbles: e.bubbles,
            cancelable: e.cancelable,
            data: e.data,
            origin: e.origin,
            lastEventId: e.lastEventId,
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"type":"custom","bubbles":true,"cancelable":false,"data":"data!","origin":"https://o","lastEventId":"lid"}"#
    );
}

// ---------------------------------------------------------------------------
// Symbol.toStringTag
// ---------------------------------------------------------------------------

#[test]
fn message_event_to_string_tag() {
    // Per WebIDL: `Object.prototype.toString.call(messageEvent)` should
    // produce "[object MessageEvent]". The class macro install adds a
    // Symbol.toStringTag descriptor.
    let s = run_in_v8(
        r#"
        const e = new MessageEvent("message");
        Object.prototype.toString.call(e);
        "#,
        js_string,
    );
    assert_eq!(s, "[object MessageEvent]");
}

// ---------------------------------------------------------------------------
// Inherited Event surface
// ---------------------------------------------------------------------------

#[test]
fn message_event_inherited_event_methods() {
    // MessageEvent inherits stopPropagation / preventDefault / etc. from
    // Event. The cancelable=true variant is necessary for
    // preventDefault to actually flip defaultPrevented.
    let s = run_in_v8(
        r#"
        const e = new MessageEvent("message", { cancelable: true });
        e.stopPropagation();
        e.preventDefault();
        JSON.stringify({
            isTrusted: e.isTrusted,
            defaultPrevented: e.defaultPrevented,
            eventPhase: e.eventPhase,
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"isTrusted":false,"defaultPrevented":true,"eventPhase":0}"#
    );
}

// ---------------------------------------------------------------------------
// Dispatch via EventTarget
// ---------------------------------------------------------------------------

#[test]
fn message_event_dispatch_through_event_target() {
    // A native MessageEvent must successfully `dispatchEvent` on an
    // EventTarget — its #[repr(C)] layout means the dispatch path's
    // `event_from_obj` cast picks up the right Event base.
    let s = run_in_v8(
        r#"
        const t = new EventTarget();
        let saw = null;
        t.addEventListener("message", (e) => {
            saw = {
                isMessageEvent: e instanceof MessageEvent,
                data: e.data,
                origin: e.origin,
            };
        });
        const e = new MessageEvent("message", { data: 42, origin: "x" });
        t.dispatchEvent(e);
        JSON.stringify(saw);
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"isMessageEvent":true,"data":42,"origin":"x"}"#);
}
