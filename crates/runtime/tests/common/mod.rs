#![allow(dead_code)]

use std::collections::HashMap;
use std::time::Duration;

use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, RequestResult, SettledFetch};
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::runtime::{Runtime, RuntimeLimits};

/// Create a module list from a single JS source string.
///
/// The caller writes `export function foo() {...}` style, and `m()` returns a
/// single-entry `index.js` module. The runtime itself injects a bootstrap
/// module that routes `POST /_rpc/<name>` to the user's named export, so tests
/// using [`dispatch`] can call their named exports as if they were RPC methods.
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

/// Extract `{status, body, logs}` from a `FetchOutcome`. Caller must be
/// inside a compio runtime (e.g. inside `block_on`) for Pending / Stream
/// variants to drive the pump.
async fn drive_fetch_outcome(outcome: FetchOutcome) -> (u16, String, Vec<String>) {
    match outcome {
        FetchOutcome::Response { status, body, logs, .. } => (status, body, logs),
        FetchOutcome::Stream { status, body_reader, logs, .. } => {
            // Most dispatch calls return Response.json(...) which collapses to
            // the Response arm; SSE (async-generator) tests intentionally
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
    let url = format!("http://localhost/_rpc/{}", url_path_encode(method));
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
    let runtime = Runtime::builder().modules(modules).build();
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
    let runtime = Runtime::builder().modules(modules).build();

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
    let runtime = Runtime::builder().modules(modules).env_vars(env_vars).build();
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

