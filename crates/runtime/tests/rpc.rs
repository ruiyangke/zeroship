mod common;
use common::*;

// These tests exercise the synthetic-entry RPC contract: every HTTP
// request flows through `default.fetch`, which dispatches via
// `default.rpc(name, input, ctx)`. The `wrap_with_synthetic_entry`
// helper in `common/mod.rs` wraps user code in a tiny shim that
// implements the same contract `@zeroship/vite-plugin` emits in
// production.

use std::time::Duration;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::runtime::Runtime;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, RequestCtx, SettledFetch};

fn dispatch_zs(
    modules: Vec<zeroship_runtime::ModuleEntry>,
    name: &str,
    body: &str,
) -> Result<String, String> {
    init_v8();
    let runtime = Runtime::builder().modules(modules).build();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let url = format!("http://localhost/_zs/v1/{}", name);
    let outcome = runtime.call_fetch_handler(
        "POST",
        &url,
        &[("content-type".into(), "application/json".into())],
        body,
        &env,
        ctx,
    );

    // Sync Response — return immediately.
    if let FetchOutcome::Response { status, body, .. } = &outcome {
        if !(200..300).contains(status) {
            return Err(parse_message(body));
        }
        return Ok(unwrap_json_envelope(body));
    }

    // Async / streaming — drive through a compio runtime.
    compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();
        match outcome {
            FetchOutcome::Response { status, body, .. } => {
                if !(200..300).contains(&status) {
                    Err(parse_message(&body))
                } else {
                    Ok(unwrap_json_envelope(&body))
                }
            }
            FetchOutcome::Stream { status, body_reader, .. } => {
                let mut out = Vec::new();
                loop {
                    while let Some(chunk) = body_reader.pop() {
                        out.extend_from_slice(&chunk);
                    }
                    if body_reader.is_done() {
                        break;
                    }
                    body_reader.wait_for_data().await;
                }
                let s = String::from_utf8_lossy(&out).into_owned();
                if !(200..300).contains(&status) {
                    Err(s)
                } else {
                    Ok(s)
                }
            }
            FetchOutcome::Pending { rx, cancel: _ } => {
                let settled = compio::time::timeout(Duration::from_secs(5), rx.recv())
                    .await
                    .expect("dispatch pending timed out")
                    .expect("dispatch error");
                match settled {
                    SettledFetch::Response { status, body, .. } => {
                        if !(200..300).contains(&status) {
                            Err(parse_message(&body))
                        } else {
                            Ok(unwrap_json_envelope(&body))
                        }
                    }
                    SettledFetch::Stream { status, body_reader, .. } => {
                        let mut out = Vec::new();
                        loop {
                            while let Some(chunk) = body_reader.pop() {
                                out.extend_from_slice(&chunk);
                            }
                            if body_reader.is_done() {
                                break;
                            }
                            body_reader.wait_for_data().await;
                        }
                        let s = String::from_utf8_lossy(&out).into_owned();
                        if !(200..300).contains(&status) {
                            Err(s)
                        } else {
                            Ok(s)
                        }
                    }
                    SettledFetch::WebSocketUpgrade { .. } => panic!("unexpected WS upgrade"),
                }
            }
            FetchOutcome::WebSocketUpgrade { .. } => panic!("unexpected WS upgrade"),
        }
    })
}

fn parse_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from))
        .unwrap_or_else(|| body.to_string())
}

fn unwrap_json_envelope(body: &str) -> String {
    // The wire wraps results in `{ json: ... }`. Tests assert the inner shape.
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(inner) = v.get("json") {
            return serde_json::to_string(inner).unwrap_or_else(|_| body.to_string());
        }
    }
    body.to_string()
}

#[test]
fn basic_rpc() {
    let modules = wrap_with_synthetic_entry(
        r#"function ping() { return "pong"; }"#,
        "{ ping }",
    );
    let r = dispatch_zs(modules, "ping", r#"{"json":null}"#).unwrap();
    assert_eq!(r, "\"pong\"");
}

#[test]
fn superjson_input_date_revives_on_rpc_fast_path() {
    let modules = wrap_with_synthetic_entry(
        r#"
        function inspect(input) {
            return {
                isDate: input instanceof Date,
                iso: input instanceof Date ? input.toISOString() : null,
            };
        }
        "#,
        "{ inspect }",
    );
    let r = dispatch_zs(
        modules,
        "inspect",
        r#"{"json":"2026-01-01T00:00:00.000Z","meta":{"values":["Date"],"v":1}}"#,
    )
    .unwrap();
    assert_eq!(
        r,
        r#"{"isDate":true,"iso":"2026-01-01T00:00:00.000Z"}"#,
    );
}

#[test]
fn superjson_output_date_preserves_meta_on_rpc_fast_path() {
    let modules = wrap_with_synthetic_entry(
        r#"function today() { return new Date("2026-01-01T00:00:00.000Z"); }"#,
        "{ today }",
    );
    init_v8();
    let runtime = Runtime::builder().modules(modules).build();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler(
        "POST",
        "http://localhost/_zs/v1/today",
        &[("content-type".into(), "application/json".into())],
        r#"{"json":null}"#,
        &env,
        ctx,
    );
    let FetchOutcome::Response { status, body, .. } = outcome else {
        panic!("expected Response");
    };
    assert_eq!(status, 200, "body: {}", body);
    assert!(
        body.contains(r#""json":"2026-01-01T00:00:00.000Z""#),
        "body: {}",
        body,
    );
    assert!(
        body.contains(r#""meta":{"values":["Date"],"v":1}"#),
        "body: {}",
        body,
    );
}

#[test]
fn persistent_context() {
    init_v8();
    let modules = wrap_with_synthetic_entry(
        r#"
        let n = 0;
        function count() { return ++n; }
        "#,
        "{ count }",
    );
    let runtime = Runtime::builder().modules(modules).build();

    let call = |runtime: &Runtime| -> String {
        let env = EnvSnapshot::empty();
        let ctx = RequestCtx::new(CancelFlag::new());
        let outcome = runtime.call_fetch_handler(
            "POST", "http://localhost/_zs/v1/count",
            &[("content-type".into(), "application/json".into())],
            r#"{"json":null}"#, &env, ctx,
        );
        match outcome {
            FetchOutcome::Response { body, .. } => unwrap_json_envelope(&body),
            _ => panic!("unexpected outcome"),
        }
    };
    assert_eq!(call(&runtime), "1");
    assert_eq!(call(&runtime), "2");
    assert_eq!(call(&runtime), "3");
}

#[test]
fn sync_still_works_with_event_loop() {
    let modules = wrap_with_synthetic_entry(
        r#"function add(args) { return args[0] + args[1]; }"#,
        "{ add }",
    );
    let r = dispatch_zs(modules, "add", r#"{"json":[3,4]}"#).unwrap();
    assert_eq!(r, "7");
}

#[test]
fn set_timeout_zero_delay() {
    let modules = wrap_with_synthetic_entry(
        r#"
        function immediate() {
            return new Promise(function(resolve) {
                setTimeout(function() { resolve("immediate"); }, 0);
            });
        }
        "#,
        "{ immediate }",
    );
    let r = dispatch_zs(modules, "immediate", r#"{"json":null}"#).unwrap();
    assert_eq!(r, "\"immediate\"");
}

#[test]
fn promise_resolve_sync() {
    let modules = wrap_with_synthetic_entry(
        r#"async function test() { return "sync-async"; }"#,
        "{ test }",
    );
    let r = dispatch_zs(modules, "test", r#"{"json":null}"#).unwrap();
    assert_eq!(r, "\"sync-async\"");
}

#[test]
fn promise_then_chain_sync() {
    let modules = wrap_with_synthetic_entry(
        r#"function test() { return Promise.resolve(1).then(v => v + 10).then(v => v * 2); }"#,
        "{ test }",
    );
    let r = dispatch_zs(modules, "test", r#"{"json":null}"#).unwrap();
    assert_eq!(r, "22");
}

#[test]
fn async_generator_streams_sse() {
    // Async generator → SSE per the AI-SDK Data Stream Protocol:
    // object yields are framed as `2:[<json>]\n`, completion as `d:{}\n`.
    let modules = wrap_with_synthetic_entry(
        r#"
        async function* chat() {
            yield { token: "Hi" };
            yield { token: "!" };
        }
        "#,
        "{ chat }",
    );
    let r = dispatch_zs(modules, "chat", r#"{"json":null}"#).unwrap();
    assert!(r.contains("2:[{\"token\":\"Hi\"}]\n"), "got: {}", r);
    assert!(r.contains("2:[{\"token\":\"!\"}]\n"), "got: {}", r);
    assert!(r.contains("d:{}\n"), "got: {}", r);
}

#[test]
fn method_not_found_errors() {
    let modules = wrap_with_synthetic_entry(
        r#"function ping() { return "pong"; }"#,
        "{ ping }",
    );
    let err = dispatch_zs(modules, "nope", r#"{"json":null}"#).unwrap_err();
    assert!(err.contains("Method not found"), "got: {}", err);
}

#[test]
fn plain_object_with_status_is_not_response() {
    // Regression guard: a handler return shaped like `{ status, url }`
    // must be JSON-encoded verbatim, not fed into the Response inspection.
    let modules = wrap_with_synthetic_entry(
        r#"
        async function fetchLike() {
            return { status: 200, url: "http://example.com" };
        }
        "#,
        "{ fetchLike }",
    );
    let r = dispatch_zs(modules, "fetchLike", r#"{"json":null}"#).unwrap();
    assert_eq!(r, r#"{"status":200,"url":"http://example.com"}"#);
}

#[test]
fn user_returned_response_passes_through() {
    // A user-constructed `new Response(...)` goes through the HTTP
    // inspection path. The helper passes Response through unchanged;
    // dispatch_zs collapses the buffered body for assertion.
    let modules = wrap_with_synthetic_entry(
        r#"
        function respond() {
            return new Response("hello", { status: 200 });
        }
        "#,
        "{ respond }",
    );
    let r = dispatch_zs(modules, "respond", r#"{"json":null}"#).unwrap();
    assert_eq!(r, "hello");
}
