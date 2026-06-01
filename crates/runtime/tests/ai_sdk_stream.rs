// Vercel AI-SDK Data Stream Protocol wire tests.
//
// Each line of the SSE response is `<typeId>:<json>\n`:
//
//   0:"text"               — text part (when output is string)
//   2:[<json>]             — typed object yield (when output is object)
//   e:{...}                — structured error envelope (zeroship extension)
//   d:{}                   — done
//
// These tests drive the synthetic-entry shim's SSE encoder over the
// `/__zeroship/v1/<id>` wire (HTTP POST). Async generators returned by
// procedures are piped through the shim's `_zsRpcAndRespond` helper.

mod common;
use common::*;

use std::time::Duration;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, RequestCtx, SettledFetch};
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::runtime::Runtime;

fn drain_sse(modules: Vec<zeroship_runtime::ModuleEntry>, name: &str) -> String {
    init_v8();
    let runtime = Runtime::builder().modules(modules).build();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let url = format!("http://localhost/__zeroship/v1/{}", name);
    let outcome = runtime.call_fetch_handler(
        "POST", &url,
        &[("content-type".into(), "application/json".into())],
        r#"{"json":null}"#, &env, ctx,
    );
    if let FetchOutcome::Response { body, .. } = &outcome {
        return body.clone();
    }
    compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();
        match outcome {
            FetchOutcome::Stream { body_reader, .. } => {
                let mut out = Vec::new();
                loop {
                    while let Some(chunk) = body_reader.pop() { out.extend_from_slice(&chunk); }
                    if body_reader.is_done() { break; }
                    body_reader.wait_for_data().await;
                }
                String::from_utf8_lossy(&out).into_owned()
            }
            FetchOutcome::Pending { rx, cancel: _ } => {
                let settled = compio::time::timeout(Duration::from_secs(5), rx.recv())
                    .await.expect("timeout").expect("error");
                match settled {
                    SettledFetch::Stream { body_reader, .. } => {
                        let mut out = Vec::new();
                        loop {
                            while let Some(chunk) = body_reader.pop() { out.extend_from_slice(&chunk); }
                            if body_reader.is_done() { break; }
                            body_reader.wait_for_data().await;
                        }
                        String::from_utf8_lossy(&out).into_owned()
                    }
                    SettledFetch::Response { body, .. } => body,
                    _ => panic!("unexpected"),
                }
            }
            FetchOutcome::Response { body, .. } => body,
            _ => panic!("unexpected outcome"),
        }
    })
}

#[test]
fn stream_string_output_emits_zero_lines_then_done() {
    // An async generator yielding strings produces `0:` lines per yield
    // and a final `d:{}` (no return-value emission per the proposal).
    let r = drain_sse(
        wrap_with_synthetic_entry(
            r#"
            async function* sayHello() {
                yield "Hi";
                yield " there";
            }
            "#,
            "{ sayHello }",
        ),
        "sayHello",
    );
    assert!(r.contains("0:\"Hi\"\n"), "expected 0:\"Hi\", got: {}", r);
    assert!(r.contains("0:\" there\"\n"), "expected 0:\" there\", got: {}", r);
    assert!(r.contains("d:{}\n"), "expected d:{{}}, got: {}", r);
    assert!(!r.contains("event: yield"), "legacy SSE shape leaked: {}", r);
}

#[test]
fn stream_object_output_emits_two_lines() {
    // Object yields are framed as `2:[<json>]\n`.
    let r = drain_sse(
        wrap_with_synthetic_entry(
            r#"
            async function* todos() {
                yield { id: 1, text: "first" };
                yield { id: 2, text: "second" };
            }
            "#,
            "{ todos }",
        ),
        "todos",
    );
    assert!(r.contains("2:[{\"id\":1,\"text\":\"first\"}]\n"), "got: {}", r);
    assert!(r.contains("2:[{\"id\":2,\"text\":\"second\"}]\n"), "got: {}", r);
    assert!(r.contains("d:{}\n"), "got: {}", r);
}

#[test]
fn stream_mid_error_emits_envelope_then_done() {
    // Mid-stream throw emits an `e:` envelope with structured metadata
    // followed by a final `d:{}`. message/name are mandatory; code,
    // details, retryable are optional pass-throughs.
    let r = drain_sse(
        wrap_with_synthetic_entry(
            r#"
            async function* boom() {
                yield { id: 1 };
                const err = new Error("kaboom");
                err.code = "INTERNAL";
                err.details = { hint: "demo" };
                err.retryable = false;
                throw err;
            }
            "#,
            "{ boom }",
        ),
        "boom",
    );
    assert!(r.contains("2:[{\"id\":1}]\n"), "expected first yield, got: {}", r);
    let e_idx = r.find("e:").unwrap_or_else(|| panic!("expected e: envelope, got: {}", r));
    let e_line_end = r[e_idx..].find('\n').unwrap();
    let env_json = &r[e_idx + 2..e_idx + e_line_end];
    let parsed: serde_json::Value = serde_json::from_str(env_json)
        .unwrap_or_else(|_| panic!("e: envelope not JSON: {}", env_json));
    assert_eq!(parsed["message"], "kaboom");
    assert_eq!(parsed["code"], "INTERNAL");
    assert_eq!(parsed["details"]["hint"], "demo");
    assert_eq!(parsed["retryable"], false);
    let after_err = &r[e_idx + e_line_end..];
    assert!(after_err.contains("d:{}\n"), "expected d:{{}} after error: {}", r);
}

#[test]
fn stream_string_output_yields_non_string_still_uses_zero() {
    // Per-value typeof check: strings → `0:`, anything else → `2:`.
    let r = drain_sse(
        wrap_with_synthetic_entry(
            r#"
            async function* mixed() {
                yield "alpha";
                yield 42;
            }
            "#,
            "{ mixed }",
        ),
        "mixed",
    );
    assert!(r.contains("0:\"alpha\"\n"), "got: {}", r);
    assert!(r.contains("2:[42]\n"), "got: {}", r);
    assert!(r.contains("d:{}\n"), "got: {}", r);
}
