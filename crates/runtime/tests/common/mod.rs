#![allow(dead_code)]

use std::collections::HashMap;
use std::time::Duration;

use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, RequestResult, SettledFetch};
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::runtime::{Runtime, DispatchOutcome, RuntimeLimits};

/// Create a module list from a single JS source string.
///
/// The caller writes `export function foo() {...}` style, and `m()` returns a
/// single-entry `index.js` module. Tests that need the old `dispatch_rpc`-style
/// named-export lookup go through [`dispatch`], which wraps this in a second
/// module that exposes a default fetch handler on top of the user's exports.
pub fn m(source: &str) -> Vec<ModuleEntry> {
    vec![ModuleEntry {
        specifier: "index.js".into(),
        source: source.into(),
    }]
}

/// Shorthand: empty env vars for tests.
pub fn no_env() -> HashMap<String, String> {
    HashMap::new()
}

/// Bootstrap module that replicates the pre-kernel-cut `DISPATCH_JS` contract
/// on top of `call_fetch_handler`.
///
/// Imports everything from the user's entry as `user.*`, then re-exports a
/// module-worker default whose `fetch` handler:
///
/// - extracts the method name from the URL path (`/<method>`),
/// - reads args as a JSON array from the request body,
/// - calls `user[method].apply(null, args)`,
/// - JSON-serializes the return value into a `Response` (or passes through a
///   `Response`, or SSE-wraps an async generator — matching the old contract).
///
/// This lets crypto/url/stream/env/etc. tests keep their concise
/// `export function x() { ... }` shape without each file having to hand-write
/// a fetch wrapper.
const DISPATCH_BOOTSTRAP_JS: &str = r#"
import * as user from "./user.js";

function wrapAsyncGenerator(gen) {
    var encoder = new TextEncoder();
    var body = new ReadableStream({
        async start(controller) {
            try {
                while (true) {
                    var step = await gen.next();
                    if (step.done) {
                        var retJson = JSON.stringify(step.value === undefined ? null : step.value);
                        controller.enqueue(encoder.encode("event: return\ndata: " + retJson + "\n\n"));
                        break;
                    }
                    var valJson = JSON.stringify(step.value === undefined ? null : step.value);
                    controller.enqueue(encoder.encode("event: yield\ndata: " + valJson + "\n\n"));
                }
            } catch (e) {
                var payload = JSON.stringify({
                    message: (e && e.message) || String(e),
                    name: (e && e.name) || "Error",
                });
                controller.enqueue(encoder.encode("event: error\ndata: " + payload + "\n\n"));
            } finally {
                controller.close();
            }
        },
    });
    return new Response(body, {
        status: 200,
        headers: {
            "Content-Type": "text/event-stream",
            "Cache-Control": "no-cache, no-transform",
            "X-Accel-Buffering": "no",
        },
    });
}

export default {
    async fetch(request, env, ctx) {
        var url = new URL(request.url);
        var method = decodeURIComponent(url.pathname.replace(/^\//, ""));
        var fn = user[method];
        if (typeof fn !== 'function') {
            var err = new Error('Method not found: ' + method);
            err.status = 404;
            throw err;
        }

        var bodyText = "";
        try { bodyText = await request.text(); } catch (_) {}
        var args;
        if (!bodyText) {
            args = [];
        } else {
            var parsed;
            try { parsed = JSON.parse(bodyText); }
            catch (_e) {
                var err = new Error('Invalid args JSON');
                err.status = 400;
                throw err;
            }
            if (parsed == null) args = [];
            else if (Array.isArray(parsed)) args = parsed;
            else {
                var err = new Error('RPC args body must be a JSON array');
                err.status = 400;
                throw err;
            }
        }

        var result = fn.apply(null, args);
        if (result && typeof result.then === 'function') result = await result;

        if (result instanceof Response) return result;
        if (result != null && typeof result === 'object'
            && typeof result[Symbol.asyncIterator] === 'function'
            && typeof result.next === 'function'
            && typeof result.return === 'function') {
            return wrapAsyncGenerator(result);
        }
        return new Response(
            JSON.stringify(result === undefined ? null : result),
            { status: 200, headers: { "Content-Type": "application/json" } }
        );
    },
};
"#;

/// Wrap a single-file user module in the DISPATCH_BOOTSTRAP_JS bootstrap so
/// the named-export + JSON args + JSON response contract of the old
/// `dispatch_rpc` tests rides on top of `call_fetch_handler`.
fn wrap_with_dispatch_bootstrap(modules: Vec<ModuleEntry>) -> Vec<ModuleEntry> {
    // Move the user's entry aside to ./user.js; synthesize a new index.js
    // that imports from it and re-exports a default fetch handler. The rest
    // of the user modules (if any) are passed through unchanged; their
    // specifiers must not be "user.js" or "index.js".
    let mut out: Vec<ModuleEntry> = Vec::with_capacity(modules.len() + 1);
    out.push(ModuleEntry {
        specifier: "index.js".into(),
        source: DISPATCH_BOOTSTRAP_JS.into(),
    });

    for (i, entry) in modules.into_iter().enumerate() {
        if i == 0 {
            // The first module is the entrypoint — rename to user.js.
            out.push(ModuleEntry {
                specifier: "user.js".into(),
                source: entry.source,
            });
        } else {
            out.push(entry);
        }
    }
    out
}

/// Extract `{status, body, logs}` from a `FetchOutcome`. Caller must be
/// inside a compio runtime (e.g. inside `block_on`) for Pending / Stream
/// variants to drive the pump.
async fn drive_fetch_outcome(outcome: FetchOutcome) -> (u16, String, Vec<String>) {
    match outcome {
        FetchOutcome::Response { status, body, logs, .. } => (status, body, logs),
        FetchOutcome::Stream { status, body_reader, logs, .. } => {
            // For dispatch_rpc tests the response was fully buffered by the
            // DISPATCH_JS path; Response(JSON.stringify(...)) should collapse
            // to the Response arm, but SSE (async-generator) tests intentionally
            // return a stream. Drain once synchronously — good enough for the
            // fully-buffered-at-send-time case.
            let mut body = Vec::new();
            for chunk in body_reader.drain() {
                body.extend_from_slice(&chunk);
            }
            (status, String::from_utf8_lossy(&body).into_owned(), logs)
        }
        FetchOutcome::Pending { rx, cancel: _ } => {
            let settled = compio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("fetch pending timed out")
                .expect("fetch pending delivered DispatchError");
            match settled {
                SettledFetch::Response { status, body, logs, .. } => (status, body, logs),
                SettledFetch::Stream { status, body_reader, logs, .. } => {
                    let mut body = Vec::new();
                    for chunk in body_reader.drain() {
                        body.extend_from_slice(&chunk);
                    }
                    (status, String::from_utf8_lossy(&body).into_owned(), logs)
                }
                SettledFetch::WebSocketUpgrade { .. } => {
                    panic!("unexpected WebSocketUpgrade in dispatch helper")
                }
            }
        }
        FetchOutcome::WebSocketUpgrade { .. } => {
            panic!("unexpected WebSocketUpgrade in dispatch helper")
        }
    }
}

/// Dispatch a single RPC-style call and block on its result.
///
/// Handles both sync handlers (no compio runtime needed) and async ones
/// (spins up a compio runtime just for the await). Mirrors the old
/// `dispatch_rpc` contract: JSON string body on 2xx, Err(message) on 4xx/5xx.
fn run_dispatch_on_runtime(runtime: &Runtime, method: &str, args_json: &str) -> Result<RequestResult, String> {
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let body = if args_json.is_empty() { "".to_string() } else { args_json.to_string() };
    let url = format!("http://localhost/{}", url_path_encode(method));
    let outcome = runtime.call_fetch_handler(
        "POST",
        &url,
        &[("content-type".into(), "application/json".into())],
        &body,
        &env,
        ctx,
    );

    // Sync path: Response variant returns immediately — no compio needed.
    if let FetchOutcome::Response { status, body: json_body, logs, .. } = &outcome {
        let (status, json_body, logs) = (*status, json_body.clone(), logs.clone());
        if !(200..300).contains(&status) {
            return Err(parse_error_message(&json_body));
        }
        return Ok(RequestResult {
            json: json_body,
            cpu_time: Duration::ZERO,
            wall_time: Duration::ZERO,
            logs,
        });
    }

    // Non-Response outcomes (Stream / Pending / WebSocketUpgrade) need the
    // compio runtime. Spin one up just for this call — cheap because we
    // immediately block_on it.
    let (status, json_body, logs) = compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();
        drive_fetch_outcome(outcome).await
    });
    if !(200..300).contains(&status) {
        return Err(parse_error_message(&json_body));
    }
    Ok(RequestResult {
        json: json_body,
        cpu_time: Duration::ZERO,
        wall_time: Duration::ZERO,
        logs,
    })
}

/// Invoke `method(args)` on the user's module through the fetch-handler
/// bootstrap. Matches the pre-kernel-cut `dispatch_rpc` contract (returns a
/// JSON string + the status→Err mapping for 4xx/5xx errors).
pub fn dispatch(modules: Vec<ModuleEntry>, method: &str, args_json: &str) -> Result<RequestResult, String> {
    init_v8();
    let wrapped = wrap_with_dispatch_bootstrap(modules);
    let runtime = Runtime::builder().modules(wrapped).build();
    run_dispatch_on_runtime(&runtime, method, args_json)
}

/// URL-encode a single path segment. The bootstrap's JS side uses
/// `decodeURIComponent`, so any `%xx` we emit here round-trips.
fn url_path_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let c = b as char;
        // Alphanumeric + unreserved path chars per RFC 3986.
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '/') {
            out.push(c);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// Extract a usable "message" string from a JSON error body. Falls back to
/// the raw body if it's not parseable.
fn parse_error_message(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(msg) = v.get("message").and_then(|m| m.as_str()) {
            return msg.to_string();
        }
    }
    body.to_string()
}

/// Create a Runtime, dispatch multiple requests sequentially — the Runtime
/// is shared so per-module state (e.g. closure-captured counters) persists
/// between invocations.
pub fn dispatch_multi(
    modules: Vec<ModuleEntry>,
    requests: &[(&str, &str)],
) -> Vec<Result<RequestResult, String>> {
    init_v8();
    let wrapped = wrap_with_dispatch_bootstrap(modules);
    let runtime = Runtime::builder().modules(wrapped).build();

    requests
        .iter()
        .map(|(method, args)| run_dispatch_on_runtime(&runtime, method, args))
        .collect()
}

/// Like [`dispatch`] but with caller-supplied env vars.
pub fn dispatch_with_env(
    modules: Vec<ModuleEntry>,
    env_vars: HashMap<String, String>,
    method: &str,
    args_json: &str,
) -> Result<RequestResult, String> {
    init_v8();
    let wrapped = wrap_with_dispatch_bootstrap(modules);
    let runtime = Runtime::builder().modules(wrapped).env_vars(env_vars).build();
    run_dispatch_on_runtime(&runtime, method, args_json)
}

/// Helper: feed an HTTP request straight through `call_fetch_handler`.
///
/// Previously this called the obsolete `dispatch_http`; now it just lifts
/// headers into the `call_fetch_handler` contract. Returns `None` for the
/// "no handler" case to mirror the old "no onRequest" behavior (for tests
/// that negate the presence of a handler).
pub fn dispatch_http_sync(
    modules: Vec<ModuleEntry>,
    method: &str,
    url: &str,
    headers_json: &str,
    body: &str,
) -> Option<(u16, Vec<(String, String)>, String)> {
    init_v8();
    let runtime = Runtime::builder().modules(modules).build();

    let headers: Vec<(String, String)> = serde_json::from_str(headers_json).unwrap_or_default();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler(method, url, &headers, body, &env, ctx);

    // Sync path: Response variant returns immediately.
    if let FetchOutcome::Response { status, headers, body, .. } = &outcome {
        if *status == 404 && body.contains("No default.fetch handler") {
            return None;
        }
        return Some((*status, headers.clone(), body.clone()));
    }

    // Otherwise spin up a compio runtime and drive through the pump.
    Some(compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();
        match outcome {
            FetchOutcome::Response { status, headers, body, .. } => (status, headers, body),
            FetchOutcome::Stream { status, headers, body_reader, .. } => {
                let mut out = Vec::new();
                for chunk in body_reader.drain() {
                    out.extend_from_slice(&chunk);
                }
                (status, headers, String::from_utf8_lossy(&out).into_owned())
            }
            FetchOutcome::Pending { rx, cancel: _ } => {
                let settled = compio::time::timeout(Duration::from_secs(5), rx.recv())
                    .await
                    .expect("dispatch_http_sync pending timed out")
                    .expect("dispatch_http_sync pending delivered DispatchError");
                match settled {
                    SettledFetch::Response { status, headers, body, .. } => (status, headers, body),
                    SettledFetch::Stream { status, headers, body_reader, .. } => {
                        let mut out = Vec::new();
                        for chunk in body_reader.drain() {
                            out.extend_from_slice(&chunk);
                        }
                        (status, headers, String::from_utf8_lossy(&out).into_owned())
                    }
                    SettledFetch::WebSocketUpgrade { .. } => {
                        panic!("unexpected WebSocketUpgrade in dispatch_http_sync")
                    }
                }
            }
            FetchOutcome::WebSocketUpgrade { .. } => {
                panic!("unexpected WebSocketUpgrade in dispatch_http_sync")
            }
        }
    }))
}

// Keep limits alias accessible from tests in case a future test wants it.
#[allow(dead_code)]
pub fn default_limits() -> RuntimeLimits {
    RuntimeLimits::default()
}

/// HTTP request to feed `call_fetch_handler` — the new kernel primitive.
pub struct TestRequest {
    pub method: &'static str,
    pub url: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl TestRequest {
    pub fn get(url: &'static str) -> Self {
        Self { method: "GET", url, headers: vec![], body: String::new() }
    }
    pub fn post_json(url: &'static str, body: impl Into<String>) -> Self {
        Self {
            method: "POST",
            url,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.into(),
        }
    }
}

/// Build a Runtime + call `call_fetch_handler` once synchronously.
/// Returns the outcome as-is; tests destructure.
pub fn dispatch_fetch(modules: Vec<ModuleEntry>, req: TestRequest) -> FetchOutcome {
    init_v8();
    let runtime = Runtime::builder().modules(modules).build();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    runtime.call_fetch_handler(
        req.method,
        req.url,
        &req.headers,
        &req.body,
        &env,
        ctx,
    )
}

/// Silence the never-used warning for DispatchOutcome alias (tests import it
/// by name; the D2 removal deletes this alias entirely).
#[allow(dead_code)]
fn _keep_dispatch_outcome_alias_alive() -> Option<DispatchOutcome> { None }
