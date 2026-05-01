//! Hand-written tests for the native `EventTarget` and `Event` classes.
//!
//! Pattern matches `headers.rs` / `wpt_headers.rs`: install the
//! globals on a fresh isolate, evaluate JS, assert via stringified
//! result. WPT runs separately in `wpt_event_target.rs`.

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
fn event_target_construct_and_identity() {
    let s = run_in_v8(
        r#"
        const t = new EventTarget();
        JSON.stringify({
            kind: t.constructor.name,
            tag: Object.prototype.toString.call(t),
            isET: t instanceof EventTarget,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(
        s,
        r#"{"kind":"EventTarget","tag":"[object EventTarget]","isET":true}"#
    );
}

#[test]
fn event_construct_basic() {
    let s = run_in_v8(
        r#"
        const e = new Event("test");
        JSON.stringify({
            type: e.type,
            bubbles: e.bubbles,
            cancelable: e.cancelable,
            composed: e.composed,
            target: e.target,
            currentTarget: e.currentTarget,
            defaultPrevented: e.defaultPrevented,
            isTrusted: e.isTrusted,
            phase: e.eventPhase,
            none: Event.NONE,
            atTarget: Event.AT_TARGET,
            cp: Event.CAPTURING_PHASE,
            bp: Event.BUBBLING_PHASE,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    let parsed: serde_json::Value = serde_json::from_str(&s).expect("json");
    assert_eq!(parsed["type"], "test");
    assert_eq!(parsed["bubbles"], false);
    assert_eq!(parsed["cancelable"], false);
    assert_eq!(parsed["composed"], false);
    assert_eq!(parsed["target"], serde_json::Value::Null);
    assert_eq!(parsed["currentTarget"], serde_json::Value::Null);
    assert_eq!(parsed["defaultPrevented"], false);
    assert_eq!(parsed["isTrusted"], false);
    assert_eq!(parsed["phase"], 0);
    assert_eq!(parsed["none"], 0);
    assert_eq!(parsed["atTarget"], 2);
    assert_eq!(parsed["cp"], 1);
    assert_eq!(parsed["bp"], 3);
}

#[test]
fn event_construct_with_init() {
    let s = run_in_v8(
        r#"
        const e = new Event("click", { bubbles: true, cancelable: true, composed: true });
        JSON.stringify({
            t: e.type, b: e.bubbles, c: e.cancelable, comp: e.composed,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"t":"click","b":true,"c":true,"comp":true}"#);
}

// ---------------------------------------------------------------------------
// addEventListener / dispatchEvent / removeEventListener
// ---------------------------------------------------------------------------

#[test]
fn add_dispatch_remove_round_trip() {
    let s = run_in_v8(
        r#"
        const t = new EventTarget();
        let calls = [];
        const listener = (ev) => calls.push(ev.type);
        t.addEventListener("ping", listener);

        const e1 = new Event("ping");
        const r1 = t.dispatchEvent(e1);

        t.dispatchEvent(new Event("ping"));

        t.removeEventListener("ping", listener);
        t.dispatchEvent(new Event("ping"));

        JSON.stringify({ calls, r1 });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"calls":["ping","ping"],"r1":true}"#);
}

#[test]
fn dispatch_sets_target_and_current_target() {
    let s = run_in_v8(
        r#"
        const t = new EventTarget();
        let target_match, current_match;
        t.addEventListener("x", function(ev) {
            target_match = ev.target === t;
            current_match = ev.currentTarget === t;
        });
        t.dispatchEvent(new Event("x"));
        JSON.stringify({ target_match, current_match });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"target_match":true,"current_match":true}"#);
}

#[test]
fn dispatch_phase_is_at_target() {
    let s = run_in_v8(
        r#"
        const t = new EventTarget();
        let phase_during, phase_after;
        const ev = new Event("y");
        t.addEventListener("y", () => { phase_during = ev.eventPhase; });
        t.dispatchEvent(ev);
        phase_after = ev.eventPhase;
        JSON.stringify({ during: phase_during, after: phase_after });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"during":2,"after":0}"#);
}

#[test]
fn dispatch_returns_false_when_prevented() {
    let s = run_in_v8(
        r#"
        const t = new EventTarget();
        t.addEventListener("y", (ev) => ev.preventDefault());
        const cancelable = new Event("y", { cancelable: true });
        const r1 = t.dispatchEvent(cancelable);

        // Non-cancelable: preventDefault is a no-op, dispatch returns true.
        t.addEventListener("z", (ev) => ev.preventDefault());
        const noncancel = new Event("z");
        const r2 = t.dispatchEvent(noncancel);

        JSON.stringify({
            r1, r2,
            cancelable_dp: cancelable.defaultPrevented,
            noncancel_dp: noncancel.defaultPrevented,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(
        s,
        r#"{"r1":false,"r2":true,"cancelable_dp":true,"noncancel_dp":false}"#
    );
}

// ---------------------------------------------------------------------------
// once / signal removal
// ---------------------------------------------------------------------------

#[test]
fn once_listener_removes_after_first_fire() {
    let s = run_in_v8(
        r#"
        const t = new EventTarget();
        let calls = 0;
        t.addEventListener("e", () => { calls++; }, { once: true });
        t.dispatchEvent(new Event("e"));
        t.dispatchEvent(new Event("e"));
        t.dispatchEvent(new Event("e"));
        JSON.stringify({ calls });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"calls":1}"#);
}

#[test]
fn signal_option_removes_listener_on_abort() {
    let s = run_in_v8(
        r#"
        const t = new EventTarget();
        const c = new AbortController();
        let calls = 0;
        t.addEventListener("e", () => { calls++; }, { signal: c.signal });
        t.dispatchEvent(new Event("e"));     // 1
        c.abort();
        t.dispatchEvent(new Event("e"));     // skipped
        JSON.stringify({ calls });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"calls":1}"#);
}

#[test]
fn aborted_signal_skips_initial_registration() {
    let s = run_in_v8(
        r#"
        const t = new EventTarget();
        const c = new AbortController();
        c.abort();   // already aborted before addEventListener
        let calls = 0;
        t.addEventListener("e", () => { calls++; }, { signal: c.signal });
        t.dispatchEvent(new Event("e"));
        JSON.stringify({ calls });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"calls":0}"#);
}

// ---------------------------------------------------------------------------
// Listener dedup + propagation
// ---------------------------------------------------------------------------

#[test]
fn duplicate_add_is_dedup_by_callback_and_capture() {
    let s = run_in_v8(
        r#"
        const t = new EventTarget();
        let calls = 0;
        const fn = () => { calls++; };
        t.addEventListener("e", fn);
        t.addEventListener("e", fn);   // dedup'd
        t.addEventListener("e", fn, { capture: true });  // different capture → new entry
        t.dispatchEvent(new Event("e"));
        JSON.stringify({ calls });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"calls":2}"#);
}

#[test]
fn stop_immediate_propagation_halts_remaining_listeners() {
    let s = run_in_v8(
        r#"
        const t = new EventTarget();
        const order = [];
        t.addEventListener("e", (ev) => { order.push("a"); ev.stopImmediatePropagation(); });
        t.addEventListener("e", () => { order.push("b"); });
        t.dispatchEvent(new Event("e"));
        JSON.stringify({ order });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"order":["a"]}"#);
}

#[test]
fn passive_listener_blocks_prevent_default() {
    let s = run_in_v8(
        r#"
        const t = new EventTarget();
        t.addEventListener("e", (ev) => ev.preventDefault(), { passive: true });
        const ev = new Event("e", { cancelable: true });
        const r = t.dispatchEvent(ev);
        JSON.stringify({ r, dp: ev.defaultPrevented });
        "#,
        |val, scope| js_string(val, scope),
    );
    // r = !defaultPrevented = true, dp = false (passive blocked it)
    assert_eq!(s, r#"{"r":true,"dp":false}"#);
}

#[test]
fn listener_throwing_does_not_crash_dispatch() {
    let s = run_in_v8(
        r#"
        const t = new EventTarget();
        let later = 0;
        t.addEventListener("e", () => { throw new Error("boom"); });
        t.addEventListener("e", () => { later++; });
        t.dispatchEvent(new Event("e"));
        JSON.stringify({ later });
        "#,
        |val, scope| js_string(val, scope),
    );
    // The first listener throws but the dispatcher swallows the
    // exception and continues with the second listener.
    assert_eq!(s, r#"{"later":1}"#);
}

#[test]
fn redispatch_during_dispatch_throws() {
    let s = run_in_v8(
        r#"
        const t = new EventTarget();
        let kind, msg;
        const ev = new Event("e");
        t.addEventListener("e", () => {
            try { t.dispatchEvent(ev); }
            catch (err) { kind = err.constructor.name; msg = err.message; }
        });
        t.dispatchEvent(ev);
        JSON.stringify({ kind, msg });
        "#,
        |val, scope| js_string(val, scope),
    );
    let parsed: serde_json::Value = serde_json::from_str(&s).expect("json");
    // kind is "Error" because we don't have native DOMException yet;
    // the polyfill upgrades to InvalidStateError. Either way, throw.
    let kind = parsed["kind"].as_str().unwrap();
    assert!(kind == "Error" || kind == "DOMException", "got kind: {kind}");
    assert!(
        parsed["msg"].as_str().unwrap().contains("dispatched"),
        "msg should mention dispatch: {parsed}"
    );
}

// ---------------------------------------------------------------------------
// Mid-dispatch listener mutations
// ---------------------------------------------------------------------------

#[test]
fn listeners_added_mid_dispatch_are_not_invoked() {
    let s = run_in_v8(
        r#"
        const t = new EventTarget();
        let calls = 0;
        const inner = () => { calls++; };
        t.addEventListener("e", () => {
            // Add inner mid-dispatch — DOM §2.7 says this listener is
            // NOT in the snapshot and won't fire for THIS dispatch.
            t.addEventListener("e", inner);
        });
        t.dispatchEvent(new Event("e"));
        const after_first = calls;
        // Second dispatch: inner IS in the snapshot now and fires.
        t.dispatchEvent(new Event("e"));
        JSON.stringify({ after_first, after_second: calls });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"after_first":0,"after_second":1}"#);
}

#[test]
fn listeners_removed_mid_dispatch_are_skipped() {
    let s = run_in_v8(
        r#"
        const t = new EventTarget();
        const order = [];
        const a = () => order.push("a");
        const b = () => { order.push("b"); t.removeEventListener("e", c); };
        const c = () => order.push("c");
        t.addEventListener("e", a);
        t.addEventListener("e", b);
        t.addEventListener("e", c);
        t.dispatchEvent(new Event("e"));
        JSON.stringify({ order });
        "#,
        |val, scope| js_string(val, scope),
    );
    // a fires, b fires (and removes c), c is skipped.
    assert_eq!(s, r#"{"order":["a","b"]}"#);
}

// ---------------------------------------------------------------------------
// Event init via initEvent (legacy)
// ---------------------------------------------------------------------------

#[test]
fn init_event_resets_type_and_flags() {
    let s = run_in_v8(
        r#"
        const e = new Event("a", { bubbles: true, cancelable: true });
        e.initEvent("b", false, false);
        JSON.stringify({
            t: e.type, b: e.bubbles, c: e.cancelable,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"t":"b","b":false,"c":false}"#);
}

// ---------------------------------------------------------------------------
// composedPath in non-DOM context returns [target] post-dispatch
// ---------------------------------------------------------------------------

#[test]
fn composed_path_returns_target_after_dispatch() {
    let s = run_in_v8(
        r#"
        const t = new EventTarget();
        const ev = new Event("e");
        const before = ev.composedPath();
        t.addEventListener("e", () => {});
        t.dispatchEvent(ev);
        const after = ev.composedPath();
        JSON.stringify({
            before_len: before.length,
            after_len: after.length,
            after_is_target: after[0] === t,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(
        s,
        r#"{"before_len":0,"after_len":1,"after_is_target":true}"#
    );
}
