//! Tests for `#[v8_async_method]`.
//!
//! Each `#[v8_async_method]`-marked method on a `#[v8_class]` impl
//! produces a sync V8 callback that:
//!   1. allocates a `v8::PromiseResolver`,
//!   2. spawns the user's async body via `state.spawned_ops`,
//!   3. returns the Promise immediately.
//!
//! When the future settles the runtime pump materialises the value
//! through `OpResult::JsValue` and resolves (or rejects) the bound
//! resolver. This file drives the macro's emitted code through a real
//! `Runtime` so the full spawn-pump-resolve dance gets exercised.
//!
//! The test class is registered onto `globalThis` via a tiny
//! `NativePlugin` whose `add_setup` closure runs inside
//! `build_env_object`, before user modules load — so JS code in the
//! `dispatch` shim can `new AsyncTest()` without further plumbing.

#![allow(unsafe_code, missing_debug_implementations)]

use std::cell::Cell;

use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::runtime::Runtime;
use zeroship_runtime::state::OpError;
use zeroship_runtime::{
    init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, NativePlugin, NativeRegistrar, RequestCtx,
    SettledFetch,
};
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_async_method, v8_class, v8_constructor, v8_getter, v8_method};

// ---------------------------------------------------------------------------
// Test class — every async return shape we care about
// ---------------------------------------------------------------------------

/// State held inside the test wrapper. `Cell<u32>` is enough because
/// every method takes `&self` (the macro REJECTS `&mut self` async at
/// compile time — see compile_fail tests below). Concurrent calls are
/// serialised by the single-thread compio runtime, so plain `Cell`
/// suffices.
pub struct AsyncTest {
    pub counter: Cell<u32>,
}

#[v8_class]
impl AsyncTest {
    #[v8_constructor]
    fn new() -> Self {
        AsyncTest {
            counter: Cell::new(0),
        }
    }

    /// Sync sibling — used to verify async installs alongside sync on
    /// the same class without disturbing dispatch. Exposed as a
    /// JS-side getter so tests can read `t.current` rather than
    /// `t.current()`, which clarifies the assertion intent.
    #[v8_getter]
    fn current(&self) -> u32 {
        self.counter.get()
    }

    /// 1. Simple async, no args, returns `()` → Promise<undefined>
    #[v8_async_method]
    async fn pause_briefly(&self) {
        compio::time::sleep(std::time::Duration::from_millis(1)).await;
    }

    /// 2. Returns Result<Vec<u8>, OpError> → Promise<Uint8Array>
    #[v8_async_method]
    async fn make_bytes(&self, len: u32) -> Result<Vec<u8>, OpError> {
        compio::time::sleep(std::time::Duration::from_millis(1)).await;
        Ok(vec![0u8; len as usize])
    }

    /// 3. Returns Result<String, OpError>
    #[v8_async_method]
    async fn echo(&self, s: String) -> Result<String, OpError> {
        compio::time::sleep(std::time::Duration::from_millis(1)).await;
        Ok(s)
    }

    /// 4. Returns Result<u32, OpError>; mutates state via Cell
    #[v8_async_method]
    async fn add_then_increment(&self, n: u32) -> Result<u32, OpError> {
        compio::time::sleep(std::time::Duration::from_millis(1)).await;
        let new_val = self.counter.get() + n;
        self.counter.set(new_val);
        Ok(new_val)
    }

    /// 5. Returns Result<bool, OpError>
    #[v8_async_method]
    async fn is_even(&self, n: u32) -> Result<bool, OpError> {
        Ok(n % 2 == 0)
    }

    /// 6. Synchronous throw (no .await) — exercises the early-Err
    ///    path. `Result<(), OpError>` Err → Promise rejection.
    #[v8_async_method]
    async fn throw_sync(&self, msg: String) -> Result<(), OpError> {
        Err(OpError::type_error(format!("user: {msg}")))
    }

    /// 7. Throw after .await — exercises the late-Err path.
    #[v8_async_method]
    async fn throw_async(&self) -> Result<(), OpError> {
        compio::time::sleep(std::time::Duration::from_millis(1)).await;
        Err(OpError::type_error("after await"))
    }

    /// 8. Multiple args of mixed types.
    #[v8_async_method]
    async fn multi_args(&self, a: u32, b: String, c: bool) -> Result<String, OpError> {
        compio::time::sleep(std::time::Duration::from_millis(1)).await;
        Ok(format!("{a} {b} {c}"))
    }

    /// 9. Vec<u8> arg + Vec<u8> return — round-trips a byte buffer.
    #[v8_async_method]
    async fn echo_bytes(&self, b: Vec<u8>) -> Result<Vec<u8>, OpError> {
        compio::time::sleep(std::time::Duration::from_millis(1)).await;
        Ok(b)
    }

    /// 10. Slow increment — used by `Promise.all([slow, slow])` to
    ///     verify concurrent calls share state via Cell correctly
    ///     (each call captures its own future, both poll independently
    ///     under compio).
    #[v8_async_method]
    async fn slow_increment(&self) -> Result<u32, OpError> {
        compio::time::sleep(std::time::Duration::from_millis(20)).await;
        let new_val = self.counter.get() + 1;
        self.counter.set(new_val);
        Ok(new_val)
    }

    /// 11. RangeError variant of OpError → Promise rejected with
    ///     RangeError (not TypeError) — ensures the pump's
    ///     RejectError arm dispatches on `OpErrorKind` correctly.
    #[v8_async_method]
    async fn range_check(&self, n: u32) -> Result<u32, OpError> {
        if n > 100 {
            Err(OpError::range_error("too big"))
        } else {
            Ok(n)
        }
    }

    /// 12. Plain async with no Result wrapper — sanity-check the
    ///     non-Result return shape (T directly → Promise<T>). The
    ///     IntoResolveValue blanket only covers Result for error
    ///     dispatch; plain T uses the matching primitive variant.
    #[v8_async_method]
    async fn double(&self, n: u32) -> u32 {
        compio::time::sleep(std::time::Duration::from_millis(1)).await;
        n * 2
    }
}

// ---------------------------------------------------------------------------
// Test plugin — installs `AsyncTest` on globalThis via add_setup
// ---------------------------------------------------------------------------

struct AsyncTestPlugin;

impl NativePlugin for AsyncTestPlugin {
    fn namespace(&self) -> &str {
        // Namespace is required even if we don't register methods on it.
        // The setup closure runs during env object construction and
        // does the real install onto globalThis as a side effect.
        "_async_test"
    }

    fn register(&self, r: &mut NativeRegistrar) {
        r.add_setup("install_async_test_class", |scope, _ns_obj| {
            let global = scope.get_current_context().global(scope);
            let tmpl = AsyncTest::install(scope);
            let class_fn = tmpl.get_function(scope).unwrap();
            let key = v8::String::new(scope, "AsyncTest").unwrap();
            global.set(scope, key.into(), class_fn.into());
        });
    }
}

// ---------------------------------------------------------------------------
// Test harness — drive a full Runtime
// ---------------------------------------------------------------------------

/// Build a Runtime with our test plugin and a synthetic-entry shim
/// that exposes a `default.fetch` returning whatever the body of the
/// shim's call to `runTest()` produces.
fn run_async_js(js: &str) -> String {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        // Wrap the user JS in a `default.fetch` shim. The shim awaits
        // `runTest()` (which the user JS defines) and returns the
        // result as JSON. The harness POSTs to `/run` to invoke it.
        source: format!(
            r#"
{js}
async function _zsFetch(_request) {{
    try {{
        const result = await runTest();
        return new Response(JSON.stringify({{ ok: true, result }}), {{
            status: 200,
            headers: {{ "content-type": "application/json" }},
        }});
    }} catch (e) {{
        return new Response(JSON.stringify({{
            ok: false,
            err: {{ name: e?.name ?? "Error", msg: e?.message ?? String(e) }},
        }}), {{
            status: 200,
            headers: {{ "content-type": "application/json" }},
        }});
    }}
}}
export default {{ fetch: _zsFetch }};
"#,
        ),
    }];

    let runtime = Runtime::builder()
        .modules(modules)
        .plugin(AsyncTestPlugin)
        .build();

    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome =
        runtime.call_fetch_handler("POST", "http://test/run", &[], "", &env, ctx);

    if let FetchOutcome::Response { body, .. } = &outcome {
        return body.clone();
    }

    // Async dispatch: drive through the pump on a compio runtime.
    compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();
        match outcome {
            FetchOutcome::Response { body, .. } => body,
            FetchOutcome::Stream { body_reader, .. } => {
                let mut buf = Vec::new();
                for chunk in body_reader.drain() {
                    buf.extend_from_slice(&chunk);
                }
                String::from_utf8_lossy(&buf).into_owned()
            }
            FetchOutcome::Pending { rx, cancel: _ } => {
                let settled = compio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                    .await
                    .expect("test pending timed out")
                    .expect("test pending delivered DispatchError");
                match settled {
                    SettledFetch::Response { body, .. } => body,
                    SettledFetch::Stream { body_reader, .. } => {
                        let mut buf = Vec::new();
                        for chunk in body_reader.drain() {
                            buf.extend_from_slice(&chunk);
                        }
                        String::from_utf8_lossy(&buf).into_owned()
                    }
                    SettledFetch::WebSocketUpgrade { .. } => panic!("unexpected WS upgrade"),
                }
            }
            FetchOutcome::WebSocketUpgrade { .. } => panic!("unexpected WS upgrade"),
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Returns parsed JSON from the harness response.
fn parse(body: &str) -> serde_json::Value {
    serde_json::from_str(body).expect("harness response must be JSON")
}

/// Assert OK + return the inner `result` field.
fn ok_result(body: &str) -> serde_json::Value {
    let v = parse(body);
    assert_eq!(v["ok"], serde_json::json!(true), "expected ok, got {body}");
    v["result"].clone()
}

#[test]
fn pause_briefly_resolves_to_undefined() {
    let body = run_async_js(
        r#"
        async function runTest() {
            const t = new AsyncTest();
            const r = await t.pause_briefly();
            return r === undefined ? "undefined" : `not-undefined:${r}`;
        }
        "#,
    );
    assert_eq!(ok_result(&body), serde_json::json!("undefined"));
}

#[test]
fn make_bytes_returns_uint8array() {
    let body = run_async_js(
        r#"
        async function runTest() {
            const t = new AsyncTest();
            const u8 = await t.make_bytes(5);
            return {
                kind: u8.constructor.name,
                len: u8.byteLength,
                isView: ArrayBuffer.isView(u8),
            };
        }
        "#,
    );
    assert_eq!(
        ok_result(&body),
        serde_json::json!({"kind":"Uint8Array","len":5,"isView":true})
    );
}

#[test]
fn echo_round_trips_string() {
    let body = run_async_js(
        r#"
        async function runTest() {
            const t = new AsyncTest();
            return await t.echo("hello async");
        }
        "#,
    );
    assert_eq!(ok_result(&body), serde_json::json!("hello async"));
}

#[test]
fn add_then_increment_mutates_state() {
    let body = run_async_js(
        r#"
        async function runTest() {
            const t = new AsyncTest();
            const a = await t.add_then_increment(10);   // 10
            const b = await t.add_then_increment(7);    // 17
            const c = await t.add_then_increment(3);    // 20
            return [a, b, c, t.current];
        }
        "#,
    );
    assert_eq!(ok_result(&body), serde_json::json!([10, 17, 20, 20]));
}

#[test]
fn is_even_returns_bool() {
    let body = run_async_js(
        r#"
        async function runTest() {
            const t = new AsyncTest();
            return [await t.is_even(4), await t.is_even(5), await t.is_even(0)];
        }
        "#,
    );
    assert_eq!(ok_result(&body), serde_json::json!([true, false, true]));
}

#[test]
fn throw_sync_rejects_with_typeerror() {
    let body = run_async_js(
        r#"
        async function runTest() {
            const t = new AsyncTest();
            try {
                await t.throw_sync("boom");
                return { caught: false };
            } catch (e) {
                return { caught: true, name: e.constructor.name, msg: e.message };
            }
        }
        "#,
    );
    let r = ok_result(&body);
    assert_eq!(r["caught"], serde_json::json!(true));
    assert_eq!(r["name"], serde_json::json!("TypeError"));
    assert_eq!(r["msg"], serde_json::json!("user: boom"));
}

#[test]
fn throw_async_rejects_after_await() {
    let body = run_async_js(
        r#"
        async function runTest() {
            const t = new AsyncTest();
            try {
                await t.throw_async();
                return { caught: false };
            } catch (e) {
                return { caught: true, name: e.constructor.name, msg: e.message };
            }
        }
        "#,
    );
    let r = ok_result(&body);
    assert_eq!(r["caught"], serde_json::json!(true));
    assert_eq!(r["name"], serde_json::json!("TypeError"));
    assert_eq!(r["msg"], serde_json::json!("after await"));
}

#[test]
fn multi_args_round_trips_mixed_types() {
    let body = run_async_js(
        r#"
        async function runTest() {
            const t = new AsyncTest();
            return await t.multi_args(7, "hello", true);
        }
        "#,
    );
    assert_eq!(ok_result(&body), serde_json::json!("7 hello true"));
}

#[test]
fn echo_bytes_round_trips_buffer() {
    let body = run_async_js(
        r#"
        async function runTest() {
            const t = new AsyncTest();
            const input = new Uint8Array([1, 2, 3, 0xff, 0x80]);
            const out = await t.echo_bytes(input);
            return {
                kind: out.constructor.name,
                len: out.byteLength,
                bytes: Array.from(out),
            };
        }
        "#,
    );
    assert_eq!(
        ok_result(&body),
        serde_json::json!({
            "kind": "Uint8Array",
            "len": 5,
            "bytes": [1, 2, 3, 255, 128],
        })
    );
}

#[test]
fn promise_all_drives_concurrent_calls_correctly() {
    // Two slow_increment calls running concurrently; both .await the
    // Cell-backed counter. compio is single-threaded so the writes are
    // serialised, but each future polls independently — observed
    // counter values must be 1 then 2 (or 2 then 1 if the second
    // future polled first).
    let body = run_async_js(
        r#"
        async function runTest() {
            const t = new AsyncTest();
            const [a, b] = await Promise.all([t.slow_increment(), t.slow_increment()]);
            const sorted = [a, b].sort((x, y) => x - y);
            return { results: sorted, final: t.current };
        }
        "#,
    );
    assert_eq!(
        ok_result(&body),
        serde_json::json!({"results": [1, 2], "final": 2})
    );
}

#[test]
fn range_error_dispatches_correctly() {
    // Verifies the pump's RejectError arm correctly switches on
    // OpErrorKind: RangeError → range_error exception (not TypeError).
    let body = run_async_js(
        r#"
        async function runTest() {
            const t = new AsyncTest();
            try {
                await t.range_check(1000);
                return { caught: false };
            } catch (e) {
                return { caught: true, name: e.constructor.name, msg: e.message };
            }
        }
        "#,
    );
    let r = ok_result(&body);
    assert_eq!(r["caught"], serde_json::json!(true));
    assert_eq!(r["name"], serde_json::json!("RangeError"));
    assert_eq!(r["msg"], serde_json::json!("too big"));
}

#[test]
fn range_check_ok_path_returns_value() {
    let body = run_async_js(
        r#"
        async function runTest() {
            const t = new AsyncTest();
            return await t.range_check(50);
        }
        "#,
    );
    assert_eq!(ok_result(&body), serde_json::json!(50));
}

#[test]
fn double_returns_plain_u32() {
    // Bare T return (not wrapped in Result). The IntoResolveValue impl
    // for u32 hands back ResolveValue::U32, which the pump materialises
    // as Number.
    let body = run_async_js(
        r#"
        async function runTest() {
            const t = new AsyncTest();
            return await t.double(21);
        }
        "#,
    );
    assert_eq!(ok_result(&body), serde_json::json!(42));
}

#[test]
fn calling_method_on_wrong_receiver_throws_typeerror() {
    // `AsyncTest.prototype.echo.call({})` → no internal field 0 → the
    // sync prologue throws "Illegal invocation" before the future is
    // ever spawned. JS sees this as a regular synchronous throw, not a
    // promise rejection.
    let body = run_async_js(
        r#"
        async function runTest() {
            try {
                AsyncTest.prototype.echo.call({}, "x");
                return { caught: false };
            } catch (e) {
                return { caught: true, name: e.constructor.name, msg: e.message };
            }
        }
        "#,
    );
    let r = ok_result(&body);
    assert_eq!(r["caught"], serde_json::json!(true));
    assert_eq!(r["name"], serde_json::json!("TypeError"));
    assert_eq!(r["msg"], serde_json::json!("Illegal invocation"));
}

#[test]
fn sequential_calls_to_same_instance_share_state() {
    // Verifies the &Self pointer recovered each poll matches the
    // wrapper Object — i.e. there's no slot-mixup where call N writes
    // to a different instance from call N+1.
    let body = run_async_js(
        r#"
        async function runTest() {
            const a = new AsyncTest();
            const b = new AsyncTest();
            await a.add_then_increment(5);
            await b.add_then_increment(7);
            await a.add_then_increment(2);
            return { aFinal: a.current, bFinal: b.current };
        }
        "#,
    );
    assert_eq!(
        ok_result(&body),
        serde_json::json!({"aFinal": 7, "bFinal": 7})
    );
}

#[test]
fn instance_dropped_mid_pending_still_resolves() {
    // Drop the JS reference to the instance after kicking off
    // slow_increment(). The wrapper Global captured by the future keeps
    // the Box<AsyncTest> alive, so the future can still poll &self
    // safely. Without the keepalive, V8's GC could finalise the wrapper
    // while the future is still pending — UB.
    let body = run_async_js(
        r#"
        async function runTest() {
            let t = new AsyncTest();
            const p = t.slow_increment();   // future captures wrapper Global
            t = null;                       // drop our JS reference
            // Force a GC pass to give V8 every chance to finalise.
            if (typeof gc === "function") gc();
            return await p;
        }
        "#,
    );
    assert_eq!(ok_result(&body), serde_json::json!(1));
}
