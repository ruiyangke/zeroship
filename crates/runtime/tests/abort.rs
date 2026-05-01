//! Hand-written tests for the native `AbortController` and
//! `AbortSignal` classes.
//!
//! Covers DOM §3.3 (signal abort algorithm, dependent signals,
//! AbortSignal.{abort,timeout,any}) plus the design's CRITICAL list
//! (CRITICAL-7/8/9 — abort ordering, source-signals flattening,
//! timeout GC retention).
//!
//! AbortSignal.timeout's GC retention test requires the runtime
//! event loop; standalone v8 isolate doesn't pump timers. The
//! timeout-firing test lives separately in `abort_runtime.rs`
//! against the full Runtime.

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
// Construction / identity / inheritance
// ---------------------------------------------------------------------------

#[test]
fn abort_controller_construct_signal_is_abortsignal() {
    let s = run_in_v8(
        r#"
        const c = new AbortController();
        JSON.stringify({
            sig_kind: c.signal.constructor.name,
            sig_aborted: c.signal.aborted,
            sig_reason: c.signal.reason === undefined ? "undefined" : "set",
            // Same-object on every access ([SameObject])
            same: c.signal === c.signal,
            // signal instanceof AbortSignal
            isAbortSignal: c.signal instanceof AbortSignal,
            // signal instanceof EventTarget — KEY assertion: native
            // class inherits via #[v8_inherit].
            isEventTarget: c.signal instanceof EventTarget,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(
        s,
        r#"{"sig_kind":"AbortSignal","sig_aborted":false,"sig_reason":"undefined","same":true,"isAbortSignal":true,"isEventTarget":true}"#
    );
}

#[test]
fn abort_controller_abort_sets_aborted_and_reason() {
    let s = run_in_v8(
        r#"
        const c = new AbortController();
        const reason = { code: 42 };
        c.abort(reason);
        JSON.stringify({
            aborted: c.signal.aborted,
            // reason is the EXACT object passed (JS identity).
            same: c.signal.reason === reason,
            code: c.signal.reason.code,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"aborted":true,"same":true,"code":42}"#);
}

#[test]
fn abort_without_reason_uses_aborterror_default() {
    let s = run_in_v8(
        r#"
        const c = new AbortController();
        c.abort();
        const r = c.signal.reason;
        JSON.stringify({
            name: r.name,
            // Default reason has DOMException-shaped properties.
            code: r.code,
            // Message is informative.
            msg_starts_with_op: r.message.startsWith("The operation"),
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(
        s,
        r#"{"name":"AbortError","code":20,"msg_starts_with_op":true}"#
    );
}

#[test]
fn abort_is_idempotent() {
    let s = run_in_v8(
        r#"
        const c = new AbortController();
        const r1 = { kind: "first" };
        const r2 = { kind: "second" };
        c.abort(r1);
        c.abort(r2);   // ignored — already aborted
        JSON.stringify({
            kind: c.signal.reason.kind,
            same: c.signal.reason === r1,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"kind":"first","same":true}"#);
}

// ---------------------------------------------------------------------------
// abort event firing
// ---------------------------------------------------------------------------

#[test]
fn abort_fires_abort_event_synchronously() {
    let s = run_in_v8(
        r#"
        const c = new AbortController();
        let fired = 0;
        let event_type;
        let target_match;
        c.signal.addEventListener("abort", (ev) => {
            fired++;
            event_type = ev.type;
            target_match = ev.target === c.signal;
        });
        c.abort();
        JSON.stringify({ fired, event_type, target_match });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(
        s,
        r#"{"fired":1,"event_type":"abort","target_match":true}"#
    );
}

#[test]
fn abort_fires_event_after_algorithms() {
    // Per DOM §3.3.1 step 5: algorithms first, then "abort" event.
    // We register an algorithm by calling our internal API would be
    // ideal but it's not exposed; instead we observe ordering via
    // the SECOND registered listener vs. the implicit "algorithms".
    // In practice the only observable from JS is event ordering;
    // both addEventListener listeners fire after `aborted = true`,
    // but we verify the algorithm-side via a different approach
    // below — see `addEventListener_with_signal_already_aborted`.
    //
    // What we CAN verify here: the listener sees `aborted = true`
    // (algorithms ran first, set aborted, then event fires).
    let s = run_in_v8(
        r#"
        const c = new AbortController();
        let aborted_in_listener;
        c.signal.addEventListener("abort", () => {
            aborted_in_listener = c.signal.aborted;
        });
        c.abort();
        JSON.stringify({ aborted_in_listener });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"aborted_in_listener":true}"#);
}

#[test]
fn already_aborted_signal_does_not_fire_abort_event_on_listener_add() {
    // Per spec: AbortSignal.abort() returns an already-aborted
    // signal. Adding an "abort" listener after the fact does NOT
    // fire (the dispatch already happened). Per WPT
    // dom/abort/AbortSignal.any.js test 2.
    let s = run_in_v8(
        r#"
        const sig = AbortSignal.abort();
        let fired = 0;
        sig.addEventListener("abort", () => { fired++; });
        // Wait via microtasks to make sure no async fire happens.
        Promise.resolve().then(() => {});
        JSON.stringify({ fired });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"fired":0}"#);
}

// ---------------------------------------------------------------------------
// throwIfAborted
// ---------------------------------------------------------------------------

#[test]
fn throw_if_aborted_no_throw_before() {
    let s = run_in_v8(
        r#"
        const c = new AbortController();
        let threw = false;
        try { c.signal.throwIfAborted(); }
        catch (e) { threw = true; }
        JSON.stringify({ threw });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"threw":false}"#);
}

#[test]
fn throw_if_aborted_throws_reason_after() {
    let s = run_in_v8(
        r#"
        const c = new AbortController();
        const r = { kind: "custom" };
        c.abort(r);
        let caught_kind;
        try { c.signal.throwIfAborted(); }
        catch (e) { caught_kind = e.kind; }
        JSON.stringify({ caught_kind });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"caught_kind":"custom"}"#);
}

// ---------------------------------------------------------------------------
// AbortSignal.abort static
// ---------------------------------------------------------------------------

#[test]
fn abort_signal_abort_returns_already_aborted() {
    let s = run_in_v8(
        r#"
        const sig = AbortSignal.abort();
        JSON.stringify({
            kind: sig.constructor.name,
            isAbortSignal: sig instanceof AbortSignal,
            aborted: sig.aborted,
            name: sig.reason.name,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(
        s,
        r#"{"kind":"AbortSignal","isAbortSignal":true,"aborted":true,"name":"AbortError"}"#
    );
}

#[test]
fn abort_signal_abort_uses_provided_reason() {
    let s = run_in_v8(
        r#"
        const r = "string-reason";
        const sig = AbortSignal.abort(r);
        JSON.stringify({ aborted: sig.aborted, reason: sig.reason });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"aborted":true,"reason":"string-reason"}"#);
}

// ---------------------------------------------------------------------------
// AbortSignal.any — synchronous behavior
// ---------------------------------------------------------------------------

#[test]
fn any_with_already_aborted_returns_aborted() {
    let s = run_in_v8(
        r#"
        const c = new AbortController();
        const r = { who: "first" };
        c.abort(r);
        const sig = AbortSignal.any([c.signal]);
        JSON.stringify({
            aborted: sig.aborted,
            same: sig.reason === r,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"aborted":true,"same":true}"#);
}

#[test]
fn any_aborts_when_first_input_aborts() {
    let s = run_in_v8(
        r#"
        const c1 = new AbortController();
        const c2 = new AbortController();
        const sig = AbortSignal.any([c1.signal, c2.signal]);
        const r = "from-c1";
        c1.abort(r);
        JSON.stringify({
            aborted: sig.aborted,
            reason: sig.reason,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"aborted":true,"reason":"from-c1"}"#);
}

#[test]
fn any_aborts_when_second_input_aborts() {
    let s = run_in_v8(
        r#"
        const c1 = new AbortController();
        const c2 = new AbortController();
        const sig = AbortSignal.any([c1.signal, c2.signal]);
        c2.abort("from-c2");
        JSON.stringify({ aborted: sig.aborted, reason: sig.reason });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"aborted":true,"reason":"from-c2"}"#);
}

#[test]
fn any_fires_abort_event_on_dependent() {
    let s = run_in_v8(
        r#"
        const c = new AbortController();
        const sig = AbortSignal.any([c.signal]);
        let fired = 0;
        sig.addEventListener("abort", () => { fired++; });
        c.abort();
        JSON.stringify({ fired, sig_aborted: sig.aborted });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"fired":1,"sig_aborted":true}"#);
}

// ---------------------------------------------------------------------------
// CRITICAL-8: AbortSignal.any transitive flattening
// ---------------------------------------------------------------------------

#[test]
fn any_flattens_through_dependent_inputs() {
    // Per DOM §3.3.4 step 4: if an input signal is itself dependent
    // (i.e. was returned by an earlier `any` call), its source
    // signals must be spliced into the new dependent's source list,
    // not the dependent itself. This way, aborting an original
    // source aborts every transitively-dependent signal.
    let s = run_in_v8(
        r#"
        const root = new AbortController();
        const middle = AbortSignal.any([root.signal]);
        const leaf = AbortSignal.any([middle]);
        // Aborting root must abort leaf transitively. The polyfill
        // got this wrong (it registered an addEventListener on the
        // input, which is `middle` — but `middle.aborted` only flips
        // when its abort algorithm runs, AFTER `leaf`'s registration
        // observed it). The native impl flattens sources, so root
        // is a direct source of leaf.
        root.abort();
        JSON.stringify({
            root_aborted: root.signal.aborted,
            middle_aborted: middle.aborted,
            leaf_aborted: leaf.aborted,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(
        s,
        r#"{"root_aborted":true,"middle_aborted":true,"leaf_aborted":true}"#
    );
}

// ---------------------------------------------------------------------------
// CRITICAL-7: signal-abort algorithm ordering
// ---------------------------------------------------------------------------

#[test]
fn dependent_reasons_collected_before_signal_steps_run() {
    // Per DOM §3.3.1: the dependent reasons are set BEFORE the
    // signal's abort algorithms run. We can observe this by adding
    // an addEventListener on the head signal (which fires AFTER
    // dependents have their reason set) and reading the dependents'
    // reason from inside.
    let s = run_in_v8(
        r#"
        const root = new AbortController();
        const dep1 = AbortSignal.any([root.signal]);
        const dep2 = AbortSignal.any([root.signal]);
        let dep1_aborted_when_root_listener_fires;
        let dep2_aborted_when_root_listener_fires;
        root.signal.addEventListener("abort", () => {
            dep1_aborted_when_root_listener_fires = dep1.aborted;
            dep2_aborted_when_root_listener_fires = dep2.aborted;
        });
        const reason = "ABORT";
        root.abort(reason);
        JSON.stringify({
            dep1_aborted_when_root_listener_fires,
            dep2_aborted_when_root_listener_fires,
            dep1_reason: dep1.reason,
            dep2_reason: dep2.reason,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    // Per spec step 3: dependents' reason is set BEFORE running
    // signal's abort steps (which include firing the abort event).
    // So when root's "abort" listener fires, dep1.aborted = true.
    assert_eq!(
        s,
        r#"{"dep1_aborted_when_root_listener_fires":true,"dep2_aborted_when_root_listener_fires":true,"dep1_reason":"ABORT","dep2_reason":"ABORT"}"#
    );
}

#[test]
fn dependent_event_order_after_root_event() {
    // Per DOM §3.3.1: the head signal's abort steps run FIRST
    // (including its event), then each dependent's abort steps run
    // in collection order. The order of "abort" event firings
    // should be: root listener, then dep listeners.
    let s = run_in_v8(
        r#"
        const root = new AbortController();
        const dep = AbortSignal.any([root.signal]);
        const order = [];
        root.signal.addEventListener("abort", () => order.push("root"));
        dep.addEventListener("abort", () => order.push("dep"));
        root.abort();
        JSON.stringify({ order });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"order":["root","dep"]}"#);
}

// ---------------------------------------------------------------------------
// MAJOR-40: abort algorithms run BEFORE the abort event
// ---------------------------------------------------------------------------
//
// We can observe this from JS via AbortSignal.any: when root aborts,
// the spec runs root's abort algorithms (including ours that wires
// dep abort) BEFORE root's "abort" event listeners. So a
// `dep.aborted` check inside root's "abort" listener must see true.
// This is the same as the test above, kept here for traceability.

// ---------------------------------------------------------------------------
// addEventListener with already-aborted signal: short-circuit
// ---------------------------------------------------------------------------

#[test]
fn add_event_listener_with_aborted_signal_no_op() {
    let s = run_in_v8(
        r#"
        const t = new EventTarget();
        const c = new AbortController();
        c.abort();   // already aborted
        let calls = 0;
        t.addEventListener("e", () => { calls++; }, { signal: c.signal });
        t.dispatchEvent(new Event("e"));
        JSON.stringify({ calls });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"calls":0}"#);
}
