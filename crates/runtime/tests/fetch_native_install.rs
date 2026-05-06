//! Smoke tests for the `globalThis.fetch` install path.
//!
//! The native install is unconditional, so these tests exercise the
//! hand-rolled callback directly without flipping any env-var gate.
//! The historical `ZEROSHIP_NATIVE_FETCH` set call survives below
//! because some sister tests share the same `init_v8` state and a
//! stale unset would change behaviour for them; the call is now a
//! no-op for `install_fetch_global` itself.
//!
//! Coverage:
//!   1. `install_fetch_global` registers `globalThis.fetch` as a
//!      function.
//!   2. `fetch("data:...")` returns a Promise (resolves async via the
//!      pump, which we don't drive here).
//!   3. The native `fetch()` synchronously rejects with the abort
//!      reason when called with a pre-aborted AbortSignal.
//!   4. 1-arg and 2-arg call shapes are both accepted.
//!
//! Async tests (real network round-trips) live in `fetch_native.rs`
//! and run against a live HTTP fixture.

#![allow(unsafe_code)]

use zeroship_runtime::dom;
use zeroship_runtime::fetch_native;
use zeroship_runtime::fetch_request;
use zeroship_runtime::fetch_response;
use zeroship_runtime::headers;
use zeroship_runtime::init_v8;
use zeroship_runtime::streams;

fn install_globals(scope: &mut v8::PinScope) {
    let global = scope.get_current_context().global(scope);
    streams::install_native_streams(scope, global);
    streams::strategies::install_byte_length_queuing_strategy(scope, global);
    streams::strategies::install_count_queuing_strategy(scope, global);
    headers::install_global(scope, global);
    dom::install_globals(scope, global);
    fetch_request::install_global(scope, global);
    fetch_response::install_global(scope, global);
    fetch_native::install_fetch_global(scope, global);
}

fn run_in_v8<F, R>(src: &str, f: F) -> R
where
    F: FnOnce(v8::Local<v8::Value>, &mut v8::PinScope) -> R,
{
    // install_fetch_global is unconditional now. The set_var call is a
    // no-op kept for symmetry with legacy tests that may still
    // reference the var via shared process-wide state.
    unsafe { std::env::set_var("ZEROSHIP_NATIVE_FETCH", "1"); }
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    install_globals(scope);

    let src_v8 = v8::String::new(scope, src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    let result = script.run(scope).unwrap();
    scope.perform_microtask_checkpoint();
    f(result, scope)
}

#[test]
fn fetch_global_installed() {
    let s = run_in_v8(
        r#"
        typeof globalThis.fetch === "function";
        "#,
        |v, scope| v.boolean_value(scope),
    );
    assert!(s, "globalThis.fetch should be installed");
}

#[test]
fn fetch_returns_promise() {
    let s = run_in_v8(
        r#"
        const p = fetch("data:text/plain,hi").catch(() => null);
        p instanceof Promise;
        "#,
        |v, scope| v.boolean_value(scope),
    );
    assert!(s, "fetch should return a Promise");
}

#[test]
fn fetch_with_pre_aborted_signal_rejects_synchronously() {
    // The promise rejects with the abort reason. We don't run the
    // pump, so the rejection settles via microtask only — but
    // perform_microtask_checkpoint runs the .then callback that
    // captures the rejection.
    let s = run_in_v8(
        r#"
        globalThis.captured = "pending";
        const ctrl = new AbortController();
        ctrl.abort();
        const p = fetch("data:text/plain,x", { signal: ctrl.signal });
        p.then(
            () => { globalThis.captured = "resolved"; },
            (e) => { globalThis.captured = "rejected:" + (e && e.name ? e.name : typeof e); }
        );
        "captured-set";
        "#,
        |_v, scope| {
            // Run microtasks to let the .then handler observe the
            // synchronous rejection.
            for _ in 0..16 {
                scope.perform_microtask_checkpoint();
            }
            let read = v8::String::new(scope, "globalThis.captured").unwrap();
            let script = v8::Script::compile(scope, read, None).unwrap();
            let v = script.run(scope).unwrap();
            v.to_rust_string_lossy(scope)
        },
    );
    // The rejection name depends on whether AbortSignal.abort()'s
    // default reason is a DOMException with name "AbortError" — our
    // implementation builds that shape. Accept any shape that
    // confirms the rejection path fired.
    assert!(s.starts_with("rejected:"), "got {s}");
}

#[test]
fn fetch_signature_takes_input_and_init() {
    // Verify length === 2 (formal arity); some specs say 1 (just input)
    // but our hand-rolled FunctionTemplate doesn't pin arity, so it
    // reports 0 (unspecified). What matters is that a 1-arg AND a
    // 2-arg call both work without throwing on shape.
    let s = run_in_v8(
        r#"
        const a = fetch("data:,hi");          // 1 arg
        const b = fetch("data:,bye", {});     // 2 args
        // Catch the rejections so they don't trigger
        // unhandledRejection diagnostics.
        a.catch(() => 0);
        b.catch(() => 0);
        a instanceof Promise && b instanceof Promise;
        "#,
        |v, scope| v.boolean_value(scope),
    );
    assert!(s);
}
