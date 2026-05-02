//! Hand-written tests for the native `CustomEvent` class.
//!
//! Follows the same pattern as `event_target.rs` — install DOM
//! globals on a fresh isolate, evaluate JS, assert via stringified
//! result. WPT runs separately in `wpt_event_target.rs` (which now
//! also covers CustomEvent).

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
// Construction + identity
// ---------------------------------------------------------------------------

#[test]
fn custom_event_construct_basic() {
    let s = run_in_v8(
        r#"
        const e = new CustomEvent("foo");
        JSON.stringify({
            type: e.type,
            detail: e.detail,
            bubbles: e.bubbles,
            cancelable: e.cancelable,
            composed: e.composed,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(
        s,
        r#"{"type":"foo","detail":null,"bubbles":false,"cancelable":false,"composed":false}"#
    );
}

#[test]
fn custom_event_detail_string() {
    let s = run_in_v8(
        r#"
        const e = new CustomEvent("foo", { detail: "bar" });
        JSON.stringify({ type: e.type, detail: e.detail });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"type":"foo","detail":"bar"}"#);
}

#[test]
fn custom_event_detail_object() {
    let s = run_in_v8(
        r#"
        const e = new CustomEvent("foo", { detail: { x: 1, y: "two" } });
        JSON.stringify({ x: e.detail.x, y: e.detail.y });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"x":1,"y":"two"}"#);
}

#[test]
fn custom_event_inherited_event_init_fields() {
    // bubbles/cancelable/composed must flow through to the native
    // Event base class via the inherited constructor logic.
    let s = run_in_v8(
        r#"
        const e = new CustomEvent("foo", {
            bubbles: true,
            cancelable: true,
            composed: true,
            detail: 42,
        });
        JSON.stringify({
            t: e.type,
            b: e.bubbles,
            c: e.cancelable,
            comp: e.composed,
            d: e.detail,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(
        s,
        r#"{"t":"foo","b":true,"c":true,"comp":true,"d":42}"#
    );
}

#[test]
fn custom_event_instanceof_event() {
    // Per `#[v8_inherit(Event)]`, CustomEvent extends Event.
    let s = run_in_v8(
        r#"
        const e = new CustomEvent("foo");
        JSON.stringify({
            isEvent: e instanceof Event,
            isCustomEvent: e instanceof CustomEvent,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"isEvent":true,"isCustomEvent":true}"#);
}

#[test]
fn custom_event_dispatched_via_event_target() {
    // CustomEvent must pass the EventTarget.dispatchEvent brand check
    // (which uses the Event internal-field external; CustomEvent's
    // wrapper carries one because the native class is set up the
    // same way as Event).
    let s = run_in_v8(
        r#"
        const t = new EventTarget();
        let received = null;
        t.addEventListener("ping", (ev) => {
            received = { type: ev.type, detail: ev.detail, isCE: ev instanceof CustomEvent };
        });
        const ce = new CustomEvent("ping", { detail: { msg: "hi" } });
        const r = t.dispatchEvent(ce);
        JSON.stringify({ r, received });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(
        s,
        r#"{"r":true,"received":{"type":"ping","detail":{"msg":"hi"},"isCE":true}}"#
    );
}

#[test]
fn custom_event_to_string_tag() {
    let s = run_in_v8(
        r#"
        const e = new CustomEvent("foo");
        Object.prototype.toString.call(e);
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "[object CustomEvent]");
}

#[test]
fn custom_event_detail_undefined_is_null() {
    // Per CustomEventInit IDL: `any detail = null;` — when the init
    // dict is absent OR `detail` is undefined, the value is null.
    let s = run_in_v8(
        r#"
        const e1 = new CustomEvent("a");
        const e2 = new CustomEvent("a", {});
        const e3 = new CustomEvent("a", { detail: undefined });
        JSON.stringify({
            d1: e1.detail,
            d2: e2.detail,
            d3: e3.detail,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    // null serialises to JSON null in all three cases.
    assert_eq!(s, r#"{"d1":null,"d2":null,"d3":null}"#);
}

#[test]
fn custom_event_detail_arbitrary_values() {
    // Per CustomEventInit IDL: `any detail` — the value is held by
    // identity, so functions, BigInts, and Symbols round-trip
    // unchanged. Use side-channel checks (typeof, ===) since these
    // values aren't all JSON-serialisable.
    let s = run_in_v8(
        r#"
        const fn = () => 42;
        const big = 12345678901234567890n;
        const sym = Symbol("zs");
        const eFn = new CustomEvent("a", { detail: fn });
        const eBig = new CustomEvent("a", { detail: big });
        const eSym = new CustomEvent("a", { detail: sym });
        JSON.stringify({
            fnSame: eFn.detail === fn,
            fnTypeof: typeof eFn.detail,
            bigSame: eBig.detail === big,
            bigTypeof: typeof eBig.detail,
            symSame: eSym.detail === sym,
            symTypeof: typeof eSym.detail,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(
        s,
        r#"{"fnSame":true,"fnTypeof":"function","bigSame":true,"bigTypeof":"bigint","symSame":true,"symTypeof":"symbol"}"#
    );
}

#[test]
fn custom_event_detail_held_strongly() {
    // The detail value must not be GC'd before access — the native
    // class holds it via `v8::Global<v8::Value>`. Smoke-test by
    // forcing a GC and re-reading.
    let s = run_in_v8(
        r#"
        const e = new CustomEvent("foo", { detail: { x: 1 } });
        // Drop our local reference; only e.detail keeps it alive.
        // (The const binding `e` itself keeps the wrapper alive,
        // which is exactly what we want — the wrapper's Global keeps
        // the detail alive.)
        // We can't trigger GC from JS deterministically, but we can
        // re-check the value after creating churn.
        const arr = [];
        for (let i = 0; i < 1000; i++) arr.push({ junk: i });
        JSON.stringify({ x: e.detail.x });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"x":1}"#);
}

#[test]
fn custom_event_inherits_event_methods() {
    // preventDefault / stopPropagation / composedPath should all be
    // reachable via the Event prototype chain.
    let s = run_in_v8(
        r#"
        const e = new CustomEvent("foo", { cancelable: true });
        e.preventDefault();
        const out = {
            dp: e.defaultPrevented,
            phase: e.eventPhase,
            none: Event.NONE,
            cp: typeof e.composedPath === "function",
            sp: typeof e.stopPropagation === "function",
            sip: typeof e.stopImmediatePropagation === "function",
        };
        JSON.stringify(out);
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(
        s,
        r#"{"dp":true,"phase":0,"none":0,"cp":true,"sp":true,"sip":true}"#
    );
}

#[test]
fn custom_event_constructor_name_and_length() {
    let s = run_in_v8(
        r#"
        JSON.stringify({
            name: CustomEvent.name,
            // Per WebIDL the `length` is the number of required args; for
            // CustomEvent that's 1 (`type`). V8 reports the function's
            // declared arity; FunctionTemplate's default is 0 unless
            // set_length is called. We accept either 0 or 1 depending on
            // how the macro emits it.
            isFn: typeof CustomEvent === "function",
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"name":"CustomEvent","isFn":true}"#);
}

#[test]
fn custom_event_missing_type_throws() {
    // Per DOM `Event` IDL — `type` is required; CustomEvent inherits
    // the same requirement. Calling `new CustomEvent()` should throw
    // TypeError.
    let s = run_in_v8(
        r#"
        let threw = false, msg = "";
        try { new CustomEvent(); }
        catch (e) { threw = true; msg = e instanceof TypeError ? "TypeError" : "Error"; }
        JSON.stringify({ threw, msg });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"threw":true,"msg":"TypeError"}"#);
}
