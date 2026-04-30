#![allow(dead_code)]

use std::collections::HashMap;
use std::time::Duration;

use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, RequestResult, SettledFetch};
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::runtime::{Runtime, RuntimeLimits};

/// Create a module list from a single JS source string.
///
/// The caller writes `export function foo() {...}` style, and `m()` returns a
/// single-entry `index.js` module. After the WinterCG-symmetric refactor, the
/// runtime no longer auto-routes `POST /_rpc/<name>` to named exports — every
/// HTTP request flows through `default.fetch`. Tests using [`dispatch`]
/// synthesize a tiny `default.{fetch, rpc}` shim around the user code (see
/// `dispatch` for the wrapping logic).
pub fn m(source: &str) -> Vec<ModuleEntry> {
    vec![ModuleEntry {
        specifier: "index.js".into(),
        source: source.into(),
    }]
}

/// Wrap a user-supplied JS source (with bare `export function name(...)`
/// declarations) in a tiny synthetic-entry shim that exposes
/// `default.{fetch, rpc}` per the WinterCG-symmetric contract.
///
/// The shim:
///   - Rebuilds a name → fn lookup at module top by walking `globalThis`-
///     stashed bindings — since we control the harness we just inline a
///     literal `_procedures` map after the user code (see usage below).
///   - Implements `_zsFetch(request)` for `/_zs/v1/<id>` URLs (POST body is
///     `{"json": <input>}`).
///   - Implements `_zsRpc(name, input, ctx)` that throws 404 NOT_FOUND on
///     missing keys.
///
/// `procs_block` must be a JS expression like `{ ping, count }` that
/// references the user's named exports. The shim exports
/// `default = { fetch: _zsFetch, rpc: _zsRpc }`.
pub fn wrap_with_synthetic_entry(user_source: &str, procs_block: &str) -> Vec<ModuleEntry> {
    let src = format!(
        r#"
{user}
const _procedures = {procs};
async function _zsRpc(name, input, _ctx) {{
    const fn = _procedures[name];
    if (typeof fn !== "function") {{
        throw Object.assign(new Error("Method not found: " + name), {{ status: 404, code: "NOT_FOUND" }});
    }}
    return await fn(input);
}}
async function _zsRpcAndRespond(name, input) {{
    try {{
        const result = await _zsRpc(name, input);
        if (result != null && typeof result === "object"
            && typeof result[Symbol.asyncIterator] === "function"
            && typeof result.next === "function") {{
            const enc = new TextEncoder();
            const body = new ReadableStream({{
                async start(controller) {{
                    try {{
                        while (true) {{
                            const step = await result.next();
                            if (step.done) {{ controller.enqueue(enc.encode("d:{{}}\n")); break; }}
                            const v = step.value;
                            if (typeof v === "string") {{
                                controller.enqueue(enc.encode("0:" + JSON.stringify(v) + "\n"));
                            }} else {{
                                controller.enqueue(enc.encode("2:[" + JSON.stringify(v) + "]\n"));
                            }}
                        }}
                    }} catch (e) {{
                        const env = {{ message: e?.message ?? String(e), name: e?.name ?? "Error" }};
                        if (e && typeof e.code === "string") env.code = e.code;
                        if (e && e.details !== undefined) env.details = e.details;
                        if (e && typeof e.retryable === "boolean") env.retryable = e.retryable;
                        controller.enqueue(enc.encode("e:" + JSON.stringify(env) + "\n"));
                        controller.enqueue(enc.encode("d:{{}}\n"));
                    }} finally {{ controller.close(); }}
                }},
            }});
            return new Response(body, {{
                status: 200,
                headers: {{ "content-type": "text/event-stream", "cache-control": "no-cache" }},
            }});
        }}
        if (result instanceof Response) return result;
        return new Response(JSON.stringify({{ json: result === undefined ? null : result }}), {{
            status: 200, headers: {{ "content-type": "application/json" }},
        }});
    }} catch (err) {{
        const status = (err && Number.isInteger(err.status) && err.status >= 400 && err.status < 600) ? err.status : 500;
        const body = {{ message: err?.message ?? String(err), name: err?.name ?? "Error" }};
        if (err && typeof err.code === "string") body.code = err.code;
        if (err && err.details !== undefined) body.details = err.details;
        if (err && typeof err.retryable === "boolean") body.retryable = err.retryable;
        return new Response(JSON.stringify(body), {{
            status, headers: {{ "content-type": "application/json" }},
        }});
    }}
}}
async function _zsFetch(request) {{
    const url = new URL(request.url);
    if (!url.pathname.startsWith("/_zs/v1/")) {{
        return new Response("Not Found", {{ status: 404 }});
    }}
    const id = decodeURIComponent(url.pathname.slice("/_zs/v1/".length));
    let input = undefined;
    if (request.method === "POST") {{
        const text = await request.text();
        if (text) {{
            try {{
                const env = JSON.parse(text);
                input = env && typeof env === "object" && "json" in env ? env.json : env;
            }} catch (e) {{
                return new Response(JSON.stringify({{
                    message: "invalid JSON body: " + (e?.message ?? e),
                    name: "Error", code: "INVALID_ARGUMENT",
                }}), {{ status: 400, headers: {{ "content-type": "application/json" }} }});
            }}
        }}
    }}
    return await _zsRpcAndRespond(id, input);
}}
export default {{ fetch: _zsFetch, rpc: _zsRpc }};
"#,
        user = user_source,
        procs = procs_block,
    );
    vec![ModuleEntry {
        specifier: "index.js".into(),
        source: src,
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
    // Spec wire: POST /_zs/v1/<id> with body { json: <input> }. The
    // legacy `dispatch` contract takes args as a JSON array (e.g.
    // `[3, 4]`); we wrap that as the `input` value (the synthetic-entry
    // shim spreads it over the handler's arguments at call time).
    let body = if args_json.is_empty() {
        "".to_string()
    } else {
        format!(r#"{{"json":{}}}"#, args_json)
    };
    let url = format!("http://localhost/_zs/v1/{}", url_path_encode(method));
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
            json: unwrap_json_envelope(&json_body),
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
        json: unwrap_json_envelope(&json_body),
        cpu_time: Duration::ZERO,
        wall_time: Duration::ZERO,
        logs,
    })
}

/// Strip the `{ "json": ... }` envelope from a 200-OK body. Streaming
/// (`text/event-stream`) bodies are returned verbatim — the SSE wire is
/// already its own framing. Falls back to the raw body if it's not a
/// JSON object with a `json` key.
fn unwrap_json_envelope(body: &str) -> String {
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(inner) = map.get("json") {
            if let Some(s) = inner.as_str() {
                // Re-stringify so callers see a JSON string.
                return format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
            }
            return serde_json::to_string(inner).unwrap_or_else(|_| body.to_string());
        }
    }
    body.to_string()
}

/// Invoke `method(args)` on the user's module through the synthetic
/// entry over the spec wire. Returns a JSON string + the status→Err
/// mapping for 4xx/5xx errors.
///
/// User code uses `export function NAME(...)` declarations; this helper
/// auto-detects exports via a regex over the source and wraps them in a
/// synthetic-entry shim that exposes `default.{fetch, rpc}`.
///
/// Calling convention: `args_json` is a JSON array (`[a, b, ...]`). The
/// shim spreads the array over the handler's args, matching the legacy
/// dispatch contract: `dispatch(m, "add", "[3,4]")` → `add(3, 4)`.
pub fn dispatch(modules: Vec<ModuleEntry>, method: &str, args_json: &str) -> Result<RequestResult, String> {
    init_v8();
    let wrapped = wrap_user_modules_for_legacy_dispatch(modules);
    let runtime = Runtime::builder().modules(wrapped).build();
    run_dispatch_on_runtime(&runtime, method, args_json)
}

/// Detect every named export in a JS source string and wrap the modules
/// in a synthetic-entry shim that exposes `default.{fetch, rpc}` per
/// the WinterCG-symmetric contract.
///
/// The shim's `_zsRpc(name, input, ctx)` looks up the procedure by name
/// and spreads `input` (a JS array, per the legacy dispatch convention)
/// over the handler's arguments — so user code can keep writing
/// `function add(a, b) { ... }` without rewriting their signatures.
fn wrap_user_modules_for_legacy_dispatch(modules: Vec<ModuleEntry>) -> Vec<ModuleEntry> {
    if modules.is_empty() { return modules; }
    let entry = &modules[0];
    let names = extract_exported_names(&entry.source);
    let procs_block = if names.is_empty() {
        "{}".to_string()
    } else {
        format!("{{ {} }}", names.join(", "))
    };
    let shim = format!(
        r#"
{user}
const _procedures = {procs};
async function _zsRpc(name, input, _ctx) {{
    const fn = _procedures[name];
    if (typeof fn !== "function") {{
        throw Object.assign(new Error("Method not found: " + name), {{ status: 404, code: "NOT_FOUND" }});
    }}
    const args = Array.isArray(input) ? input : [];
    return await fn.apply(null, args);
}}
async function _zsRpcAndRespond(name, input) {{
    try {{
        const result = await _zsRpc(name, input);
        if (result != null && typeof result === "object"
            && typeof result[Symbol.asyncIterator] === "function"
            && typeof result.next === "function") {{
            const enc = new TextEncoder();
            const body = new ReadableStream({{
                async start(controller) {{
                    try {{
                        while (true) {{
                            const step = await result.next();
                            if (step.done) {{ controller.enqueue(enc.encode("d:{{}}\n")); break; }}
                            const v = step.value;
                            if (typeof v === "string") {{
                                controller.enqueue(enc.encode("0:" + JSON.stringify(v) + "\n"));
                            }} else {{
                                controller.enqueue(enc.encode("2:[" + JSON.stringify(v) + "]\n"));
                            }}
                        }}
                    }} catch (e) {{
                        const env = {{ message: e?.message ?? String(e), name: e?.name ?? "Error" }};
                        if (e && typeof e.code === "string") env.code = e.code;
                        if (e && e.details !== undefined) env.details = e.details;
                        if (e && typeof e.retryable === "boolean") env.retryable = e.retryable;
                        controller.enqueue(enc.encode("e:" + JSON.stringify(env) + "\n"));
                        controller.enqueue(enc.encode("d:{{}}\n"));
                    }} finally {{ controller.close(); }}
                }},
            }});
            return new Response(body, {{
                status: 200,
                headers: {{ "content-type": "text/event-stream", "cache-control": "no-cache" }},
            }});
        }}
        if (result instanceof Response) return result;
        return new Response(JSON.stringify({{ json: result === undefined ? null : result }}), {{
            status: 200, headers: {{ "content-type": "application/json" }},
        }});
    }} catch (err) {{
        const status = (err && Number.isInteger(err.status) && err.status >= 400 && err.status < 600) ? err.status : 500;
        const body = {{ message: err?.message ?? String(err), name: err?.name ?? "Error" }};
        if (err && typeof err.code === "string") body.code = err.code;
        if (err && err.details !== undefined) body.details = err.details;
        if (err && typeof err.retryable === "boolean") body.retryable = err.retryable;
        return new Response(JSON.stringify(body), {{
            status, headers: {{ "content-type": "application/json" }},
        }});
    }}
}}
async function _zsFetch(request) {{
    const url = new URL(request.url);
    if (!url.pathname.startsWith("/_zs/v1/")) {{
        return new Response("Not Found", {{ status: 404 }});
    }}
    const id = decodeURIComponent(url.pathname.slice("/_zs/v1/".length));
    let input = undefined;
    if (request.method === "POST") {{
        const text = await request.text();
        if (text) {{
            try {{
                const env = JSON.parse(text);
                input = env && typeof env === "object" && "json" in env ? env.json : env;
            }} catch (_e) {{
                return new Response(JSON.stringify({{ message: "invalid JSON body", name: "Error", code: "INVALID_ARGUMENT" }}), {{
                    status: 400, headers: {{ "content-type": "application/json" }},
                }});
            }}
        }}
    }}
    return await _zsRpcAndRespond(id, input);
}}
export default {{ fetch: _zsFetch, rpc: _zsRpc }};
"#,
        user = entry.source,
        procs = procs_block,
    );
    let mut out = vec![ModuleEntry {
        specifier: entry.specifier.clone(),
        source: shim,
    }];
    out.extend(modules.into_iter().skip(1));
    out
}

/// Walk the source for `export function NAME`, `export async function NAME`,
/// `export async function* NAME`, and `export const NAME = ...`. Returns
/// the discovered names in source order.
///
/// This is a coarse string scan that handles the test patterns; it
/// doesn't try to handle every ES grammar shape. Tests that don't fit
/// (re-exports, namespace exports) should use `wrap_with_synthetic_entry`
/// directly.
fn extract_exported_names(src: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Find next "export"
        let Some(rel) = src[i..].find("export") else { break; };
        let abs = i + rel;
        // Word boundary on the left: byte before must be non-identifier.
        let left_ok = abs == 0 || {
            let c = bytes[abs - 1];
            !is_ident_char(c)
        };
        i = abs + "export".len();
        if !left_ok { continue; }
        // Skip whitespace.
        let mut j = i;
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\n' || bytes[j] == b'\r') { j += 1; }
        // Optional "async ".
        if src[j..].starts_with("async") {
            let k = j + "async".len();
            if k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\t' || bytes[k] == b'\n' || bytes[k] == b'\r') {
                j = k;
                while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\n' || bytes[j] == b'\r') { j += 1; }
            }
        }
        // Now must be "function", "const", "let", "var".
        let kind = if src[j..].starts_with("function") { "function" }
                   else if src[j..].starts_with("const") { "const" }
                   else if src[j..].starts_with("let") { "let" }
                   else if src[j..].starts_with("var") { "var" }
                   else { continue; };
        j += kind.len();
        // For "function", optional "*" then whitespace.
        if kind == "function" {
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\n' || bytes[j] == b'\r' || bytes[j] == b'*') { j += 1; }
        } else {
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\n' || bytes[j] == b'\r') { j += 1; }
        }
        // Read identifier.
        let id_start = j;
        if id_start < bytes.len() && (bytes[id_start].is_ascii_alphabetic() || bytes[id_start] == b'_' || bytes[id_start] == b'$') {
            let mut k = id_start + 1;
            while k < bytes.len() && is_ident_char(bytes[k]) { k += 1; }
            let name = src[id_start..k].to_string();
            if seen.insert(name.clone()) {
                names.push(name);
            }
            i = k;
        } else {
            i = j;
        }
    }
    names
}

fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
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

/// Like [`dispatch`] but with caller-supplied env vars surfaced as the
/// user-facing `vars` half of the EnvSnapshot. Routes through the same
/// path as production: snapshot → `set_env_snapshot` → `env_app_vars`,
/// reachable via `env.get(name)` and the `zeroship` module's `env`.
pub fn dispatch_with_env(
    modules: Vec<ModuleEntry>,
    env_vars: HashMap<String, String>,
    method: &str,
    args_json: &str,
) -> Result<RequestResult, String> {
    init_v8();
    let wrapped = wrap_user_modules_for_legacy_dispatch(modules);
    let runtime = Runtime::builder().modules(wrapped).build();
    let vars: std::collections::BTreeMap<String, String> = env_vars.into_iter().collect();
    let env = EnvSnapshot::new(vars, std::collections::BTreeMap::new(), Vec::new());
    let ctx = RequestCtx::new(CancelFlag::new());
    let body = if args_json.is_empty() {
        "".to_string()
    } else {
        format!(r#"{{"json":{}}}"#, args_json)
    };
    let url = format!("http://localhost/_zs/v1/{}", url_path_encode(method));
    let outcome = runtime.call_fetch_handler(
        "POST",
        &url,
        &[("content-type".into(), "application/json".into())],
        &body,
        &env,
        ctx,
    );

    if let FetchOutcome::Response { status, body: json_body, logs, .. } = &outcome {
        let (status, json_body, logs) = (*status, json_body.clone(), logs.clone());
        if !(200..300).contains(&status) {
            return Err(parse_error_message(&json_body));
        }
        return Ok(RequestResult {
            json: unwrap_json_envelope(&json_body),
            cpu_time: Duration::ZERO,
            wall_time: Duration::ZERO,
            logs,
        });
    }

    let (status, json_body, logs) = compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();
        drive_fetch_outcome(outcome).await
    });
    if !(200..300).contains(&status) {
        return Err(parse_error_message(&json_body));
    }
    Ok(RequestResult {
        json: unwrap_json_envelope(&json_body),
        cpu_time: Duration::ZERO,
        wall_time: Duration::ZERO,
        logs,
    })
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

/// Build a Runtime + call `call_fetch_handler` with a caller-supplied
/// EnvSnapshot. Used by Part B tests that want to assert visibility of
/// vars vs. secrets vs. the per-app `expose` opt-in across `process.env`,
/// the `zeroship` module's `env` import, and `env.get()`.
pub fn dispatch_fetch_with_env(
    modules: Vec<ModuleEntry>,
    req: TestRequest,
    env: EnvSnapshot,
) -> FetchOutcome {
    init_v8();
    let runtime = Runtime::builder().modules(modules).build();
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

