//! Native procedure dispatch through the deployed RPC wire.

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
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body)
        && let Some(inner) = v.get("json")
    {
        return serde_json::to_string(inner).unwrap_or_else(|_| body.to_string());
    }
    body.to_string()
}

/// Returns `(status, body_string)` from a single RPC dispatch. The body
/// is JSON-decoded enough to peek at top-level fields; tests assert
/// against the raw envelope or the unwrapped `json` value.
fn request(runtime: &Runtime, name: &str, body: &str) -> FetchOutcome {
    runtime.call_fetch_handler(
        "POST", &format!("http://localhost/__zeroship/v1/{name}"),
        &[("content-type".into(), "application/json".into())], body,
        &EnvSnapshot::empty(), RequestCtx::new(CancelFlag::new()),
    )
}

fn dispatch(runtime: &Runtime, name: &str, body: &str) -> (u16, String) {
    compio::runtime::Runtime::new().unwrap().block_on(dispatch_async(runtime, name, body))
}

async fn dispatch_async(runtime: &Runtime, name: &str, body: &str) -> (u16, String) {
    let outcome = request(runtime, name, body);
    match outcome {
        FetchOutcome::Response {status, body, ..} => (status, String::from_utf8(body).unwrap()),
        FetchOutcome::Stream {status, body_reader, ..} => {
            runtime.start_pump();
            (status, read_stream(body_reader).await)
        }
        FetchOutcome::Pending {rx, ..} => {
            runtime.start_pump();
            let settled = compio::time::timeout(Duration::from_secs(5), rx.recv()).await
                .expect("dispatch deadline").expect("dispatch result");
            match settled {
                SettledFetch::Response {status, body, ..} => (status, String::from_utf8(body).unwrap()),
                SettledFetch::Stream {status, body_reader, ..} => (status, read_stream(body_reader).await),
                _ => panic!("unexpected WebSocket"),
            }
        }
        _ => panic!("unexpected WebSocket"),
    }
}

async fn read_stream(reader: zeroship_runtime::channel::StreamReader) -> String {
    compio::time::timeout(Duration::from_secs(5), async {
        let mut bytes = Vec::new();
        loop {
            while let Some(chunk) = reader.pop() { bytes.extend_from_slice(&chunk); }
            if reader.is_done() { break; }
            reader.wait_for_data().await;
        }
        assert!(!reader.is_overflow(), "stream must finish without buffer overflow");
        assert!(reader.error().is_none(), "stream transport failed");
        String::from_utf8(bytes).unwrap()
    }).await.expect("stream deadline")
}

// ── 1. Dict-shape dispatches a query ────────────────────────────────────────

#[test]
fn dict_shape_dispatches_basic_handler() {
    // The simplest possible dictionary shape: native dispatch resolves `foo`
    // by its wire name and calls the stored procedure reference.
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

#[test]
fn callable_rpc_is_rejected_at_startup() {
    let runtime = build_runtime(r#"
        export default { rpc: (name, input) => ({name, input}) };
    "#);
    let error = compio::runtime::Runtime::new().unwrap().block_on(runtime.initialize(&EnvSnapshot::empty()))
        .expect_err("callable RPC must not publish an alternate dispatcher");
    assert!(error.to_string().contains("procedure dictionary"), "{error}");
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

#[test]
fn dictionary_snapshots_own_string_ids_and_metadata() {
    let runtime = build_runtime(r#"
        const original = value => value;
        original.config = { input: { parse: value => value + 1 } };
        const rpc = Object.assign(Object.create({inherited: () => 'forbidden'}), {
            '123': original,
            'notes.list': original,
            constructor: () => 'own',
            change() {
                rpc['123'] = () => 'replacement';
                original.config.input.parse = () => 'replacement';
                return true;
            },
        });
        export default {rpc};
    "#);
    assert_eq!(dispatch(&runtime, "change", r#"{"json":null}"#).0, 200);
    for name in ["123", "notes.list"] {
        let (status, body) = dispatch(&runtime, name, r#"{"json":5}"#);
        assert_eq!(status, 200, "{body}");
        assert_eq!(unwrap_json_envelope(&body), "6");
    }
    assert_eq!(unwrap_json_envelope(&dispatch(&runtime, "constructor", "{}").1), "\"own\"");
    for name in ["inherited", "toString", "__proto__"] {
        assert_eq!(dispatch(&runtime, name, "{}").0, 404, "{name}");
    }
}

#[compio::test]
async fn lazy_procedure_retains_metadata_through_load_and_handler_suspension() {
    let runtime = build_runtime(r#"
        import {getRequestContext} from 'zeroship';
        let loads = 0;
        export default { rpc: {
            'notes.read': { async load() {
                loads++;
                await new Promise(resolve => setTimeout(resolve, 1));
                const handler = async (input, ctx) => {
                    await new Promise(resolve => setTimeout(resolve, 1));
                    return {input, sameContext: ctx === getRequestContext(), loads};
                };
                handler.config = {kind: 'query', input: {parse: value => value + 1}};
                return handler;
            }},
        }};
    "#);
    for input in [3, 7] {
        let (status, body) = dispatch_async(&runtime, "notes.read", &format!(r#"{{"json":{input}}}"#)).await;
        assert_eq!(status, 200, "{body}");
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["json"], serde_json::json!({"input":input+1,"sameContext":true,"loads":1}));
    }
}

#[test]
fn promised_frozen_iterator_is_invoked_once_with_native_context() {
    let runtime = build_runtime(r#"
        import {getRequestContext} from 'zeroship';
        let calls = 0;
        const stream = async (_, ctx) => {
            calls++;
            await new Promise(resolve => setTimeout(resolve, 1));
            let index = 0;
            return Object.freeze({
                [Symbol.asyncIterator]() { return this; },
                async next() {
                    await new Promise(resolve => setTimeout(resolve, 1));
                    if (ctx !== getRequestContext()) throw new Error('lost context');
                    return index++ ? {done:true} : {value:calls, done:false};
                },
            });
        };
        stream.config = {kind:'stream', outputIsString:true};
        export default {rpc:{stream}, fetch() { throw new Error('must not fall through'); }};
    "#);
    let (status, body) = dispatch(&runtime, "stream", "{}");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, "0:\"1\"\nd:{}\n");
}

#[test]
fn stream_error_preserves_structured_client_failure_and_terminal_frame() {
    let runtime = build_runtime(r#"
        async function* stream() {
            yield {started:true};
            await new Promise(resolve => setTimeout(resolve, 1));
            throw Object.assign(new Error('conflict'), {
                status:409, code:'CONFLICT', details:{key:'note'}, retryable:true,
            });
        }
        stream.config = {kind:'stream'};
        export default {rpc:{stream}};
    "#);
    let (status, body) = dispatch(&runtime, "stream", "{}");
    assert_eq!(status, 200);
    let lines: Vec<_> = body.lines().collect();
    assert_eq!(lines.first().copied(), Some("2:[{\"started\":true}]"));
    let error: serde_json::Value = serde_json::from_str(lines[1].strip_prefix("e:").unwrap()).unwrap();
    assert_eq!(error["code"], "CONFLICT");
    assert_eq!(error["details"]["key"], "note");
    assert_eq!(error["retryable"], true);
    assert_eq!(lines.last().copied(), Some("d:{}"));
}

#[test]
fn output_validation_uses_host_configuration() {
    let source = r#"
        globalThis.__zsValidateOutput = false;
        const value = () => 'result';
        value.config = { output: { parse() { throw new Error('bad output'); } } };
        export default {rpc:{value}};
    "#;
    let unchecked = build_runtime(source);
    assert_eq!(dispatch(&unchecked, "value", "{}").0, 200);
    let checked = Runtime::builder().modules(vec![ModuleEntry {
        specifier:"index.js".into(), source:source.into(),
    }]).validate_rpc_output(true).build();
    let (status, body) = dispatch(&checked, "value", "{}");
    assert_eq!(status, 500, "{body}");
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["message"], "internal error");
    assert!(body["request_id"].is_string());
}

#[compio::test]
async fn slow_stream_consumer_bounds_pulls_and_disconnect_returns_in_context() {
    let runtime = build_runtime(r#"
        import {getRequestContext} from 'zeroship';
        let pulls = 0, returned = false, sameContext = false;
        const stream = (_, ctx) => ({
            [Symbol.asyncIterator]() { return this; },
            next() { return {done: pulls++ >= 1000, value:'x'.repeat(65536)}; },
            async return() {
                await new Promise(resolve => setTimeout(resolve, 1));
                sameContext = ctx === getRequestContext();
                returned = true;
                return {done:true};
            },
        });
        stream.config = {kind:'stream'};
        export default {rpc:{stream, inspect:() => ({pulls,returned,sameContext})}};
    "#);
    let reader = match request(&runtime, "stream", "{}") {
        FetchOutcome::Stream {body_reader, ..} => body_reader,
        _ => panic!("synchronous iterator should produce a stream"),
    };
    runtime.start_pump();
    compio::time::sleep(Duration::from_millis(10)).await;
    let (_, body) = dispatch_async(&runtime, "inspect", "{}").await;
    let before: serde_json::Value = serde_json::from_str(&body).unwrap();
    let pulls = before["json"]["pulls"].as_u64().unwrap();
    assert!(pulls > 0 && pulls < 1000, "producer ran beyond demand: {body}");
    assert!(reader.buffered_bytes() <= reader.cap());
    assert!(!reader.is_done());
    drop(reader);
    compio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (_, body) = dispatch_async(&runtime, "inspect", "{}").await;
            let after: serde_json::Value = serde_json::from_str(&body).unwrap();
            if after["json"]["returned"] == true {
                assert_eq!(after["json"]["sameContext"], true);
                assert_eq!(after["json"]["pulls"], pulls);
                break;
            }
            compio::time::sleep(Duration::from_millis(1)).await;
        }
    }).await.expect("disconnect must close the original iterator");
}

#[compio::test]
async fn response_stream_retains_the_native_rpc_signal_after_headers() {
    use zeroship_runtime::rpc::abort;
    for promised in [false, true] {
        let app_id = zeroship_core::app_id::AppId::mint();
        let source = format!(r#"
            AbortController.prototype.abort = () => {{
                throw new Error('creator replaced controller.abort');
            }};
            let aborted = false;
            const make = (_, ctx) => {{
                ctx.signal.addEventListener('abort', () => {{ aborted = true; }});
                return {{
                    [Symbol.asyncIterator]() {{ return this; }},
                    next() {{ return new Promise(() => {{}}); }},
                    return() {{ return {{done:true}}; }},
                }};
            }};
            const stream = {handler};
            stream.config = {{kind:'stream'}};
            export default {{rpc:{{stream, inspect:() => aborted}}}};
        "#, handler = if promised {
            "async (...args) => { await new Promise(r => setTimeout(r,1)); return make(...args); }"
        } else { "make" });
        let runtime = Runtime::builder().app_id(app_id.clone()).modules(vec![ModuleEntry {
            specifier:"index.js".into(), source,
        }]).build();
        runtime.start_pump();
        let reader = match request(&runtime, "stream", "{}") {
            FetchOutcome::Stream {body_reader, ..} => body_reader,
            FetchOutcome::Pending {rx, ..} => {
                match compio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap() {
                    SettledFetch::Stream {body_reader, ..} => body_reader,
                    _ => panic!("expected promised stream"),
                }
            }
            _ => panic!("expected stream"),
        };
        assert_eq!(abort::entries_for_app(&app_id), 1);
        runtime.with_scope(|scope| abort::entered_for_eviction(scope, &app_id));
        let (status, body) = dispatch_async(&runtime, "inspect", "{}").await;
        assert_eq!(status, 200);
        assert_eq!(unwrap_json_envelope(&body), "true");
        drop(reader);
        assert_eq!(abort::entries_for_app(&app_id), 0);
    }
}

#[test]
fn rpc_signals_ignore_replaced_javascript_constructors() {
    for eager in [false, true] {
        let source = r#"
            import { currentSignal } from 'zeroship';
            const NativeAbortSignal = AbortSignal;
            for (const name of ['AbortController', 'AbortSignal']) {
                Object.defineProperty(globalThis, name, {
                    configurable: true,
                    get() { throw new Error(`creator replaced ${name}`); },
                });
            }
            let previous;
            export default {rpc: {inspect: (_, ctx) => {
                const current = ctx.signal;
                const result = {
                    native: current instanceof NativeAbortSignal,
                    same: current === ctx.signal && current === currentSignal(),
                    fresh: current !== previous,
                    aborted: current.aborted,
                    noReason: current.reason === undefined,
                };
                current.throwIfAborted();
                previous = current;
                return result;
            }}};
        "#;
        let mut builder = Runtime::builder().modules(vec![ModuleEntry {
            specifier: "index.js".into(), source: source.into(),
        }]);
        if eager {
            builder = builder.app_id(zeroship_core::app_id::AppId::mint());
        }
        let runtime = builder.build();
        for _ in 0..2 {
            let (status, body) = dispatch(&runtime, "inspect", "{}");
            assert_eq!(status, 200, "{body}");
            let value: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(value["json"], serde_json::json!({
                "native": true, "same": true, "fresh": true,
                "aborted": false, "noReason": true,
            }));
        }
    }
}
