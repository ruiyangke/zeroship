//! Stage 5a — embedded RPC dispatcher tests.
//!
//! Covers both dispatch shapes the runtime now accepts on
//! `user.default.rpc`:
//!
//!   - Dict-shape `{ [wireId]: handler }` (Stage 5a, new). The
//!     bootstrap wraps the dict in an internal dispatcher which owns:
//!       * input validation via `fn.config.input.parse()`
//!       * capability frame via hidden native kind callbacks
//!       * AsyncIterator stream framing (`__zsOutputIsString`)
//!       * dev-only output validation via `__zsValidateOutput`
//!   - Function-shape `(name, input, ctx) => ...` (legacy back-compat).
//!     The bootstrap uses the function directly; the internal dispatcher does
//!     NOT run.
//!
//! Tests drive the kernel via `call_fetch_handler` against the spec
//! wire (`POST /__zeroship/v1/<id>` with `{"json":<input>}`) so the full
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
    let url = format!("http://localhost/__zeroship/v1/{}", name);
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

// ── 2. Dict-shape hides dispatcher/kind globals ────────────────────────────

#[test]
fn dict_shape_hides_dispatcher_and_kind_globals() {
    let runtime = build_runtime(
        r#"
        const peek = () => ({
            dispatch: typeof globalThis.__zsDispatch,
            enter: typeof globalThis.__zsEnterKind,
            exit: typeof globalThis.__zsExitKind,
            clear: typeof globalThis.__zsClearKind,
        });
        peek.config = { kind: "action" };

        export default {
            rpc: { peek },
        };
        "#,
    );
    let (status, body) = dispatch(&runtime, "peek", r#"{"json":null}"#);
    assert_eq!(status, 200, "body: {}", body);
    assert_eq!(
        unwrap_json_envelope(&body),
        r#"{"dispatch":"undefined","enter":"undefined","exit":"undefined","clear":"undefined"}"#,
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

// ── 7. Dispatcher globals are not creator-callable ─────────────────────────

#[test]
fn dict_shape_dispatch_global_is_not_installed() {
    let runtime = build_runtime(
        r#"
        const probe = () => typeof globalThis.__zsDispatch;

        export default { rpc: { probe } };
        "#,
    );
    let (status, body) = dispatch(&runtime, "probe", r#"{"json":null}"#);
    assert_eq!(status, 200, "body: {}", body);
    assert_eq!(unwrap_json_envelope(&body), r#""undefined""#);
}

#[test]
fn dict_shape_kind_globals_are_not_installed() {
    let runtime = build_runtime(
        r#"
        const probe = () => [
            typeof globalThis.__zsEnterKind,
            typeof globalThis.__zsExitKind,
            typeof globalThis.__zsClearKind,
        ].join("/");

        export default { rpc: { probe } };
        "#,
    );
    let (status, body) = dispatch(&runtime, "probe", r#"{"json":null}"#);
    assert_eq!(status, 200, "body: {}", body);
    assert_eq!(unwrap_json_envelope(&body), r#""undefined/undefined/undefined""#);
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

// ── 10. Creator cannot reinstall dispatch global ───────────────────────────

#[test]
fn creator_defined_dispatch_global_does_not_affect_runtime_dispatch() {
    let runtime = build_runtime(
        r#"
        globalThis.__zsDispatch = function forged() {
            throw new Error("forged dispatch should not run");
        };
        const check = () => {
            return {
                visible: typeof globalThis.__zsDispatch,
                ok: true,
            };
        };

        export default { rpc: { check } };
        "#,
    );
    let (status, body) = dispatch(&runtime, "check", r#"{"json":null}"#);
    assert_eq!(status, 200, "body: {}", body);
    assert_eq!(
        unwrap_json_envelope(&body),
        r#"{"visible":"function","ok":true}"#,
    );
}
