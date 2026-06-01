mod common;
use common::*;

use std::time::Duration;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, RequestCtx, Runtime, SettledFetch};

// Regression test: the runtime installs `globalThis.setImmediate` and
// `clearImmediate` on every isolate at boot. These two are the
// remaining bare globals owned by `node-globals.js` after Buffer
// moved out — Buffer now comes from `@rollup/plugin-inject`'s
// rewrite of bare references into `import { Buffer } from
// "node:buffer"`, so it's a build-time concern, not a runtime one.
// This test exercises the runtime in isolation (no vite-plugin), so
// only setImmediate/clearImmediate are visible bare.
#[test]
fn node_globals_set_immediate_present() {
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                return Response.json({
                    setImmediateIsFn: typeof globalThis.setImmediate === "function",
                    clearImmediateIsFn: typeof globalThis.clearImmediate === "function",
                });
            }
        };
    "#);
    match dispatch_fetch(modules, TestRequest::get("http://localhost/")) {
        FetchOutcome::Response { status, body, .. } => {
            assert_eq!(status, 200, "body: {}", body);
            assert!(body.contains(r#""setImmediateIsFn":true"#), "body: {}", body);
            assert!(body.contains(r#""clearImmediateIsFn":true"#), "body: {}", body);
        }
        _ => panic!("expected Response outcome"),
    }
}

#[test]
fn simple_response() {
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                return new Response("hello", { status: 200 });
            }
        };
    "#);
    match dispatch_fetch(modules, TestRequest::get("http://localhost/")) {
        FetchOutcome::Response { status, body, .. } => {
            assert_eq!(status, 200);
            assert_eq!(body, "hello");
        }
        _ => panic!("expected Response outcome"),
    }
}

#[test]
fn async_response() {
    let modules = m(r#"
        export default {
            async fetch(request, env, ctx) {
                await new Promise(r => setTimeout(r, 0));
                return new Response("later", { status: 202 });
            }
        };
    "#);

    // call_fetch_handler must return Pending for async handlers. The pump
    // then drives the promise to completion and delivers the final
    // SettledFetch via the receiver. This mirrors the idiom used in
    // crates/runtime/tests/http.rs ~line 198 for dispatch_http.
    compio::runtime::Runtime::new().unwrap().block_on(async move {
        init_v8();
        let runtime = Runtime::builder().modules(modules).build();
        runtime.start_pump();

        let env = EnvSnapshot::empty();
        let ctx = RequestCtx::new(CancelFlag::new());
        let outcome = runtime.call_fetch_handler(
            "GET",
            "http://localhost/",
            &[],
            "",
            &env,
            ctx,
        );

        let FetchOutcome::Pending { rx, cancel: _ } = outcome else {
            panic!("expected Pending outcome, got different variant");
        };

        let settled = compio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("receiver wait timed out")
            .expect("pending delivered DispatchError");

        match settled {
            SettledFetch::Response { status, body, .. } => {
                assert_eq!(status, 202);
                assert_eq!(body, "later");
            }
            _ => panic!("expected SettledFetch::Response"),
        }
    });
}

#[test]
fn handler_throwing_http_error_preserves_status() {
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                const err = new Error("not found");
                err.status = 404;
                throw err;
            }
        };
    "#);
    match dispatch_fetch(modules, TestRequest::get("http://localhost/")) {
        FetchOutcome::Response { status, body, .. } => {
            assert_eq!(status, 404, "expected 404 from thrown err.status, got status={} body={}", status, body);
            assert!(body.contains("not found"), "body: {}", body);
        }
        _ => panic!("expected Response outcome"),
    }
}

#[test]
fn handler_throwing_500_returns_sanitized_error_body() {
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                const err = new Error("dsn=postgres://internal/path");
                err.code = "DB_PRIVATE";
                err.details = { host: "db.internal" };
                throw err;
            }
        };
    "#);
    match dispatch_fetch(modules, TestRequest::get("http://localhost/")) {
        FetchOutcome::Response { status, body, .. } => {
            assert_eq!(status, 500, "body: {}", body);
            assert!(body.contains(r#""message":"internal error""#), "body: {}", body);
            assert!(body.contains(r#""name":"Error""#), "body: {}", body);
            assert!(body.contains(r#""request_id":""#), "body: {}", body);
            assert!(!body.contains("dsn=postgres"), "body: {}", body);
            assert!(!body.contains("DB_PRIVATE"), "body: {}", body);
            assert!(!body.contains("db.internal"), "body: {}", body);
            assert!(!body.contains("stack"), "body: {}", body);
        }
        _ => panic!("expected Response outcome"),
    }
}

#[test]
fn async_handler_rejecting_500_returns_sanitized_error_body() {
    compio::runtime::Runtime::new().unwrap().block_on(async move {
        init_v8();
        let runtime = Runtime::builder()
            .modules(m(r#"
                export default {
                    async fetch(request, env, ctx) {
                        await new Promise(r => setTimeout(r, 0));
                        throw new Error("secret async stack");
                    }
                };
            "#))
            .build();
        runtime.start_pump();

        let env = EnvSnapshot::empty();
        let ctx = RequestCtx::new(CancelFlag::new());
        let outcome = runtime.call_fetch_handler(
            "GET",
            "http://localhost/",
            &[],
            "",
            &env,
            ctx,
        );

        let FetchOutcome::Pending { rx, cancel: _ } = outcome else {
            panic!("expected Pending outcome");
        };

        let settled = compio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("receiver wait timed out")
            .expect("pending delivered DispatchError");

        match settled {
            SettledFetch::Response { status, body, .. } => {
                assert_eq!(status, 500, "body: {}", body);
                assert!(body.contains(r#""message":"internal error""#), "body: {}", body);
                assert!(body.contains(r#""request_id":""#), "body: {}", body);
                assert!(!body.contains("secret async stack"), "body: {}", body);
                assert!(!body.contains("stack"), "body: {}", body);
            }
            _ => panic!("expected SettledFetch::Response"),
        }
    });
}

#[test]
fn streaming_response() {
    // A synchronously-closed ReadableStream is collapsed by `inspect_response`
    // to `ResponseInfo::Complete` (matching dispatch_http's behavior). To
    // actually exercise the `Stream` arm, the handler must be async so
    // inspect_response runs while the stream is still open (pre-start). We
    // then schedule the close via a setTimeout(..., 0) so the pump fires it
    // only after inspect_response has snapshotted the stream.
    //
    // Pattern mirrors `streaming_http_response_async_closes_cleanly` in
    // tests/http.rs.
    let modules = m(r#"
        export default {
            async fetch(request, env, ctx) {
                // Force the handler to be async so inspect_response runs
                // while the stream is still open (pre-start).
                await new Promise(r => setTimeout(r, 0));
                const enc = new TextEncoder();
                const stream = new ReadableStream({
                    start(controller) {
                        controller.enqueue(enc.encode("chunk1"));
                        controller.enqueue(enc.encode("chunk2"));
                        // Keep the stream open until after the Response is
                        // returned; close via a timer task so inspect_response
                        // sees it as still-open (Stream arm), then it drains
                        // cleanly once the pump fires the timer.
                        //
                        // queueMicrotask is too early: microtasks drain
                        // before the handler's outer Promise resolves, so
                        // inspect_response would see a closed stream and
                        // collapse it to Complete. A setTimeout(..., 0)
                        // yields to the compio event loop and fires only
                        // after inspect_response has snapshotted the stream.
                        setTimeout(() => controller.close(), 0);
                    }
                });
                return new Response(stream, {
                    status: 200,
                    headers: { "content-type": "text/plain" }
                });
            }
        };
    "#);

    compio::runtime::Runtime::new().unwrap().block_on(async move {
        init_v8();
        let runtime = Runtime::builder().modules(modules).build();
        runtime.start_pump();

        let env = EnvSnapshot::empty();
        let ctx = RequestCtx::new(CancelFlag::new());
        let outcome = runtime.call_fetch_handler(
            "GET",
            "http://localhost/",
            &[],
            "",
            &env,
            ctx,
        );

        let FetchOutcome::Pending { rx, cancel: _ } = outcome else {
            panic!("expected Pending outcome for async handler");
        };

        let settled = compio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("receiver wait timed out")
            .expect("pending delivered DispatchError");

        match settled {
            SettledFetch::Stream { status, headers: _, body_reader: _, logs: _ } => {
                assert_eq!(status, 200);
                // Body drain verified by the sibling http.rs tests that
                // exercise the same StreamReader machinery — here we
                // assert only that the Stream variant was produced.
            }
            SettledFetch::Response { status, body, .. } => {
                panic!(
                    "expected Stream variant but got Response — inspect_response may \
                     have collapsed the stream because it closed too early \
                     (status={}, body={})",
                    status, body
                );
            }
            SettledFetch::WebSocketUpgrade { .. } => {
                panic!("expected Stream variant, got WebSocketUpgrade");
            }
        }
    });
}

#[test]
fn zs_env_returns_snapshot() {
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                return Response.json({
                    viaArg: env,
                    viaOp: __zs_env()
                });
            }
        };
    "#);
    init_v8();
    let runtime = Runtime::builder().modules(modules).build();
    // Env is always string-to-string (matches the wire format from the
    // control plane and the `env.get(name) → string | null` contract).
    let env = EnvSnapshot::vars_only(serde_json::json!({"FOO": "bar", "N": "42"}));
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler(
        "GET", "http://localhost/", &[], "",
        &env, ctx,
    );
    let FetchOutcome::Response { status, body, .. } = outcome else {
        panic!("expected Response");
    };
    assert_eq!(status, 200, "body: {}", body);
    // Both arg and op must return the env contents.
    assert!(body.contains(r#""FOO":"bar""#), "body: {}", body);
    assert!(body.contains(r#""N":"42""#), "body: {}", body);
    // Should appear TWICE (once for viaArg, once for viaOp).
    let foo_count = body.matches(r#""FOO":"bar""#).count();
    assert_eq!(foo_count, 2, "env should appear in both fields, body: {}", body);
}

#[test]
fn zs_bind_and_get_request_ctx() {
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                // Simulate what the bootstrap (PR 2) will do:
                // bind ctx on entry.
                __zs_bind_request_ctx(ctx);

                // Nested lookup — must return the SAME object reference.
                const nested = __zs_get_request_ctx();

                return Response.json({
                    sameRef: nested === ctx,
                    hasWaitUntil: typeof nested?.waitUntil === "function",
                    hasPassThrough: typeof nested?.passThroughOnException === "function"
                });
            }
        };
    "#);
    let outcome = dispatch_fetch(modules, TestRequest::get("http://localhost/"));
    let FetchOutcome::Response { status, body, .. } = outcome else {
        panic!("expected Response");
    };
    assert_eq!(status, 200, "body: {}", body);
    assert!(body.contains(r#""sameRef":true"#), "body: {}", body);
    assert!(body.contains(r#""hasWaitUntil":true"#), "body: {}", body);
    assert!(body.contains(r#""hasPassThrough":true"#), "body: {}", body);
}

#[test]
fn websocket_upgrade() {
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                const pair = new WebSocketPair();
                const [client, server] = Object.values(pair);
                server.accept();
                return new Response(null, {
                    status: 101,
                    webSocket: client
                });
            }
        };
    "#);
    match dispatch_fetch(modules, TestRequest::get("http://localhost/")) {
        FetchOutcome::WebSocketUpgrade { ws_id, headers: _ } => {
            assert!(ws_id > 0, "expected non-zero ws_id, got {}", ws_id);
        }
        other => {
            let name = match other {
                FetchOutcome::Response { .. } => "Response",
                FetchOutcome::Stream { .. } => "Stream",
                FetchOutcome::Pending { .. } => "Pending",
                FetchOutcome::WebSocketUpgrade { .. } => unreachable!(),
            };
            panic!("expected WebSocketUpgrade outcome, got {}", name);
        }
    }
}

// Regression guard ported from the deleted `tests/http.rs`. When an async
// ReadableStream.start() enqueued chunks across timer-driven await points
// and then called controller.close(), an earlier close implementation
// removed the stream from outbound_streams before the pump's final flush
// ran — buffered chunks stayed resident forever, the forwarder never
// closed, and HTTP clients hung waiting for the chunked-encoding
// terminator. The SSE [DONE] marker was the canonical symptom. This test
// rides the same plumbing via call_fetch_handler.
#[test]
fn streaming_async_closes_cleanly() {
    let modules = m(r#"
        export default {
            async fetch(request, env, ctx) {
                // Force the handler promise pending past the initial
                // microtask drain so async dispatch takes over.
                await new Promise((r) => setTimeout(r, 0));
                const encoder = new TextEncoder();
                const body = new ReadableStream({
                    async start(controller) {
                        controller.enqueue(encoder.encode("data: tick 0\n\n"));
                        await new Promise((r) => setTimeout(r, 0));
                        controller.enqueue(encoder.encode("data: tick 1\n\n"));
                        await new Promise((r) => setTimeout(r, 0));
                        controller.enqueue(encoder.encode("data: [DONE]\n\n"));
                        controller.close();
                    },
                });
                return new Response(body, {
                    headers: { "Content-Type": "text/event-stream" },
                });
            }
        };
    "#);

    compio::runtime::Runtime::new().unwrap().block_on(async move {
        init_v8();
        let runtime = Runtime::builder().modules(modules).build();
        runtime.start_pump();

        let env = EnvSnapshot::empty();
        let ctx = RequestCtx::new(CancelFlag::new());
        let outcome = runtime.call_fetch_handler(
            "GET", "http://localhost/events", &[], "",
            &env, ctx,
        );

        // The outer handler is async; we get Pending and the pump settles
        // to a Stream.
        let reader = match outcome {
            FetchOutcome::Stream { status, body_reader, .. } => {
                assert_eq!(status, 200);
                body_reader
            }
            FetchOutcome::Pending { rx, cancel: _ } => {
                let settled = compio::time::timeout(Duration::from_secs(5), rx.recv())
                    .await
                    .expect("call_fetch_handler pending timed out")
                    .expect("call_fetch_handler pending delivered DispatchError");
                match settled {
                    SettledFetch::Stream { status, body_reader, .. } => {
                        assert_eq!(status, 200);
                        body_reader
                    }
                    other => {
                        let name = match other {
                            SettledFetch::Response { .. } => "Response",
                            SettledFetch::WebSocketUpgrade { .. } => "WebSocketUpgrade",
                            SettledFetch::Stream { .. } => unreachable!(),
                        };
                        panic!("expected Stream, got {name}");
                    }
                }
            }
            other => {
                let name = match other {
                    FetchOutcome::Response { .. } => "Response",
                    FetchOutcome::WebSocketUpgrade { .. } => "WebSocketUpgrade",
                    FetchOutcome::Pending { .. } => unreachable!(),
                    FetchOutcome::Stream { .. } => unreachable!(),
                };
                panic!("expected Stream or Pending, got {name}");
            }
        };

        // Drain until is_done — the critical assertion is that close
        // actually propagates.
        let collected = compio::time::timeout(Duration::from_secs(5), async {
            let mut out = String::new();
            loop {
                while let Some(chunk) = reader.pop() {
                    out.push_str(&String::from_utf8_lossy(&chunk));
                }
                if reader.is_done() {
                    break;
                }
                reader.wait_for_data().await;
            }
            out
        })
        .await
        .expect("reader never saw close after controller.close()");

        assert!(collected.contains("data: tick 0"), "got: {collected}");
        assert!(collected.contains("data: tick 1"), "got: {collected}");
        assert!(collected.contains("data: [DONE]"), "final marker missing; got: {collected}");
    });
}

// ===========================================================================
// PR 2 Task 3 — zeroship module surface tests
// ===========================================================================
//
// These lock in the three exports of the user-facing `zeroship` module:
// `env`, `waitUntil`, `getRequest`. The module is injected by the runtime
// alongside the bootstrap (see crates/runtime/src/init.rs::ZEROSHIP_MODULE_JS).

#[test]
fn zeroship_module_env_import() {
    // User code imports `env` from the `zeroship` module — the same
    // object should surface as the `env` handler arg and `__zs_env()`.
    let modules = m(r#"
        import { env } from "zeroship";
        export default {
            fetch(request, envArg, ctx) {
                return Response.json({
                    fromImport: env,
                    fromArg: envArg,
                    fromOp: __zs_env(),
                    allEqual: JSON.stringify(env) === JSON.stringify(envArg)
                           && JSON.stringify(env) === JSON.stringify(__zs_env())
                });
            }
        };
    "#);
    init_v8();
    let runtime = Runtime::builder().modules(modules).build();
    let env = EnvSnapshot::vars_only(serde_json::json!({"FOO": "bar"}));
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler(
        "GET", "http://localhost/", &[], "", &env, ctx,
    );
    let FetchOutcome::Response { status, body, .. } = outcome else {
        panic!("expected Response");
    };
    assert_eq!(status, 200, "body: {}", body);
    assert!(body.contains(r#""allEqual":true"#), "body: {}", body);
    assert!(body.contains(r#""FOO":"bar""#), "body: {}", body);
}

#[test]
fn zeroship_wait_until_accepts_promise() {
    // waitUntil with a Promise arg should succeed silently (no throw).
    // We can't easily observe the pump's wait_until_by_request state from
    // userland, but the handler completing without error is proof the op
    // accepted the Promise.
    let modules = m(r#"
        import { waitUntil } from "zeroship";
        export default {
            fetch(request, env, ctx) {
                let called = false;
                try {
                    waitUntil(Promise.resolve("bg work"));
                    called = true;
                } catch (e) {
                    return Response.json({ threw: String(e), called: false });
                }
                return Response.json({ called });
            }
        };
    "#);
    match dispatch_fetch(modules, TestRequest::get("http://localhost/")) {
        FetchOutcome::Response { status, body, .. } => {
            assert_eq!(status, 200, "body: {}", body);
            assert!(body.contains(r#""called":true"#), "body: {}", body);
        }
        _ => panic!("expected Response"),
    }
}

#[test]
fn zeroship_wait_until_rejects_non_promise() {
    // waitUntil should throw TypeError for non-Promise args.
    let modules = m(r#"
        import { waitUntil } from "zeroship";
        export default {
            fetch(request, env, ctx) {
                try {
                    waitUntil("not a promise");
                    return Response.json({ threw: false });
                } catch (e) {
                    return Response.json({
                        threw: true,
                        name: e.name,
                        message: e.message,
                    });
                }
            }
        };
    "#);
    match dispatch_fetch(modules, TestRequest::get("http://localhost/")) {
        FetchOutcome::Response { status, body, .. } => {
            assert_eq!(status, 200, "body: {}", body);
            assert!(body.contains(r#""threw":true"#), "body: {}", body);
            assert!(body.contains(r#""name":"TypeError""#), "body: {}", body);
        }
        _ => panic!("expected Response"),
    }
}

#[test]
fn zeroship_get_request_returns_request() {
    // getRequest() returns the same Request the handler got as arg 1.
    let modules = m(r#"
        import { getRequest } from "zeroship";
        export default {
            fetch(request, env, ctx) {
                const fromLookup = getRequest();
                return Response.json({
                    sameRef: fromLookup === request,
                    url: fromLookup.url,
                });
            }
        };
    "#);
    match dispatch_fetch(modules, TestRequest::get("http://localhost/foo")) {
        FetchOutcome::Response { status, body, .. } => {
            assert_eq!(status, 200, "body: {}", body);
            assert!(body.contains(r#""sameRef":true"#), "body: {}", body);
            assert!(body.contains("/foo"), "body: {}", body);
        }
        _ => panic!("expected Response"),
    }
}

// ===========================================================================
// Spec wire: /__zeroship/v1/<id> + superjson { json } envelope
// ===========================================================================
//
// `/__zeroship/v1/<id>` traffic routes through `default.rpc`; everything else
// hits `default.fetch(request, env, ctx)`. See
// `docs/reference/zeroship-standard.md` for the full contract.
//
// Tests synthesize a tiny `default.{fetch, rpc}` shim that wears
// function-shape `default.rpc` (the advanced / back-compat path). The
// bootstrap's WS-subscription path and the kernel's HTTP path both call
// into this shape. The dict-shape contract is exercised by
// `crates/runtime/tests/rpc_dispatch.rs`.

const SYNTHETIC_ENTRY_PROLOG: &str = r#"
function _zsErrResponse(status, code, message) {
    return new Response(JSON.stringify({ message, name: "Error", code }), {
        status, headers: { "content-type": "application/json" },
    });
}
async function _zsRpcAndRespond(rpc, name, input) {
    try {
        const result = await rpc(name, input);
        if (result != null && typeof result === "object"
            && typeof result[Symbol.asyncIterator] === "function"
            && typeof result.next === "function") {
            const enc = new TextEncoder();
            const body = new ReadableStream({
                async start(controller) {
                    try {
                        while (true) {
                            const step = await result.next();
                            if (step.done) { controller.enqueue(enc.encode("d:{}\n")); break; }
                            const v = step.value;
                            if (typeof v === "string") {
                                controller.enqueue(enc.encode("0:" + JSON.stringify(v) + "\n"));
                            } else {
                                controller.enqueue(enc.encode("2:[" + JSON.stringify(v) + "]\n"));
                            }
                        }
                    } catch (e) {
                        const env = { message: e?.message ?? String(e), name: e?.name ?? "Error" };
                        if (e && typeof e.code === "string") env.code = e.code;
                        if (e && e.details !== undefined) env.details = e.details;
                        if (e && typeof e.retryable === "boolean") env.retryable = e.retryable;
                        controller.enqueue(enc.encode("e:" + JSON.stringify(env) + "\n"));
                        controller.enqueue(enc.encode("d:{}\n"));
                    } finally { controller.close(); }
                },
            });
            return new Response(body, {
                status: 200,
                headers: { "content-type": "text/event-stream", "cache-control": "no-cache" },
            });
        }
        if (result instanceof Response) return result;
        return new Response(
            JSON.stringify({ json: result === undefined ? null : result }),
            { status: 200, headers: { "content-type": "application/json" } },
        );
    } catch (err) {
        const status = (err && Number.isInteger(err.status) && err.status >= 400 && err.status < 600) ? err.status : 500;
        const body = { message: err?.message ?? String(err), name: err?.name ?? "Error" };
        if (err && typeof err.code === "string") body.code = err.code;
        if (err && err.details !== undefined) body.details = err.details;
        if (err && typeof err.retryable === "boolean") body.retryable = err.retryable;
        return new Response(JSON.stringify(body), {
            status, headers: { "content-type": "application/json" },
        });
    }
}
async function _zsFetchEntry(request, rpc) {
    const url = new URL(request.url);
    if (!url.pathname.startsWith("/__zeroship/v1/")) {
        return new Response("Not Found", { status: 404 });
    }
    const id = decodeURIComponent(url.pathname.slice("/__zeroship/v1/".length));
    if (!id) return _zsErrResponse(400, "INVALID_ARGUMENT", "missing wireId");
    let input = undefined;
    if (request.method === "POST") {
        const text = await request.text();
        if (text) {
            try {
                const env = JSON.parse(text);
                input = env && typeof env === "object" && "json" in env ? env.json : env;
            } catch (e) {
                return _zsErrResponse(400, "INVALID_ARGUMENT", "invalid JSON body: " + (e?.message ?? e));
            }
        }
    }
    return await _zsRpcAndRespond(rpc, id, input);
}
"#;

fn synthetic_entry(procs: &str) -> Vec<zeroship_runtime::ModuleEntry> {
    let src = format!(
        r#"
{prolog}
{procs}
const _procedures = _makeProcedures();
async function _zsRpc(name, input, _ctx) {{
    const fn = _procedures[name];
    if (typeof fn !== "function") {{
        throw Object.assign(new Error("Method not found: " + name), {{ status: 404, code: "NOT_FOUND" }});
    }}
    return await fn(input);
}}
export default {{
    fetch: (request) => _zsFetchEntry(request, _zsRpc),
    rpc: _zsRpc,
}};
"#,
        prolog = SYNTHETIC_ENTRY_PROLOG,
        procs = procs,
    );
    m(&src)
}

#[test]
fn bootstrap_routes_rpc_to_named_export() {
    let modules = synthetic_entry(r#"
        function greet(name) { return { hello: name }; }
        function _makeProcedures() { return { greet }; }
    "#);
    init_v8();
    let runtime = Runtime::builder().modules(modules).build();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler(
        "POST",
        "http://localhost/__zeroship/v1/greet",
        &[("content-type".into(), "application/json".into())],
        r#"{"json":"world"}"#,
        &env,
        ctx,
    );
    let FetchOutcome::Response { status, body, .. } = outcome else {
        panic!("expected Response");
    };
    assert_eq!(status, 200, "body: {}", body);
    // Wire wraps the result in `{ json: ... }`.
    assert!(body.contains(r#""hello":"world""#), "body: {}", body);
}

#[test]
fn bootstrap_rpc_method_not_found_returns_404() {
    let modules = synthetic_entry(r#"
        function greet() { return "hi"; }
        function _makeProcedures() { return { greet }; }
    "#);
    init_v8();
    let runtime = Runtime::builder().modules(modules).build();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler(
        "POST",
        "http://localhost/__zeroship/v1/unknown",
        &[("content-type".into(), "application/json".into())],
        "{}",
        &env,
        ctx,
    );
    let FetchOutcome::Response { status, body, .. } = outcome else {
        panic!("expected Response");
    };
    assert_eq!(status, 404, "body: {}", body);
    assert!(body.contains("Method not found"), "body: {}", body);
}

#[test]
fn bootstrap_rpc_malformed_json_returns_400() {
    let modules = synthetic_entry(r#"
        function greet(x) { return x; }
        function _makeProcedures() { return { greet }; }
    "#);
    init_v8();
    let runtime = Runtime::builder().modules(modules).build();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler(
        "POST",
        "http://localhost/__zeroship/v1/greet",
        &[("content-type".into(), "application/json".into())],
        "not-json",
        &env,
        ctx,
    );
    let FetchOutcome::Response { status, body, .. } = outcome else {
        panic!("expected Response");
    };
    assert_eq!(status, 400, "body: {}", body);
    assert!(body.contains("invalid JSON body"), "body: {}", body);
}

#[test]
fn bootstrap_rpc_input_extracted_from_json_envelope() {
    // New wire: body is `{"json": <input>}`. The input is unwrapped before
    // being passed to the handler.
    let modules = synthetic_entry(r#"
        function echo(x) { return { received: x }; }
        function _makeProcedures() { return { echo }; }
    "#);
    init_v8();
    let runtime = Runtime::builder().modules(modules).build();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler(
        "POST",
        "http://localhost/__zeroship/v1/echo",
        &[("content-type".into(), "application/json".into())],
        r#"{"json":{"hello":"world"}}"#,
        &env,
        ctx,
    );
    let FetchOutcome::Response { status, body, .. } = outcome else {
        panic!("expected Response");
    };
    assert_eq!(status, 200, "body: {}", body);
    assert!(body.contains(r#""hello":"world""#), "body: {}", body);
}

// ===========================================================================
// Structured-error envelope (code / details / retryable)
// ===========================================================================
//
// The bootstrap's `errorResponse` helper forwards optional fields from a
// thrown error — `code` (gRPC-style string), `details` (any JSON), and
// `retryable` (boolean) — alongside the existing `message` / `name` /
// `status`. Lets RPC procedures throw structured errors that the wire
// preserves, so callers (and the SSE path) can branch on `.code` or read
// the `.details` payload without re-deriving them from `.message`.

#[test]
fn rpc_error_envelope_carries_code_details_retryable() {
    // Procedure throws an Error with `code`, `details`, `retryable`, and
    // `status`. The synthetic entry's error envelope must forward all
    // four to the wire — regression guard for the structured-error path
    // the WebSocket subscription / fetch paths rely on.
    let modules = synthetic_entry(r#"
        function fail(_input) {
            throw Object.assign(new Error("limit must be at most 100"), {
                code: "INVALID_ARGUMENT",
                details: { issues: [{ path: ["limit"], message: "too big" }] },
                retryable: false,
                status: 400,
            });
        }
        function _makeProcedures() { return { fail }; }
    "#);
    init_v8();
    let runtime = Runtime::builder().modules(modules).build();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler(
        "POST",
        "http://localhost/__zeroship/v1/fail",
        &[("content-type".into(), "application/json".into())],
        "{}",
        &env,
        ctx,
    );
    let FetchOutcome::Response { status, body, .. } = outcome else {
        panic!("expected Response");
    };
    assert_eq!(status, 400, "body: {}", body);
    let v: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("body is not JSON: {} (body: {})", e, body));
    assert_eq!(v["message"], "limit must be at most 100", "body: {}", body);
    assert_eq!(v["name"], "Error", "body: {}", body);
    assert_eq!(v["code"], "INVALID_ARGUMENT", "body: {}", body);
    assert_eq!(v["retryable"], false, "body: {}", body);
    assert_eq!(
        v["details"],
        serde_json::json!({ "issues": [{ "path": ["limit"], "message": "too big" }] }),
        "body: {}", body
    );
}

#[test]
fn rpc_error_envelope_omits_absent_optional_fields() {
    // Procedure throws a plain Error with only `status` (and the implicit
    // `message` / `name`). The wire must NOT carry `code`, `details`, or
    // `retryable` keys at all — additive forwarding only when present.
    let modules = synthetic_entry(r#"
        function fail() { throw Object.assign(new Error("plain"), { status: 418 }); }
        function _makeProcedures() { return { fail }; }
    "#);
    init_v8();
    let runtime = Runtime::builder().modules(modules).build();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler(
        "POST",
        "http://localhost/__zeroship/v1/fail",
        &[("content-type".into(), "application/json".into())],
        "{}",
        &env,
        ctx,
    );
    let FetchOutcome::Response { status, body, .. } = outcome else {
        panic!("expected Response");
    };
    assert_eq!(status, 418, "body: {}", body);
    let v: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("body is not JSON: {} (body: {})", e, body));
    assert_eq!(v["message"], "plain", "body: {}", body);
    assert_eq!(v["name"], "Error", "body: {}", body);
    let obj = v.as_object().expect("body must be a JSON object");
    assert!(!obj.contains_key("code"), "code must be omitted when absent: {}", body);
    assert!(!obj.contains_key("details"), "details must be omitted when absent: {}", body);
    assert!(!obj.contains_key("retryable"), "retryable must be omitted when absent: {}", body);
}

#[test]
fn rpc_error_envelope_ignores_non_string_code_and_non_bool_retryable() {
    // Defensive: `code` must be a string, `retryable` must be a boolean.
    // Other types are silently dropped — the envelope is a contract the
    // wire side relies on; we don't propagate `code: 42` as a number.
    // `details` accepts any JSON value (including non-objects).
    let modules = synthetic_entry(r#"
        function fail() {
            throw Object.assign(new Error("bad shape"), {
                code: 42,                        // not a string → drop
                retryable: "yes",                // not a boolean → drop
                details: ["array", "is", "ok"],  // any JSON → keep
                // 422: a non-boundary 4xx where the verbose envelope
                // survives. (5xx bodies are blanked by the dispatch
                // error-sanitization rail, which would defeat the
                // type-filtering this test exercises.)
                status: 422,
            });
        }
        function _makeProcedures() { return { fail }; }
    "#);
    init_v8();
    let runtime = Runtime::builder().modules(modules).build();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler(
        "POST",
        "http://localhost/__zeroship/v1/fail",
        &[("content-type".into(), "application/json".into())],
        "{}",
        &env,
        ctx,
    );
    let FetchOutcome::Response { status, body, .. } = outcome else {
        panic!("expected Response");
    };
    assert_eq!(status, 422, "body: {}", body);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let obj = v.as_object().unwrap();
    assert!(!obj.contains_key("code"), "non-string code dropped: {}", body);
    assert!(!obj.contains_key("retryable"), "non-bool retryable dropped: {}", body);
    assert_eq!(v["details"], serde_json::json!(["array", "is", "ok"]), "body: {}", body);
}

#[test]
fn sse_error_frame_carries_code_details_retryable() {
    // Async generator throws partway through. The synthetic entry's
    // SSE encoder wraps the throw as an `e:` envelope frame (AI-SDK
    // Data Stream Protocol; the `e:` typeId is our extension — ai-sdk
    // parsers tolerate unknown ids). The payload carries the same
    // envelope shape as the RPC error wire. Always followed by `d:{}`.
    let modules = synthetic_entry(r#"
        async function* stream() {
            yield { tick: 0 };
            throw Object.assign(new Error("upstream gone"), {
                code: "UNAVAILABLE",
                details: { upstream: "db", attempt: 3 },
                retryable: true,
                status: 503,
            });
        }
        function _makeProcedures() { return { stream }; }
    "#);

    init_v8();
    let runtime = zeroship_runtime::runtime::Runtime::builder().modules(modules).build();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler(
        "POST",
        "http://localhost/__zeroship/v1/stream",
        &[("content-type".into(), "application/json".into())],
        "{}",
        &env,
        ctx,
    );

    // Drain the response body into a single string. SSE may surface as
    // Stream variant if the outer Response was async.
    let body = compio::runtime::Runtime::new().unwrap().block_on(async move {
        runtime.start_pump();
        match outcome {
            FetchOutcome::Response { body, .. } => body,
            FetchOutcome::Stream { body_reader, .. } => {
                let mut out = Vec::new();
                for chunk in body_reader.drain() { out.extend_from_slice(&chunk); }
                String::from_utf8_lossy(&out).into_owned()
            }
            FetchOutcome::Pending { rx, cancel: _ } => {
                let settled = compio::time::timeout(Duration::from_secs(5), rx.recv())
                    .await.expect("timeout").expect("dispatch error");
                match settled {
                    SettledFetch::Stream { body_reader, .. } => {
                        let mut out = Vec::new();
                        loop {
                            while let Some(chunk) = body_reader.pop() {
                                out.extend_from_slice(&chunk);
                            }
                            if body_reader.is_done() { break; }
                            body_reader.wait_for_data().await;
                        }
                        String::from_utf8_lossy(&out).into_owned()
                    }
                    SettledFetch::Response { body, .. } => body,
                    _ => panic!("unexpected outcome"),
                }
            }
            _ => panic!("unexpected outcome"),
        }
    });

    assert!(body.contains("2:[{\"tick\":0}]\n"), "got: {}", body);
    assert!(body.contains("e:"), "got: {}", body);

    let e_idx = body.find("e:").expect("error frame missing");
    let after = &body[e_idx + 2..];
    let line_end = after.find('\n').expect("error frame not newline-terminated");
    let payload: serde_json::Value = serde_json::from_str(&after[..line_end])
        .unwrap_or_else(|e| panic!("error data not JSON: {} (line: {})", e, &after[..line_end]));
    assert_eq!(payload["message"], "upstream gone", "payload: {}", payload);
    assert_eq!(payload["name"], "Error", "payload: {}", payload);
    assert_eq!(payload["code"], "UNAVAILABLE", "payload: {}", payload);
    assert_eq!(payload["retryable"], true, "payload: {}", payload);
    assert_eq!(
        payload["details"],
        serde_json::json!({ "upstream": "db", "attempt": 3 }),
        "payload: {}", payload
    );
    assert!(body[e_idx..].contains("d:{}\n"), "expected d:{{}} after error: {}", body);
}

// ===========================================================================
// Part B — vars / secrets / expose split (process.env hardening)
// ===========================================================================
//
// These tests lock in the contract that `process.env` only carries `vars`
// (plus secrets explicitly listed in the per-app `expose` opt-in), while
// the `zeroship` module's `env` and `env.get()` see the full merged
// `vars + secrets` map. The wire shape is:
//
//   { "vars": {...}, "secrets": {...}, "expose": [...] }
//
// See `EnvSnapshot::new` in fetch_outcome.rs.

use std::collections::BTreeMap;

fn vars(items: &[(&str, &str)]) -> BTreeMap<String, String> {
    items.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[test]
fn var_appears_in_process_env() {
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                return Response.json({ K: globalThis.process.env.K });
            }
        };
    "#);
    let env = EnvSnapshot::new(vars(&[("K", "v")]), BTreeMap::new(), vec![]);
    match dispatch_fetch_with_env(modules, TestRequest::get("http://localhost/"), env) {
        FetchOutcome::Response { status, body, .. } => {
            assert_eq!(status, 200, "body: {}", body);
            assert!(body.contains(r#""K":"v""#), "body: {}", body);
        }
        _ => panic!("expected Response"),
    }
}

#[test]
fn secret_does_not_appear_in_process_env() {
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                return Response.json({
                    type: typeof globalThis.process.env.SECRET_KEY,
                });
            }
        };
    "#);
    let env = EnvSnapshot::new(
        BTreeMap::new(),
        vars(&[("SECRET_KEY", "topsecret")]),
        vec![],
    );
    match dispatch_fetch_with_env(modules, TestRequest::get("http://localhost/"), env) {
        FetchOutcome::Response { status, body, .. } => {
            assert_eq!(status, 200, "body: {}", body);
            assert!(body.contains(r#""type":"undefined""#), "body: {}", body);
            // Belt-and-suspenders: the literal value must not appear anywhere.
            assert!(!body.contains("topsecret"), "secret leaked into response: {}", body);
        }
        _ => panic!("expected Response"),
    }
}

#[test]
fn secret_visible_via_zeroship_env() {
    let modules = m(r#"
        import { env } from "zeroship";
        export default {
            fetch(request, _envArg, ctx) {
                return Response.json({ value: env.SECRET_KEY ?? null });
            }
        };
    "#);
    let env = EnvSnapshot::new(
        BTreeMap::new(),
        vars(&[("SECRET_KEY", "topsecret")]),
        vec![],
    );
    match dispatch_fetch_with_env(modules, TestRequest::get("http://localhost/"), env) {
        FetchOutcome::Response { status, body, .. } => {
            assert_eq!(status, 200, "body: {}", body);
            assert!(body.contains(r#""value":"topsecret""#), "body: {}", body);
        }
        _ => panic!("expected Response"),
    }
}

#[test]
fn secret_visible_via_env_get() {
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                // `env.get(name)` is the lower-level primitive — the same
                // accessor used by SDKs that need a name-keyed lookup.
                const v = globalThis.env.get("SECRET_KEY");
                return Response.json({ value: v ?? null });
            }
        };
    "#);
    let env = EnvSnapshot::new(
        BTreeMap::new(),
        vars(&[("SECRET_KEY", "topsecret")]),
        vec![],
    );
    match dispatch_fetch_with_env(modules, TestRequest::get("http://localhost/"), env) {
        FetchOutcome::Response { status, body, .. } => {
            assert_eq!(status, 200, "body: {}", body);
            assert!(body.contains(r#""value":"topsecret""#), "body: {}", body);
        }
        _ => panic!("expected Response"),
    }
}

#[test]
fn exposed_secret_appears_in_process_env() {
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                return Response.json({ K: globalThis.process.env.K ?? null });
            }
        };
    "#);
    // Secret is explicitly opted-in via `expose`.
    let env = EnvSnapshot::new(
        BTreeMap::new(),
        vars(&[("K", "v")]),
        vec!["K".to_string()],
    );
    match dispatch_fetch_with_env(modules, TestRequest::get("http://localhost/"), env) {
        FetchOutcome::Response { status, body, .. } => {
            assert_eq!(status, 200, "body: {}", body);
            assert!(body.contains(r#""K":"v""#), "body: {}", body);
        }
        _ => panic!("expected Response"),
    }
}

#[test]
fn var_and_secret_same_key_var_wins_in_process_env() {
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                return Response.json({ K: globalThis.process.env.K ?? null });
            }
        };
    "#);
    // Even with K in expose, the var wins because vars are the
    // explicit, non-sensitive surface; we don't shadow them with a
    // collision-named secret.
    let env = EnvSnapshot::new(
        vars(&[("K", "var-value")]),
        vars(&[("K", "secret-value")]),
        vec!["K".to_string()],
    );
    match dispatch_fetch_with_env(modules, TestRequest::get("http://localhost/"), env) {
        FetchOutcome::Response { status, body, .. } => {
            assert_eq!(status, 200, "body: {}", body);
            assert!(body.contains(r#""K":"var-value""#), "body: {}", body);
        }
        _ => panic!("expected Response"),
    }
}

#[test]
fn merged_env_in_zeroship_module_secret_wins() {
    let modules = m(r#"
        import { env } from "zeroship";
        export default {
            fetch(request, _envArg, ctx) {
                return Response.json({ K: env.K ?? null });
            }
        };
    "#);
    // On the explicit, audited surface (`zeroship.env` / `env.get`),
    // secrets win on collision because they are the authoritative
    // value for sensitive lookups.
    let env = EnvSnapshot::new(
        vars(&[("K", "var-value")]),
        vars(&[("K", "secret-value")]),
        vec![],
    );
    match dispatch_fetch_with_env(modules, TestRequest::get("http://localhost/"), env) {
        FetchOutcome::Response { status, body, .. } => {
            assert_eq!(status, 200, "body: {}", body);
            assert!(body.contains(r#""K":"secret-value""#), "body: {}", body);
        }
        _ => panic!("expected Response"),
    }
}

// Plugin callback for `env_exposes_plugin_namespace` — returns the string
// "pong". Free function so it coerces to `v8::FunctionCallback` without the
// closure gymnastics that `NativeRegistrar::add<F>`'s trait bound rejects.
fn echo_ping_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let s = v8::String::new(scope, "pong").unwrap();
    rv.set(s.into());
}

#[test]
fn env_exposes_plugin_namespace() {
    use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};
    use std::sync::Arc;

    struct EchoPlugin;
    impl NativePlugin for EchoPlugin {
        fn namespace(&self) -> &str {
            "echo"
        }
        fn register(&self, r: &mut NativeRegistrar) {
            r.add("ping", echo_ping_callback);
        }
    }

    let modules = m(r#"
        import { env } from "zeroship";
        export default {
            fetch(request, envArg, ctx) {
                return Response.json({
                    hasEchoImport: typeof env.echo === "object",
                    hasEchoArg: typeof envArg.echo === "object",
                    fromImport: env.echo.ping(),
                    fromArg: envArg.echo.ping(),
                    sameRef: env.echo === envArg.echo,
                });
            }
        };
    "#);
    init_v8();
    let runtime = Runtime::builder()
        .modules(modules)
        .plugins(vec![Arc::new(EchoPlugin) as Arc<dyn NativePlugin>])
        .build();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler(
        "GET",
        "http://localhost/",
        &[],
        "",
        &env,
        ctx,
    );
    let FetchOutcome::Response { status, body, .. } = outcome else {
        panic!("expected Response");
    };
    assert_eq!(status, 200, "body: {}", body);
    assert!(body.contains(r#""hasEchoImport":true"#), "body: {}", body);
    assert!(body.contains(r#""hasEchoArg":true"#), "body: {}", body);
    assert!(body.contains(r#""fromImport":"pong""#), "body: {}", body);
    assert!(body.contains(r#""fromArg":"pong""#), "body: {}", body);
    assert!(body.contains(r#""sameRef":true"#), "body: {}", body);
}
