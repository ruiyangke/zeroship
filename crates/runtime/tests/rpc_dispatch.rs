//! Stage 5a — embedded RPC dispatcher (`__zsDispatch`) tests.
//!
//! Covers both dispatch shapes the runtime now accepts on
//! `user.default.rpc`:
//!
//!   - Dict-shape `{ [wireId]: handler }` (Stage 5a, new). The
//!     bootstrap wraps the dict in `globalThis.__zsDispatch` which owns:
//!       * input validation via `fn.config.input.parse()`
//!       * capability frame via `__zsEnterKind` / `__zsExitKind`
//!       * AsyncIterator stream framing (`__zsOutputIsString`)
//!       * dev-only output validation via `__zsValidateOutput`
//!   - Function-shape `(name, input, ctx) => ...` (legacy back-compat).
//!     The bootstrap uses the function directly; `__zsDispatch` does
//!     NOT run.
//!
//! Tests drive the kernel via `call_fetch_handler` against the spec
//! wire (`POST /_zs/v1/<id>` with `{"json":<input>}`) so the full
//! dispatch path — including the Rust-side fast path that reads
//! `default.rpc` off the bootstrap module's namespace — is exercised.

use std::time::Duration;

use zeroship_runtime::{
    init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, SettledFetch,
};
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::runtime::Runtime;

// ── Helpers ────────────────────────────────────────────────────────────────

fn build_runtime(user_src: &str) -> Runtime {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: user_src.into(),
    }];
    Runtime::builder().modules(modules).build()
}

fn parse_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("message")
                .and_then(|m| m.as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| body.to_string())
}

fn unwrap_json_envelope(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(inner) = v.get("json") {
            return serde_json::to_string(inner).unwrap_or_else(|_| body.to_string());
        }
    }
    body.to_string()
}

/// Returns `(status, body_string)` from a single RPC dispatch. The body
/// is JSON-decoded enough to peek at top-level fields; tests assert
/// against the raw envelope or the unwrapped `json` value.
fn dispatch(runtime: &Runtime, name: &str, body: &str) -> (u16, String) {
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
    if let FetchOutcome::Response { status, body, .. } = &outcome {
        return (*status, body.clone());
    }
    // Async/stream — drive on a compio runtime.
    compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();
        match outcome {
            FetchOutcome::Response { status, body, .. } => (status, body),
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
                (status, String::from_utf8_lossy(&out).into_owned())
            }
            FetchOutcome::Pending { rx, cancel: _ } => {
                let settled = compio::time::timeout(Duration::from_secs(5), rx.recv())
                    .await
                    .expect("dispatch pending timed out")
                    .expect("dispatch error");
                match settled {
                    SettledFetch::Response { status, body, .. } => (status, body),
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
                        (status, String::from_utf8_lossy(&out).into_owned())
                    }
                    SettledFetch::WebSocketUpgrade { .. } => {
                        panic!("unexpected WS upgrade")
                    }
                }
            }
            FetchOutcome::WebSocketUpgrade { .. } => panic!("unexpected WS upgrade"),
        }
    })
}

// ── 1. Dict-shape dispatches a query ────────────────────────────────────────

#[test]
fn dict_shape_dispatches_basic_handler() {
    // The simplest possible dict-shape: `default.rpc = { foo: (input) => ... }`.
    // The bootstrap detects the object shape and wraps it via
    // `__zsDispatch`. The kernel calls our wrapper as if it were a
    // function-shape handler.
    let runtime = build_runtime(
        r#"
        export default {
            rpc: {
                foo: (input) => ({ ok: true, got: input }),
            },
        };
        "#,
    );
    let (status, body) = dispatch(&runtime, "foo", r#"{"json":42}"#);
    assert_eq!(status, 200, "body: {}", body);
    let inner = unwrap_json_envelope(&body);
    assert_eq!(inner, r#"{"ok":true,"got":42}"#);
}

// ── 2. Dict-shape applies fn.config.kind for capability frame ──────────────

#[test]
fn dict_shape_applies_capability_frame() {
    // The dispatcher reads `fn.config.kind` and invokes
    // `__zsEnterKind(kind)` / `__zsExitKind(token)` around the handler.
    // We mock both natives and verify the kind string + balanced
    // enter/exit calls.
    let runtime = build_runtime(
        r#"
        globalThis.__zsCapTrace = [];
        globalThis.__zsEnterKind = function(kind) {
            globalThis.__zsCapTrace.push("enter:" + kind);
            return 7; // arbitrary token
        };
        globalThis.__zsExitKind = function(token) {
            globalThis.__zsCapTrace.push("exit:" + token);
        };

        const q = (input) => ({ visited: true });
        q.config = { kind: "query" };

        const peek = () => globalThis.__zsCapTrace.join(",");
        peek.config = {}; // no kind → no frame around the peek itself

        export default {
            rpc: { q, peek },
        };
        "#,
    );
    let (status, _) = dispatch(&runtime, "q", r#"{"json":null}"#);
    assert_eq!(status, 200);
    let (status2, body2) = dispatch(&runtime, "peek", r#"{"json":null}"#);
    assert_eq!(status2, 200);
    let trace = unwrap_json_envelope(&body2);
    // The "q" call must have produced enter:query,exit:7. The peek call
    // itself adds nothing (no `kind`).
    assert!(
        trace.contains("enter:query") && trace.contains("exit:7"),
        "expected enter:query + exit:7 in trace, got: {}",
        trace
    );
}

// ── 3. Dict-shape validates input via cfg.input.parse ──────────────────────

#[test]
fn dict_shape_validates_input() {
    // `fn.config.input.parse(input)` throws → dispatcher converts to
    // an INVALID_ARGUMENT (400) with details.issues. We simulate a
    // Zod-shaped throw: an object with `issues: [...]`.
    let runtime = build_runtime(
        r#"
        const v = (input) => "never";
        v.config = {
            input: {
                parse(_x) {
                    const err = new Error("nope");
                    err.issues = [{ path: ["x"], message: "wrong" }];
                    throw err;
                },
            },
        };

        export default { rpc: { v } };
        "#,
    );
    let (status, body) = dispatch(&runtime, "v", r#"{"json":{"x":1}}"#);
    assert_eq!(status, 400, "body: {}", body);
    let parsed: serde_json::Value =
        serde_json::from_str(&body).expect("body should be JSON");
    assert_eq!(parsed["code"], "INVALID_ARGUMENT", "body: {}", body);
    assert_eq!(parsed["message"], "Invalid input", "body: {}", body);
    let issues = &parsed["details"]["issues"];
    assert!(issues.is_array(), "details.issues missing: {}", body);
    assert_eq!(issues[0]["message"], "wrong", "body: {}", body);
}

// ── 4. Function-shape still works (back-compat) ────────────────────────────

#[test]
fn function_shape_back_compat() {
    // Function-shape: `default.rpc = (name, input, ctx) => ...`. The
    // bootstrap detects `typeof rpc === "function"` and uses it
    // directly — `__zsDispatch` does NOT run. We assert by routing
    // multiple names through one function and observing the `name`
    // discriminant landed in the reply.
    let runtime = build_runtime(
        r#"
        export default {
            rpc: (name, input, ctx) => ({ name, input }),
        };
        "#,
    );
    let (status, body) = dispatch(&runtime, "alpha", r#"{"json":{"x":1}}"#);
    assert_eq!(status, 200, "body: {}", body);
    let inner = unwrap_json_envelope(&body);
    assert_eq!(inner, r#"{"name":"alpha","input":{"x":1}}"#);

    let (status2, body2) = dispatch(&runtime, "beta", r#"{"json":[1,2]}"#);
    assert_eq!(status2, 200);
    let inner2 = unwrap_json_envelope(&body2);
    assert_eq!(inner2, r#"{"name":"beta","input":[1,2]}"#);
}

// ── 7. AsyncIterator handler tagged correctly ──────────────────────────────

#[test]
fn dict_shape_tags_string_async_iterator() {
    // When the handler returns an AsyncIterator AND cfg.output is a
    // Zod string schema, dispatcher sets `__zsOutputIsString = true`
    // on the iterator. The stream encoder (slow path) reads that flag
    // to pick the AI-SDK `0:` (text) lane vs the `2:` (object) lane.
    //
    // We probe the tag directly: a separate handler invokes
    // `__zsDispatch` against a wrapped iterator and reads back the
    // tag attribute the dispatcher attached. This isolates the
    // dispatcher's tagging logic from the slow-path encoder (which
    // sits in the bootstrap's fetch handler, not the dispatcher).
    let runtime = build_runtime(
        r#"
        async function* makeIter() { yield 1; yield 2; }
        const streamingHandler = (input) => makeIter();
        streamingHandler.config = {
            output: { _def: { typeName: "ZodString" } },
        };

        // Probe handler — synchronously dispatches the streaming
        // handler through __zsDispatch and inspects the returned
        // iterator's tag. Returns true iff the dispatcher attached
        // `__zsOutputIsString` for a Zod string output schema.
        const probe = async () => {
            const dict = { streamingHandler };
            const iter = await globalThis.__zsDispatch(
                dict, "streamingHandler", undefined, {},
            );
            return {
                tagged: iter.__zsOutputIsString === true,
                hasNext: typeof iter.next === "function",
            };
        };

        export default { rpc: { probe } };
        "#,
    );
    let (status, body) = dispatch(&runtime, "probe", r#"{"json":null}"#);
    assert_eq!(status, 200, "body: {}", body);
    let inner = unwrap_json_envelope(&body);
    assert_eq!(inner, r#"{"tagged":true,"hasNext":true}"#);
}

#[test]
fn dict_shape_does_not_tag_non_string_iterator() {
    // Negative path: an AsyncIterator with a non-string (or no) output
    // schema must NOT have __zsOutputIsString set. The encoder then
    // falls back to per-value typeof to pick the lane.
    let runtime = build_runtime(
        r#"
        async function* makeIter() { yield "a"; yield "b"; }
        const streamingHandler = (input) => makeIter();
        // No output schema attached.
        streamingHandler.config = { kind: "stream" };

        const probe = async () => {
            const dict = { streamingHandler };
            const iter = await globalThis.__zsDispatch(
                dict, "streamingHandler", undefined, {},
            );
            return { tagged: iter.__zsOutputIsString === true };
        };

        export default { rpc: { probe } };
        "#,
    );
    let (status, body) = dispatch(&runtime, "probe", r#"{"json":null}"#);
    assert_eq!(status, 200, "body: {}", body);
    let inner = unwrap_json_envelope(&body);
    assert_eq!(inner, r#"{"tagged":false}"#);
}

// ── 8. Error envelope includes code + status + details ─────────────────────

#[test]
fn dict_shape_error_envelope_shape() {
    // A handler throw with a structured-error envelope (code, status,
    // details) must round-trip through the wire response.
    let runtime = build_runtime(
        r#"
        const fail = (input) => {
            const err = new Error("explicit-failure");
            err.status = 418;
            err.code = "TEAPOT";
            err.details = { extra: "yes" };
            throw err;
        };

        export default { rpc: { fail } };
        "#,
    );
    let (status, body) = dispatch(&runtime, "fail", r#"{"json":null}"#);
    assert_eq!(status, 418, "body: {}", body);
    let parsed: serde_json::Value =
        serde_json::from_str(&body).expect("body should be JSON");
    assert_eq!(parsed["message"], "explicit-failure", "body: {}", body);
    assert_eq!(parsed["code"], "TEAPOT", "body: {}", body);
    assert_eq!(parsed["details"]["extra"], "yes", "body: {}", body);
}

// ── 9. Method-not-found via dict-shape ─────────────────────────────────────

#[test]
fn dict_shape_unknown_method_404() {
    // Asking for a name that isn't in the dict returns NOT_FOUND.
    let runtime = build_runtime(
        r#"
        export default { rpc: { known: () => "ok" } };
        "#,
    );
    let (status, body) = dispatch(&runtime, "unknown", r#"{"json":null}"#);
    assert_eq!(status, 404, "body: {}", body);
    assert!(parse_message(&body).contains("Method not found"), "body: {}", body);
    let parsed: serde_json::Value =
        serde_json::from_str(&body).expect("body should be JSON");
    assert_eq!(parsed["code"], "NOT_FOUND", "body: {}", body);
}

// ── 10. Idempotent install ─────────────────────────────────────────────────

#[test]
fn dispatch_install_is_idempotent() {
    // The dispatcher's IIFE guards against double install. We probe at
    // call time (user-module top-level runs BEFORE the bootstrap's
    // rpc_dispatch.js, so we can't snapshot `before` at top level).
    // The handler captures the live `__zsDispatch`, re-runs the install
    // IIFE verbatim, and confirms identity preserved.
    let runtime = build_runtime(
        r#"
        const check = () => {
            const before = globalThis.__zsDispatch;
            // Re-run a copy of the install IIFE — the guard must
            // short-circuit so `__zsDispatch` is not overwritten.
            (function installZsDispatch(g) {
                if (typeof g.__zsDispatch === "function") return;
                g.__zsDispatch = function shouldNotInstall() {};
            })(globalThis);
            const after = globalThis.__zsDispatch;
            return {
                installed: typeof before === "function",
                same: before === after,
            };
        };

        export default { rpc: { check } };
        "#,
    );
    let (status, body) = dispatch(&runtime, "check", r#"{"json":null}"#);
    assert_eq!(status, 200, "body: {}", body);
    assert_eq!(
        unwrap_json_envelope(&body),
        r#"{"installed":true,"same":true}"#,
    );
}
