//! Native `AsyncLocalStorage` tests.
//!
//! Covers the LangGraph-shaped subset (`getStore`, `run`,
//! `enterWith`, `disable`) plus the scenario that broke the
//! closure-based polyfill (ISS-01): `getStore()` after an awaited
//! native operation.
//!
//! The native impl uses V8's `ContinuationPreservedEmbedderData`
//! slot — V8 propagates the slot automatically across every async
//! hop, so a `run(value, cb)` body that does `await something`
//! sees the right value when the continuation resumes.

#![allow(unsafe_code)]

use zeroship_runtime::init_v8;
use zeroship_runtime::node::async_hooks;

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
    async_hooks::install_globals(scope, global);

    // Hoist `AsyncLocalStorage` from `__zsAsyncHooks` for ergonomic
    // use in JS test snippets — same shape the Vite-side synthetic
    // `node:async_hooks` re-export gives user code at runtime.
    let bind = v8::String::new(
        scope,
        "globalThis.AsyncLocalStorage = globalThis.__zsAsyncHooks.AsyncLocalStorage;",
    )
    .unwrap();
    let s = v8::Script::compile(scope, bind, None).unwrap();
    s.run(scope).unwrap();

    let src_v8 = v8::String::new(scope, src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    let result = script.run(scope).unwrap();
    f(result, scope)
}

fn js_string(val: v8::Local<v8::Value>, scope: &mut v8::PinScope) -> String {
    val.to_rust_string_lossy(scope)
}

// ---------------------------------------------------------------------------
// Basic getStore / run
// ---------------------------------------------------------------------------

#[test]
fn run_binds_store_and_getstore_reads_it() {
    let s = run_in_v8(
        r#"
        const als = new AsyncLocalStorage();
        let saw;
        const ret = als.run({ kind: "outer" }, () => {
            saw = als.getStore();
            return "fn-return";
        });
        JSON.stringify({ saw, ret, after: als.getStore() === undefined });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"saw":{"kind":"outer"},"ret":"fn-return","after":true}"#
    );
}

#[test]
fn getstore_returns_undefined_outside_run() {
    let s = run_in_v8(
        r#"
        const als = new AsyncLocalStorage();
        JSON.stringify({ store: als.getStore() === undefined });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"store":true}"#);
}

#[test]
fn nested_run_inner_sees_inner_outer_restores() {
    let s = run_in_v8(
        r#"
        const als = new AsyncLocalStorage();
        const trace = [];
        als.run("outer", () => {
            trace.push(als.getStore());
            als.run("inner", () => {
                trace.push(als.getStore());
            });
            trace.push(als.getStore());
        });
        trace.push(als.getStore() ?? "undefined");
        JSON.stringify(trace);
        "#,
        js_string,
    );
    assert_eq!(s, r#"["outer","inner","outer","undefined"]"#);
}

// ---------------------------------------------------------------------------
// THE critical regression test for ISS-01
// ---------------------------------------------------------------------------
//
// `getStore()` inside an async callback after `await` must return
// the value bound by the surrounding `run`. The closure-based polyfill
// reverted state synchronously in `try { fn(...) } finally { ... }`,
// which broke this — the .finally fired before the awaited
// continuation resumed, so `getStore()` saw the post-revert
// value (typically undefined).
//
// We don't have native `fetch` in this isolate (no event loop), so
// we use `Promise.resolve().then(...)` which exercises the same V8
// continuation machinery — the slot value is captured into the
// microtask's continuation by V8 itself. If our `run` correctly
// installs the slot before invoking the callback, the .then
// continuation sees it.

#[test]
fn getstore_survives_await_via_microtask_continuation() {
    let s = run_in_v8(
        r#"
        globalThis.__saw_before = null;
        globalThis.__saw_after = null;
        const als = new AsyncLocalStorage();
        const p = als.run("ctx-A", async () => {
            globalThis.__saw_before = als.getStore();
            await Promise.resolve();
            globalThis.__saw_after = als.getStore();
            return globalThis.__saw_after;
        });
        "marker";
        "#,
        |_val, scope| {
            // Drain the microtask queue so the async continuation runs.
            scope.perform_microtask_checkpoint();
            let global = scope.get_current_context().global(scope);
            let before_key = v8::String::new(scope, "__saw_before").unwrap();
            let before_v = global.get(scope, before_key.into()).unwrap();
            let before = before_v.to_rust_string_lossy(scope);
            let after_key = v8::String::new(scope, "__saw_after").unwrap();
            let after_v = global.get(scope, after_key.into()).unwrap();
            let after = after_v.to_rust_string_lossy(scope);
            (before, after)
        },
    );
    let (before, after) = s;
    assert_eq!(before, "ctx-A", "synchronous read should see the store");
    // KEY assertion: after the awaited microtask, getStore() still
    // returns "ctx-A". Pre-fix this would be "undefined".
    assert_eq!(after, "ctx-A", "post-await read must see the same store");
}

#[test]
fn getstore_survives_chained_await_in_isolated_branches() {
    // Two parallel async branches each in their own run() must not
    // see each other's stores. V8's ContinuationPreservedEmbedderData
    // captures the slot per-continuation, so each branch's microtask
    // resumes with its OWN context Map.
    let s = run_in_v8(
        r#"
        const als = new AsyncLocalStorage();
        globalThis.__results = [];
        const a = als.run("A", async () => {
            await Promise.resolve();
            globalThis.__results.push("a:" + als.getStore());
        });
        const b = als.run("B", async () => {
            await Promise.resolve();
            globalThis.__results.push("b:" + als.getStore());
        });
        Promise.all([a, b]).then(() => { globalThis.__results.push("done"); });
        "marker";
        "#,
        |_val, scope| {
            // Drain microtasks repeatedly until the chain settles.
            for _ in 0..10 {
                scope.perform_microtask_checkpoint();
            }
            let global = scope.get_current_context().global(scope);
            let key = v8::String::new(scope, "__results").unwrap();
            let v = global.get(scope, key.into()).unwrap();
            let arr: v8::Local<v8::Array> = v.try_into().unwrap();
            let mut out = Vec::new();
            for i in 0..arr.length() {
                let item = arr.get_index(scope, i).unwrap();
                out.push(item.to_rust_string_lossy(scope));
            }
            out
        },
    );
    assert_eq!(s, vec!["a:A", "b:B", "done"]);
}

// ---------------------------------------------------------------------------
// Throw safety: run body throws → context restored
// ---------------------------------------------------------------------------

#[test]
fn run_body_throws_context_still_restored() {
    let s = run_in_v8(
        r#"
        const als = new AsyncLocalStorage();
        let threw = false;
        try {
            als.run("ctx", () => {
                throw new Error("boom");
            });
        } catch (e) {
            threw = e.message === "boom";
        }
        JSON.stringify({
            threw,
            // Context must be cleared after the throw.
            after: als.getStore() === undefined,
        });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"threw":true,"after":true}"#);
}

#[test]
fn run_outer_throw_after_inner_run_restores_outer() {
    // Outer run sets X, inner run sets Y, inner throws. Outer's catch
    // sees X again (not undefined, not Y).
    let s = run_in_v8(
        r#"
        const als = new AsyncLocalStorage();
        let saw_before_inner, saw_in_catch, saw_after_outer;
        try {
            als.run("X", () => {
                saw_before_inner = als.getStore();
                try {
                    als.run("Y", () => {
                        throw new Error("inner-boom");
                    });
                } catch (e) {
                    saw_in_catch = als.getStore();
                    throw e; // bubble out so outer see-after fires too
                }
            });
        } catch (_e) {
            saw_after_outer = als.getStore();
        }
        JSON.stringify({ saw_before_inner, saw_in_catch, after_outer: saw_after_outer ?? "undefined" });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"saw_before_inner":"X","saw_in_catch":"X","after_outer":"undefined"}"#
    );
}

// ---------------------------------------------------------------------------
// Multiple instances coexist
// ---------------------------------------------------------------------------

#[test]
fn multiple_instances_have_distinct_contexts() {
    let s = run_in_v8(
        r#"
        const a = new AsyncLocalStorage();
        const b = new AsyncLocalStorage();
        let aIn, bIn;
        a.run("alpha", () => {
            b.run("beta", () => {
                aIn = a.getStore();
                bIn = b.getStore();
            });
            // After b's run, a still sees alpha; b is clear.
            const aMid = a.getStore();
            const bMid = b.getStore();
            globalThis.__mid = JSON.stringify({ aMid, bMid: bMid ?? "undefined" });
        });
        JSON.stringify({ aIn, bIn });
        "#,
        |val, scope| {
            let stringified = val.to_rust_string_lossy(scope);
            let global = scope.get_current_context().global(scope);
            let mid_key = v8::String::new(scope, "__mid").unwrap();
            let mid_v = global.get(scope, mid_key.into()).unwrap();
            let mid = mid_v.to_rust_string_lossy(scope);
            (stringified, mid)
        },
    );
    let (st, mid) = s;
    assert_eq!(st, r#"{"aIn":"alpha","bIn":"beta"}"#);
    assert_eq!(mid, r#"{"aMid":"alpha","bMid":"undefined"}"#);
}

// ---------------------------------------------------------------------------
// enterWith / disable
// ---------------------------------------------------------------------------

#[test]
fn enter_with_persists_for_rest_of_frame_and_disable_clears() {
    let s = run_in_v8(
        r#"
        const als = new AsyncLocalStorage();
        const trace = [];
        function run() {
            trace.push("a:" + (als.getStore() ?? "undefined"));
            als.enterWith("set-via-enter");
            trace.push("b:" + als.getStore());
            als.disable();
            trace.push("c:" + (als.getStore() ?? "undefined"));
        }
        run();
        JSON.stringify(trace);
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"["a:undefined","b:set-via-enter","c:undefined"]"#
    );
}

#[test]
fn run_with_args_passes_them_to_the_callback() {
    let s = run_in_v8(
        r#"
        const als = new AsyncLocalStorage();
        const ret = als.run("S", (a, b, c) => {
            return [als.getStore(), a, b, c].join(",");
        }, 1, 2, 3);
        JSON.stringify({ ret });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"ret":"S,1,2,3"}"#);
}

#[test]
fn run_callback_must_be_callable_throws_typeerror() {
    let s = run_in_v8(
        r#"
        const als = new AsyncLocalStorage();
        let threw = false;
        try {
            als.run("S", "not a function");
        } catch (e) {
            threw = e instanceof TypeError;
        }
        JSON.stringify({ threw });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"threw":true}"#);
}
