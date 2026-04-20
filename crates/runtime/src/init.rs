//! V8 platform initialization, global bindings, and shared constants.
//!
//! Consolidates everything needed to boot an isolate:
//! - `init_v8()` — one-time V8 platform init
//! - `setup_globals()` — console, timers, fetch, URL, KV, crypto, env, streams
//! - Polyfill constants (`FETCH_JS`, `URL_JS`, `CRYPTO_JS`, `STREAMS_JS`, `EVENTS_JS`, `BLOB_JS`, `FORMDATA_JS`)
//! - Result types (`RequestResult`, `HttpResult`)

use std::time::Duration;

use zeroship_runtime_macros::zeroship_op;

use crate::state::SharedState;
use crate::state::TimerCallback;

// ===========================================================================
// V8 platform init
// ===========================================================================

/// Initialize V8 (safe to call multiple times).
pub fn init_v8() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // Install the TLS crypto provider (rustls needs this for HTTPS fetch).
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        let platform = v8::new_default_platform(0, false).make_shared();
        v8::V8::initialize_platform(platform);
        v8::V8::initialize();
    });
}

// ===========================================================================
// CPU time helper
// ===========================================================================

/// Read the current thread's CPU time via CLOCK_THREAD_CPUTIME_ID.
/// Only counts actual CPU cycles — I/O wait is excluded.
pub fn thread_cpu_time() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    #[allow(unsafe_code)]
    unsafe {
        libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts);
    }
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

// ===========================================================================
// Result types
// ===========================================================================

/// Result of executing a JSON-RPC request.
#[derive(Debug)]
pub struct RequestResult {
    pub json: String,
    pub cpu_time: Duration,
    pub wall_time: Duration,
    /// Console output captured during execution.
    pub logs: Vec<String>,
}

/// Result of executing an HTTP request via onRequest handler.
#[derive(Debug)]
pub struct HttpResult {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub cpu_time: Duration,
    pub wall_time: Duration,
    pub logs: Vec<String>,
}

// ===========================================================================
// Polyfill / dispatch constants
// ===========================================================================

/// Embedded Fetch API polyfill -- loaded after globals are set up.
pub const FETCH_JS: &str = include_str!("embed/fetch.js");

/// Embedded URL/URLSearchParams polyfill backed by ada-url native parser.
pub const URL_JS: &str = include_str!("embed/url.js");

/// Embedded crypto polyfill (getRandomValues, SubtleCrypto.digest, base64 helpers).
pub const CRYPTO_JS: &str = include_str!("embed/crypto.js");

/// Embedded ReadableStream/WritableStream/TransformStream polyfill (backed by native __streams callbacks).
pub const STREAMS_JS: &str = include_str!("embed/streams.js");

/// Embedded Event/CustomEvent/EventTarget polyfill.
pub const EVENTS_JS: &str = include_str!("embed/events.js");

/// Embedded Blob/File polyfill.
pub const BLOB_JS: &str = include_str!("embed/blob.js");

/// Embedded FormData polyfill.
pub const FORMDATA_JS: &str = include_str!("embed/formdata.js");

/// Embedded WebSocket/WebSocketPair polyfill (depends on events.js for EventTarget).
pub const WEBSOCKET_JS: &str = include_str!("embed/websocket.js");

/// The `zeroship` user-facing ESM module. Exposes the request-scoped helpers
/// that SDK packages lean on:
///
/// - `env`: a frozen snapshot of per-app env vars (same as `fetch`'s 2nd arg).
/// - `waitUntil(promise)`: extend the isolate's hold on a request past its
///   response so fire-and-forget work (log flush, webhook retry) can finish.
/// - `getRequest()`: look up the current `Request` from any nested module
///   without threading it through every call. Throws if called outside a
///   request (bootstrap hasn't bound a ctx yet).
///
/// `__zs_wait_until` is registered by `setup_globals`; it throws to JS when
/// called outside a request. `__zs_env` / `__zs_get_request_ctx` are the
/// other halves.
pub(crate) const ZEROSHIP_MODULE_JS: &str = r#"
const env = Object.freeze(__zs_env());

function waitUntil(promise) {
    if (!(promise instanceof Promise)) {
        throw new TypeError("waitUntil expects a Promise");
    }
    __zs_wait_until(promise);
}

function getRequest() {
    // Kernel stashes the Request JS object on `state.request_by_id`
    // when it builds one in call_fetch_handler's slow path. The RPC
    // fast-path does NOT build a Request (body-is-args dispatch), so
    // getRequest() returns null there — use the default.fetch contract
    // when you need header/url access.
    const req = __zs_get_request();
    if (!req) {
        throw new Error("getRequest called outside a fetch handler (RPC fast-path has no Request)");
    }
    return req;
}

export { env, waitUntil, getRequest };
"#;

/// Internal bootstrap-only module. NOT part of the stable user-facing API —
/// only the runtime-synthesized `index.js` bootstrap imports from here. Kept
/// in its own specifier so `import { __bindRequest } from "zeroship"` fails
/// (users shouldn't poke at request-context plumbing).
pub(crate) const ZEROSHIP_INTERNAL_MODULE_JS: &str = r#"
// Bootstrap-only — NOT stable API. Users should not import this.
export function __bindRequest(ctx, request) {
    if (ctx == null) {
        __zs_bind_request_ctx(null);
        return;
    }
    // Attach the Request object to ctx so getRequest() can return it.
    ctx.__zs_request = request;
    __zs_bind_request_ctx(ctx);
}
"#;

/// Runtime-injected bootstrap module. Becomes the new entry (`index.js`),
/// wrapping the user's original entry (renamed internally to `__user__.js`).
///
/// Provides three things the user's handler doesn't have to hand-write:
///
/// 1. **RPC routing**: `POST /_rpc/<name>` → `user[<name>](...args)`, with
///    args as a JSON array in the body. Matches the "use server" named-export
///    idiom the AI compiler emits.
/// 2. **Request-context binding**: before invoking user code, stashes the
///    ctx + Request so nested modules can call `getRequest()` without
///    threading the request through every function signature.
/// 3. **Error/stream normalization**: JSON-formats thrown errors (honoring
///    `err.status`), auto-wraps async generators as SSE.
///
/// Non-`/_rpc/*` paths still fall through to `user.default?.fetch`, so
/// existing module-worker apps keep working unchanged.
pub(crate) const BOOTSTRAP_JS: &str = r##"
import * as user from "./__user__.js";
import { __bindRequest } from "zeroship/internal";

function sseFromAsyncGen(gen) {
    const encoder = new TextEncoder();
    const body = new ReadableStream({
        async start(controller) {
            try {
                while (true) {
                    const step = await gen.next();
                    if (step.done) {
                        const retJson = JSON.stringify(step.value === undefined ? null : step.value);
                        controller.enqueue(encoder.encode("event: return\ndata: " + retJson + "\n\n"));
                        break;
                    }
                    const valJson = JSON.stringify(step.value === undefined ? null : step.value);
                    controller.enqueue(encoder.encode("event: yield\ndata: " + valJson + "\n\n"));
                }
            } catch (e) {
                const payload = JSON.stringify({
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

function errorResponse(err) {
    const status = Number.isInteger(err && err.status) && err.status >= 400 && err.status < 600
        ? err.status : 500;
    const body = JSON.stringify({
        message: (err && err.message) ? err.message : String(err),
        name: (err && err.name) ? err.name : "Error",
    });
    return new Response(body, {
        status,
        headers: { "Content-Type": "application/json" },
    });
}

// Core RPC dispatch — shared by the kernel fast-path (dispatchRpc, called
// from Rust without building a full Request) and the fetch() handler's
// /_rpc/ route (which already has a Request in hand).
//
// `args` is a already-parsed JS array of positional arguments. Callers
// are responsible for the JSON.parse + array validation that precedes it.
async function invokeMethod(methodName, args) {
    const fn = user[methodName];
    if (typeof fn !== "function") {
        throw Object.assign(new Error("Method not found: " + methodName), { status: 404 });
    }
    let result = fn.apply(null, args);
    if (result && typeof result.then === "function") result = await result;

    if (result instanceof Response) return result;
    if (result != null && typeof result === "object"
        && typeof result[Symbol.asyncIterator] === "function"
        && typeof result.next === "function"
        && typeof result.return === "function") {
        return sseFromAsyncGen(result);
    }
    return Response.json(result === undefined ? null : result);
}

// Parse the RPC body into a JS positional-args array.
// Empty body → []. JSON.parse errors → 400. Non-array → 400. null → [].
function parseRpcArgs(bodyText) {
    if (!bodyText) return [];
    let parsed;
    try { parsed = JSON.parse(bodyText); }
    catch (_e) {
        throw Object.assign(new Error("Invalid args JSON"), { status: 400 });
    }
    if (parsed == null) return [];
    if (Array.isArray(parsed)) return parsed;
    throw Object.assign(new Error("RPC args body must be a JSON array"), { status: 400 });
}

// Fast RPC path called by the kernel when the URL starts with /_rpc/<method>.
// Skips full Request construction, URL parsing, and stream-body reads —
// the kernel already has the method name and body string in hand, and
// passes them directly.
async function dispatchRpc(methodName, bodyText) {
    try {
        const args = parseRpcArgs(bodyText);
        return await invokeMethod(methodName, args);
    } catch (err) {
        return errorResponse(err);
    }
}

// Full fetch handler — covers non-RPC paths, WebSocket upgrades, and
// WinterCG-style `default.fetch` delegation to user modules.
// Also handles /_rpc/* if the kernel ever routes it here (e.g., a
// third-party framework exporting `default` that isn't the bootstrap).
async function handleRpcFromRequest(request, methodName) {
    let bodyText = "";
    try { bodyText = await request.text(); } catch (_) {}
    return await dispatchRpc(methodName, bodyText);
}

// Resolve the user's default.fetch once at module init. When present, we
// export it directly as our `default.fetch` — no wrapper, no extra async
// frame, no extra try/catch. The kernel's `call_fetch_inner` already
// turns thrown exceptions into `DispatchResult::ErrorValue` with the
// correct HTTP status (honoring `err.status`), so a JS-side try/catch
// here would just add cost. This is the single biggest per-fetch win
// after dropping the URL parse and __bindRequest.
const USER_FETCH = (user && user.default && typeof user.default.fetch === "function")
    ? user.default.fetch
    : null;

// Optional zeroship extension: `user.default.fetchFast(method, url, body, env)`.
// Opt-in handler that bypasses the Request/Response construction entirely.
// Returns one of:
//   - { status, headers, body } plain object → HTTP response
//   - string / Uint8Array → 200 OK + that body
//   - null → kernel falls back to the slow `fetch(request, env, ctx)` path
// Kernel dispatches to this BEFORE constructing a Request. The path-routing
// wiring lives in the kernel: it sees /_rpc/* → dispatchRpc; everything
// else → fetchFast → (null) → fetch.
const USER_FETCH_FAST = (user && user.default && typeof user.default.fetchFast === "function")
    ? user.default.fetchFast
    : null;

const FALLBACK_RPC_TAG = "/_rpc/";

// Fallback fetch — used only when the user's module doesn't export a
// default.fetch handler. Handles /_rpc/* via URL for JS-direct callers,
// else 404.
async function fallbackFetch(request) {
    const urlStr = request.url;
    const tagIdx = urlStr.indexOf(FALLBACK_RPC_TAG);
    if (tagIdx >= 0) {
        const methodStart = tagIdx + FALLBACK_RPC_TAG.length;
        let methodEnd = urlStr.length;
        const q = urlStr.indexOf("?", methodStart);
        if (q >= 0 && q < methodEnd) methodEnd = q;
        const h = urlStr.indexOf("#", methodStart);
        if (h >= 0 && h < methodEnd) methodEnd = h;
        const rawMethod = urlStr.slice(methodStart, methodEnd);
        const method = rawMethod.indexOf("%") >= 0
            ? decodeURIComponent(rawMethod)
            : rawMethod;
        return await handleRpcFromRequest(request, method);
    }
    return new Response(
        '{"message":"Not Found","name":"Error"}',
        { status: 404, headers: { "Content-Type": "application/json" } }
    );
}

export default {
    // Kernel fast-path — caller supplies methodName + raw body text.
    dispatchRpc,
    // Zeroship extension: non-WinterCG fast HTTP dispatch. Kernel
    // calls this with raw (method, url, body, env). User returns a
    // plain response shape or null to fall through to fetch(). Skips
    // Request/Response construction entirely — hot-path-only win.
    fetchFast: USER_FETCH_FAST,
    // Standard WinterCG fetch handler — the user's default.fetch
    // directly (no bootstrap wrapper).
    fetch: USER_FETCH || fallbackFetch,
};
"##;

// ===========================================================================
// Shared initialization: polyfills + module loading
// ===========================================================================

/// Load polyfills and ES modules.
///
/// Shared by both `Isolate::ensure_initialized` and `ConcurrentIsolate::ensure_initialized`.
/// Returns the entry module's namespace object (so the caller can resolve
/// `default.fetch` without a reach-through global). `None` if module loading
/// failed (error is already logged).
///
/// The `plugins` slice is accepted but not currently invoked here — the
/// `zeroship.*` facade was removed as part of the kernel-cut refactor
/// (PR 1 Task D1). Plugins will be re-exposed via the bootstrap's `env.*`
/// binding in PR 3; the parameter is kept so call sites don't have to
/// change in this PR.
pub fn load_polyfills_and_modules(
    scope: &mut v8::PinScope,
    modules: &[crate::modules::ModuleEntry],
    _plugins: &[std::sync::Arc<dyn crate::plugin::NativePlugin>],
) -> Option<v8::Global<v8::Value>> {
    setup_globals(scope);

    // Load polyfills
    for polyfill in [FETCH_JS, URL_JS, CRYPTO_JS, STREAMS_JS, EVENTS_JS, BLOB_JS, FORMDATA_JS, WEBSOCKET_JS] {
        let code = v8::String::new(scope, polyfill).unwrap();
        let script = v8::Script::compile(scope, code, None).unwrap();
        script.run(scope).unwrap();
    }

    // Wrap the user's module graph in the bootstrap entry.
    //
    // Layout after wrapping:
    //   entries[0] = "index.js"            — BOOTSTRAP_JS (the new entry)
    //   entries[1] = "__user__.js"         — user's original entry (source preserved)
    //   entries[2] = "zeroship"            — env / waitUntil / getRequest facade
    //   entries[3] = "zeroship/internal"   — bootstrap-only __bindRequest
    //   entries[4..] = user's other modules (unchanged specifiers)
    //
    // The load_modules walker compiles BOOTSTRAP_JS first, discovers its two
    // imports (`./__user__.js` + `zeroship/internal`) and transitively the
    // user's `zeroship` imports, then instantiates + evaluates the bootstrap.
    // The returned namespace is the bootstrap's, so ensure_initialized reads
    // `default.fetch` off the bootstrap (not the user module) — exactly the
    // indirection we want.
    let wrapped = wrap_with_bootstrap(modules);

    // Load ES modules and return the entry module's namespace object.
    // The kernel reads `default.fetch` directly off the namespace — no more
    // `__rpc` copy loop, no more `DISPATCH_JS`, no more URL-path router.
    match crate::modules::load_modules(scope, &wrapped) {
        Ok(namespace) => Some(namespace),
        Err(e) => {
            eprintln!("[v8] Module loading failed: {e}");
            None
        }
    }
}

/// Rewrite the user's module list so the bootstrap is the new entry.
///
/// The user's declared first module is renamed to `__user__.js`; a synthetic
/// `index.js` (BOOTSTRAP_JS) is prepended as the new entry, plus the two
/// zeroship modules (`zeroship` and `zeroship/internal`).
///
/// **Collision**: the compiler always emits `index.js` as the user's entry,
/// so a user entry actually named `__user__.js` is a bug if it happens. A
/// `debug_assert!` catches this in dev builds; in release it's silently
/// overwritten (the user module's source wins over our internal specifier
/// by virtue of ordering in the sources map).
fn wrap_with_bootstrap(
    modules: &[crate::modules::ModuleEntry],
) -> Vec<crate::modules::ModuleEntry> {
    use crate::modules::ModuleEntry;

    // Empty input preserved as-is — the module loader will return a clean
    // "No modules to load" error. Don't synthesize a bootstrap pointing at
    // a non-existent `__user__.js`.
    if modules.is_empty() {
        return Vec::new();
    }

    let mut out: Vec<ModuleEntry> = Vec::with_capacity(modules.len() + 3);

    // entry 0: bootstrap becomes the new entrypoint under "index.js".
    out.push(ModuleEntry {
        specifier: "index.js".into(),
        source: BOOTSTRAP_JS.into(),
    });

    // entry 1: user's original entry, renamed to "__user__.js". Its own
    // declared specifier (usually "index.js") is discarded — the bootstrap
    // imports `./__user__.js` by exact name.
    let user_entry = &modules[0];
    debug_assert!(
        user_entry.specifier != "__user__.js",
        "User entry collides with bootstrap's internal specifier",
    );
    out.push(ModuleEntry {
        specifier: "__user__.js".into(),
        source: user_entry.source.clone(),
    });

    // entries 2-3: the zeroship facade + internal modules. Live in the
    // module graph alongside the user's modules so `import ... from "zeroship"`
    // resolves via the normal lookup path.
    out.push(ModuleEntry {
        specifier: "zeroship".into(),
        source: ZEROSHIP_MODULE_JS.into(),
    });
    out.push(ModuleEntry {
        specifier: "zeroship/internal".into(),
        source: ZEROSHIP_INTERNAL_MODULE_JS.into(),
    });

    // Remaining user modules — pass through unchanged. Their declared
    // specifiers (other than "index.js" which can't collide since we moved
    // the user entry) stay valid for their own cross-module imports.
    for entry in modules.iter().skip(1) {
        out.push(entry.clone());
    }

    out
}

// ===========================================================================
// Console polyfill (variadic — stays manual)
// ===========================================================================

/// Max bytes retained for a single `console.log` line. Protects the
/// per-request log vector (shipped back to the gateway) and the operator's
/// stderr from an app doing `console.log(hugeString)` in a loop.
const CONSOLE_LINE_MAX: usize = 4096;

/// Truncate a console line to `CONSOLE_LINE_MAX` bytes, preserving a valid
/// UTF-8 boundary and appending a truncation marker so operators can tell.
fn truncate_console_line(mut line: String) -> String {
    if line.len() <= CONSOLE_LINE_MAX {
        return line;
    }
    // `floor_char_boundary` isn't stable, so walk back from the cap to the
    // nearest char boundary manually.
    let mut cut = CONSOLE_LINE_MAX;
    while cut > 0 && !line.is_char_boundary(cut) {
        cut -= 1;
    }
    line.truncate(cut);
    line.push_str("…[truncated]");
    line
}

fn console_log_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let mut parts = Vec::new();
    for i in 0..args.length() {
        let arg = args.get(i);
        let s = arg.to_rust_string_lossy(scope);
        parts.push(s);
    }
    let line = truncate_console_line(parts.join(" "));

    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let mut s = state.borrow_mut();
    let req_id = s.executing_request_id;

    // Operator-visible mirror on stderr (not stdout — stdout should stay
    // clean for CLI tools that want to capture structured output). Prefix
    // with request metadata so multi-request logs are disentanglable, and
    // only enable in dev / when ZEROSHIP_LOG is set.
    if std::env::var("ZEROSHIP_LOG").is_ok() || cfg!(debug_assertions) {
        match req_id {
            Some(rid) => eprintln!("[app req={rid}] {line}"),
            None => eprintln!("[app] {line}"),
        }
    }

    let logs = s.per_request_logs.entry(req_id.unwrap_or(0)).or_default();
    logs.push(line);
    if logs.len() > 1000 {
        let drain = logs.len() - 1000;
        logs.drain(..drain);
    }
}

#[cfg(test)]
mod console_tests {
    use super::*;

    #[test]
    fn short_lines_untouched() {
        assert_eq!(truncate_console_line("hello".to_string()), "hello");
    }

    #[test]
    fn long_lines_truncated() {
        let long = "x".repeat(CONSOLE_LINE_MAX + 100);
        let out = truncate_console_line(long);
        assert!(out.len() <= CONSOLE_LINE_MAX + "…[truncated]".len());
        assert!(out.ends_with("…[truncated]"));
    }

    #[test]
    fn truncation_respects_utf8_boundaries() {
        // Multi-byte char right at the boundary — truncation must not split it.
        let mut long = "x".repeat(CONSOLE_LINE_MAX - 1);
        long.push('ñ'); // 2-byte UTF-8 char straddles the boundary
        long.push_str(&"y".repeat(200));
        let out = truncate_console_line(long);
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }
}

// ===========================================================================
// queueMicrotask — schedules a callback to run after current JS completes
// ===========================================================================

fn queue_microtask_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    if args.length() < 1 || !args.get(0).is_function() {
        return;
    }
    let func = v8::Local::<v8::Function>::try_from(args.get(0)).unwrap();
    // Schedule via Promise.resolve().then(callback)
    // This enqueues the callback as a microtask that runs at the next checkpoint.
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let undefined = v8::undefined(scope);
    resolver.resolve(scope, undefined.into());
    promise.then(scope, func);
}

// ===========================================================================
// performance.now — high-resolution monotonic timestamp in milliseconds
// ===========================================================================

/// Start time for performance.now() — set once per isolate.
/// `performance.now()` — per-isolate high-resolution clock.
///
/// Each app gets its own epoch (stored as `perf_epoch` in `RuntimeState`)
/// so one app cannot observe when another app's requests started, how
/// long they took, or when the V8 thread was busy serving someone else.
///
/// An earlier revision used a `static OnceLock<Instant>` shared across
/// the entire process — all apps saw the same time origin and could
/// derive each other's scheduling patterns via differential timing.
fn performance_now_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: crate::state::SharedState = scope
        .get_slot::<crate::state::SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let epoch = state.borrow().perf_epoch;
    let elapsed_ms = epoch.elapsed().as_secs_f64() * 1000.0;
    rv.set(v8::Number::new(scope, elapsed_ms).into());
}

// ===========================================================================
// __zs_env — return the current frozen env snapshot
// ===========================================================================

/// `__zs_env()` — returns the composite env object (plugin namespaces +
/// scalar env JSON).
///
/// Built once by `RuntimeInner::ensure_initialized` and cached on
/// `RuntimeState.env_obj`. Every call returns the same V8 Global so SDK
/// code importing `env` from the `zeroship` module sees the same object
/// identity as the `env` arg of `fetch(req, env, ctx)`.
///
/// Fallback: if called before `ensure_initialized` completed (shouldn't
/// happen under the normal dispatch path, but be defensive), JSON-parse
/// the scalar snapshot instead of panicking — plugin namespaces will be
/// missing but at least the scalar values are visible.
fn zs_env_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let env_opt = state.borrow().env_obj.clone();
    match env_opt {
        Some(env_global) => {
            let env_local = v8::Local::new(scope, env_global);
            rv.set(env_local.into());
        }
        None => {
            let json = state.borrow().env_json.clone();
            match v8::String::new(scope, &json) {
                Some(s) => match v8::json::parse(scope, s) {
                    Some(val) => rv.set(val),
                    None => rv.set(v8::Object::new(scope).into()),
                },
                None => rv.set(v8::Object::new(scope).into()),
            }
        }
    }
}

// ===========================================================================
// __zs_bind_request_ctx / __zs_get_request_ctx — per-request ctx stash
// ===========================================================================

/// `__zs_bind_request_ctx(ctxObj)` — stash the JS `ctx` object on the
/// currently-executing request so nested modules can look it up without
/// threading it through every call. Called by the bootstrap (PR 2)
/// immediately on entry to `fetch(req, env, ctx)`. Passing `null` clears
/// the stash; passing a non-object is a silent no-op.
fn zs_bind_request_ctx_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let rid_opt = state.borrow().executing_request_id;
    let Some(rid) = rid_opt else {
        // No active request — silently ignore. Bootstrap should never
        // call this outside a request, but defensive no-op is safer
        // than a throw.
        return;
    };

    let arg = args.get(0);
    if arg.is_null() || arg.is_undefined() {
        // __zs_bind_request_ctx(null) clears the stashed ctx.
        state.borrow_mut().request_ctx_by_id.remove(&rid);
        return;
    }
    if !arg.is_object() {
        // Non-null, non-object — ignore (type error from JS side
        // would be appropriate but silent for now).
        return;
    }
    let obj: v8::Local<v8::Object> = arg.try_into().unwrap();
    let global_obj = v8::Global::new(scope, obj);
    state.borrow_mut().request_ctx_by_id.insert(rid, global_obj);
}

/// `__zs_wait_until(promise)` — push a Promise onto the current request's
/// waitUntil bag. The kernel keeps the isolate alive past the response body
/// write until every promise here settles (or the wall timeout fires).
///
/// Type-checking and TypeError on non-Promise args is done in the JS-side
/// `zeroship.waitUntil` wrapper; this op defensively no-ops on bad input so
/// a JS-side bug can't crash the isolate. Silent no-op when called outside
/// an active request — the JS side already checks and doesn't call us in
/// that case, but be conservative for robustness.
fn zs_wait_until_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let state = scope
        .get_slot::<crate::state::SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let arg = args.get(0);
    if !arg.is_promise() {
        return;
    }
    let promise: v8::Local<v8::Promise> = arg.try_into().unwrap();
    let global = v8::Global::new(scope, promise);
    let _registered = state.borrow_mut().register_wait_until(global);
    // If register_wait_until returned false there's no active request —
    // drop the promise silently. The JS-side wrapper is the user-facing
    // contract for that case.
}

/// `__zs_get_request_ctx()` — return the stashed `ctx` object for the
/// currently-executing request, or `null` if none was bound (no active
/// request, or bootstrap hasn't run). Returns the exact same object
/// reference passed to `__zs_bind_request_ctx` — not a clone.
fn zs_get_request_ctx_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let rid_opt = state.borrow().executing_request_id;
    let Some(rid) = rid_opt else {
        rv.set(v8::null(scope).into());
        return;
    };
    let ctx_opt = state.borrow().request_ctx_by_id.get(&rid).cloned();
    match ctx_opt {
        Some(ctx_global) => {
            let ctx_local = v8::Local::new(scope, ctx_global);
            rv.set(ctx_local.into());
        }
        None => {
            rv.set(v8::null(scope).into());
        }
    }
}

/// `__zs_get_request()` — return the Request JS object for the current
/// in-flight request, or `null` if none (e.g. the RPC fast-path doesn't
/// construct a Request since there's no URL/header work to do).
///
/// The kernel stores the Request at call_fetch_handler's slow-path entry,
/// immediately after it constructs one via HTTP_CREATE_REQUEST_JS. Stored
/// keyed by the same `executing_request_id` that drives per_request_user
/// / waitUntil / logs, so cleanup rides on `drain_request_logs`.
fn zs_get_request_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let rid_opt = state.borrow().executing_request_id;
    let Some(rid) = rid_opt else {
        rv.set(v8::null(scope).into());
        return;
    };
    let req_opt = state.borrow().request_by_id.get(&rid).cloned();
    match req_opt {
        Some(req_global) => {
            let local = v8::Local::new(scope, req_global);
            rv.set(local.into());
        }
        None => {
            rv.set(v8::null(scope).into());
        }
    }
}

// ===========================================================================
// Timer callbacks (take v8::Function args — stays manual)
// ===========================================================================

fn set_timeout_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: crate::state::SharedState = scope
        .get_slot::<crate::state::SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let callback = match v8::Local::<v8::Function>::try_from(args.get(0)) {
        Ok(f) => f,
        Err(_) => {
            let msg = v8::String::new(scope, "setTimeout: first argument must be a function")
                .unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };

    let ms = if args.length() > 1 {
        args.get(1).uint32_value(scope).unwrap_or(0)
    } else {
        0
    };

    // Admission control: reject before allocating V8 handles / state.
    {
        let s = state.borrow();
        if s.timer_callbacks.len() >= crate::state::MAX_PENDING_TIMERS {
            drop(s);
            let msg = v8::String::new(
                scope,
                &format!("Too many pending timers (limit: {})", crate::state::MAX_PENDING_TIMERS),
            ).unwrap();
            let exc = v8::Exception::range_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    }

    let global_cb = v8::Global::new(scope, callback);
    let delay = Duration::from_millis(u64::from(ms));

    let mut s = state.borrow_mut();
    let id = s.next_timer_id;
    s.next_timer_id += 1;
    s.timer_callbacks.insert(id, TimerCallback { callback: global_cb, interval: None });
    if let Some(req_id) = s.executing_request_id {
        s.timer_owner.insert(id, req_id);
    }
    if delay < Duration::from_millis(1) {
        s.ready_timers.push_back(id);
    } else {
        s.spawned_timers.push(crate::state::SpawnedTimer { id, delay, interval: None });
    }

    rv.set(v8::Integer::new(scope, id as i32).into());
}

fn clear_timeout_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let state: crate::state::SharedState = scope
        .get_slot::<crate::state::SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let id = if args.length() > 0 {
        args.get(0).uint32_value(scope).unwrap_or(0)
    } else {
        return;
    };

    let mut s = state.borrow_mut();
    s.timer_callbacks.remove(&id);
    s.timer_owner.remove(&id);
    // The tokio::time::sleep future will still fire but handle_timer()
    // will find no callback and do nothing.
}

fn set_interval_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: crate::state::SharedState = scope
        .get_slot::<crate::state::SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let callback = match v8::Local::<v8::Function>::try_from(args.get(0)) {
        Ok(f) => f,
        Err(_) => {
            let msg = v8::String::new(scope, "setInterval: first argument must be a function")
                .unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };

    let ms = if args.length() > 1 {
        args.get(1).uint32_value(scope).unwrap_or(0)
    } else {
        0
    };
    let delay = Duration::from_millis(u64::from(ms));

    // Same admission control as setTimeout.
    {
        let s = state.borrow();
        if s.timer_callbacks.len() >= crate::state::MAX_PENDING_TIMERS {
            drop(s);
            let msg = v8::String::new(
                scope,
                &format!("Too many pending timers (limit: {})", crate::state::MAX_PENDING_TIMERS),
            ).unwrap();
            let exc = v8::Exception::range_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    }

    let global_cb = v8::Global::new(scope, callback);
    let mut s = state.borrow_mut();
    let id = s.next_timer_id;
    s.next_timer_id += 1;
    s.timer_callbacks.insert(id, TimerCallback { callback: global_cb, interval: Some(delay) });
    if let Some(req_id) = s.executing_request_id {
        s.timer_owner.insert(id, req_id);
    }
    s.spawned_timers.push(crate::state::SpawnedTimer { id, delay, interval: Some(delay) });

    rv.set(v8::Integer::new(scope, id as i32).into());
}

// ===========================================================================
// Setup all globals on a V8 context
// ===========================================================================

/// Install console, timers, fetch, URL, KV, crypto, env on the global object.
///
/// Callbacks from `#[zeroship_op]` modules are referenced as `crate::{mod}::{fn}_callback`.
pub fn setup_globals(scope: &mut v8::PinScope) {
    let global = scope.get_current_context().global(scope);

    // global = globalThis (Node.js compat — many npm packages reference `global`)
    {
        let key = v8::String::new(scope, "global").unwrap();
        global.set(scope, key.into(), global.into());
    }

    // console.log/warn/error/info
    {
        let console = v8::Object::new(scope);
        let log_fn = v8::Function::new(scope, console_log_callback).unwrap();
        let log_key = v8::String::new(scope, "log").unwrap();
        console.set(scope, log_key.into(), log_fn.into());

        let warn_key = v8::String::new(scope, "warn").unwrap();
        console.set(scope, warn_key.into(), log_fn.into());
        let error_key = v8::String::new(scope, "error").unwrap();
        console.set(scope, error_key.into(), log_fn.into());
        let info_key = v8::String::new(scope, "info").unwrap();
        console.set(scope, info_key.into(), log_fn.into());
        let debug_key = v8::String::new(scope, "debug").unwrap();
        console.set(scope, debug_key.into(), log_fn.into());

        let console_key = v8::String::new(scope, "console").unwrap();
        global.set(scope, console_key.into(), console.into());
    }

    // setTimeout
    {
        let f = v8::Function::new(scope, set_timeout_callback).unwrap();
        let key = v8::String::new(scope, "setTimeout").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // clearTimeout
    {
        let f = v8::Function::new(scope, clear_timeout_callback).unwrap();
        let key = v8::String::new(scope, "clearTimeout").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // setInterval
    {
        let f = v8::Function::new(scope, set_interval_callback).unwrap();
        let key = v8::String::new(scope, "setInterval").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // clearInterval (same implementation as clearTimeout)
    {
        let f = v8::Function::new(scope, clear_timeout_callback).unwrap();
        let key = v8::String::new(scope, "clearInterval").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // queueMicrotask
    {
        let f = v8::Function::new(scope, queue_microtask_callback).unwrap();
        let key = v8::String::new(scope, "queueMicrotask").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // performance.now
    {
        let perf = v8::Object::new(scope);
        let f = v8::Function::new(scope, performance_now_callback).unwrap();
        let key = v8::String::new(scope, "now").unwrap();
        perf.set(scope, key.into(), f.into());
        let perf_key = v8::String::new(scope, "performance").unwrap();
        global.set(scope, perf_key.into(), perf.into());
    }

    // navigator.userAgent
    {
        let nav = v8::Object::new(scope);
        let ua = v8::String::new(scope, "zeroship/1.0").unwrap();
        let ua_key = v8::String::new(scope, "userAgent").unwrap();
        nav.set(scope, ua_key.into(), ua.into());
        let nav_key = v8::String::new(scope, "navigator").unwrap();
        global.set(scope, nav_key.into(), nav.into());
    }

    // __rawFetch (native HTTP fetch)
    {
        let f = v8::Function::new(scope, crate::fetch::raw_fetch_callback).unwrap();
        let key = v8::String::new(scope, "__rawFetch").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // __urlParse / __urlCanParse (native URL parser via ada-url)
    {
        let f = v8::Function::new(scope, crate::url::url_parse_callback).unwrap();
        let key = v8::String::new(scope, "__urlParse").unwrap();
        global.set(scope, key.into(), f.into());

        let f = v8::Function::new(scope, crate::url::url_can_parse_callback).unwrap();
        let key = v8::String::new(scope, "__urlCanParse").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // crypto namespace (randomUUID + native helpers for SubtleCrypto)
    {
        let crypto = v8::Object::new(scope);

        let uuid_fn = v8::Function::new(scope, crate::crypto::crypto_random_uuid_callback).unwrap();
        let uuid_key = v8::String::new(scope, "randomUUID").unwrap();
        crypto.set(scope, uuid_key.into(), uuid_fn.into());

        // getRandomValues — direct TypedArray fill, no base64 (hand-written callback)
        let grv_fn = v8::Function::new(scope, crate::crypto::crypto_get_random_values_callback).unwrap();
        let grv_key = v8::String::new(scope, "getRandomValues").unwrap();
        crypto.set(scope, grv_key.into(), grv_fn.into());

        let digest_fn = v8::Function::new(scope, crate::crypto::crypto_digest_callback).unwrap();
        let digest_key = v8::String::new(scope, "__cryptoDigest").unwrap();
        crypto.set(scope, digest_key.into(), digest_fn.into());

        let import_fn = v8::Function::new(scope, crate::crypto::crypto_import_key_callback).unwrap();
        let import_key = v8::String::new(scope, "__cryptoImportKey").unwrap();
        crypto.set(scope, import_key.into(), import_fn.into());

        let export_fn = v8::Function::new(scope, crate::crypto::crypto_export_key_callback).unwrap();
        let export_key = v8::String::new(scope, "__cryptoExportKey").unwrap();
        crypto.set(scope, export_key.into(), export_fn.into());

        let gen_fn = v8::Function::new(scope, crate::crypto::crypto_generate_key_callback).unwrap();
        let gen_key = v8::String::new(scope, "__cryptoGenerateKey").unwrap();
        crypto.set(scope, gen_key.into(), gen_fn.into());

        let sign_fn = v8::Function::new(scope, crate::crypto::crypto_sign_callback).unwrap();
        let sign_key = v8::String::new(scope, "__cryptoSign").unwrap();
        crypto.set(scope, sign_key.into(), sign_fn.into());

        let verify_fn = v8::Function::new(scope, crate::crypto::crypto_verify_callback).unwrap();
        let verify_key = v8::String::new(scope, "__cryptoVerify").unwrap();
        crypto.set(scope, verify_key.into(), verify_fn.into());

        let encrypt_fn = v8::Function::new(scope, crate::crypto::crypto_encrypt_callback).unwrap();
        let encrypt_key = v8::String::new(scope, "__cryptoEncrypt").unwrap();
        crypto.set(scope, encrypt_key.into(), encrypt_fn.into());

        let decrypt_fn = v8::Function::new(scope, crate::crypto::crypto_decrypt_callback).unwrap();
        let decrypt_key = v8::String::new(scope, "__cryptoDecrypt").unwrap();
        crypto.set(scope, decrypt_key.into(), decrypt_fn.into());

        let derive_bits_fn = v8::Function::new(scope, crate::crypto::crypto_derive_bits_callback).unwrap();
        let derive_bits_key = v8::String::new(scope, "__cryptoDeriveBits").unwrap();
        crypto.set(scope, derive_bits_key.into(), derive_bits_fn.into());

        let derive_key_fn = v8::Function::new(scope, crate::crypto::crypto_derive_key_callback).unwrap();
        let derive_key_key = v8::String::new(scope, "__cryptoDeriveKey").unwrap();
        crypto.set(scope, derive_key_key.into(), derive_key_fn.into());

        let crypto_key = v8::String::new(scope, "crypto").unwrap();
        global.set(scope, crypto_key.into(), crypto.into());
    }

    // Native sync hash/HMAC for node:crypto polyfill
    {
        let f = v8::Function::new(scope, crate::crypto::crypto_hash_sync_callback).unwrap();
        let key = v8::String::new(scope, "__cryptoHashSync").unwrap();
        global.set(scope, key.into(), f.into());

        let f = v8::Function::new(scope, crate::crypto::crypto_hmac_sync_callback).unwrap();
        let key = v8::String::new(scope, "__cryptoHmacSync").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // __streams namespace (native backing for ReadableStream)
    {
        let streams = v8::Object::new(scope);

        let create_fn = v8::Function::new(scope, crate::streams::stream_create_callback).unwrap();
        let create_key = v8::String::new(scope, "create").unwrap();
        streams.set(scope, create_key.into(), create_fn.into());

        let read_fn = v8::Function::new(scope, crate::streams::stream_read_callback).unwrap();
        let read_key = v8::String::new(scope, "read").unwrap();
        streams.set(scope, read_key.into(), read_fn.into());

        let enqueue_fn = v8::Function::new(scope, crate::streams::stream_enqueue_callback).unwrap();
        let enqueue_key = v8::String::new(scope, "enqueue").unwrap();
        streams.set(scope, enqueue_key.into(), enqueue_fn.into());

        let close_fn = v8::Function::new(scope, crate::streams::stream_close_callback).unwrap();
        let close_key = v8::String::new(scope, "close").unwrap();
        streams.set(scope, close_key.into(), close_fn.into());

        let error_fn = v8::Function::new(scope, crate::streams::stream_error_callback).unwrap();
        let error_key = v8::String::new(scope, "error").unwrap();
        streams.set(scope, error_key.into(), error_fn.into());

        let streams_key = v8::String::new(scope, "__streams").unwrap();
        global.set(scope, streams_key.into(), streams.into());
    }

    // WebSocket native callbacks
    {
        let f = v8::Function::new(scope, crate::websocket::ws_create_pair_callback).unwrap();
        let key = v8::String::new(scope, "__wsCreatePair").unwrap();
        global.set(scope, key.into(), f.into());

        let f = v8::Function::new(scope, crate::websocket::ws_link_pair_callback).unwrap();
        let key = v8::String::new(scope, "__wsLinkPair").unwrap();
        global.set(scope, key.into(), f.into());

        let f = v8::Function::new(scope, crate::websocket::ws_accept_callback).unwrap();
        let key = v8::String::new(scope, "__wsAccept").unwrap();
        global.set(scope, key.into(), f.into());

        let f = v8::Function::new(scope, crate::websocket::ws_send_callback).unwrap();
        let key = v8::String::new(scope, "__wsSend").unwrap();
        global.set(scope, key.into(), f.into());

        let f = v8::Function::new(scope, crate::websocket::ws_close_callback).unwrap();
        let key = v8::String::new(scope, "__wsClose").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // env namespace
    {
        let env = v8::Object::new(scope);

        let get_fn = v8::Function::new(scope, env_get_callback).unwrap();
        let get_key = v8::String::new(scope, "get").unwrap();
        env.set(scope, get_key.into(), get_fn.into());

        let env_key = v8::String::new(scope, "env").unwrap();
        global.set(scope, env_key.into(), env.into());
    }

    // __zs_env — returns the frozen env snapshot (same as fetch's 2nd arg).
    // The zeroship JS module exposes this as `const env = Object.freeze(__zs_env());`
    // so SDK packages can read env.* without threading it through fetch().
    {
        let f = v8::Function::new(scope, zs_env_callback).unwrap();
        let key = v8::String::new(scope, "__zs_env").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // __zs_bind_request_ctx / __zs_get_request_ctx — per-request ctx stash
    // for the PR 2 bootstrap. `__zs_bind_request_ctx(ctx)` stashes the
    // object under the current request_id; `__zs_get_request_ctx()` returns
    // the same reference from any nested module. Lightweight replacement
    // for AsyncLocalStorage — single-threaded isolate, request_id tracked
    // by the pump across await boundaries.
    {
        let bind_key = v8::String::new(scope, "__zs_bind_request_ctx").unwrap();
        let bind_fn = v8::Function::new(scope, zs_bind_request_ctx_callback).unwrap();
        global.set(scope, bind_key.into(), bind_fn.into());

        let get_key = v8::String::new(scope, "__zs_get_request_ctx").unwrap();
        let get_fn = v8::Function::new(scope, zs_get_request_ctx_callback).unwrap();
        global.set(scope, get_key.into(), get_fn.into());

        // __zs_get_request — direct-read for the current Request JS object.
        // Stored by the kernel in `state.request_by_id` on the fetch()
        // slow path; empty for the RPC fast-path (no Request built). Lets
        // `getRequest()` skip a per-request __bindRequest round-trip.
        let req_key = v8::String::new(scope, "__zs_get_request").unwrap();
        let req_fn = v8::Function::new(scope, zs_get_request_callback).unwrap();
        global.set(scope, req_key.into(), req_fn.into());
    }

    // __zs_wait_until — registers a Promise against the current request's
    // wait-until bag. Consumed by `zeroship.waitUntil` in the user-facing
    // ESM module; the kernel holds the isolate alive past the response
    // until every promise settles or the wall timeout fires.
    {
        let key = v8::String::new(scope, "__zs_wait_until").unwrap();
        let f = v8::Function::new(scope, zs_wait_until_callback).unwrap();
        global.set(scope, key.into(), f.into());
    }

    // process.env polyfill — many npm packages (e.g. LangChain) read
    // `process.env.OPENAI_API_KEY`. Populate from the per-app env_vars
    // stored in RuntimeState (set by the worker cache during load_app).
    //
    // SECURITY: an earlier revision used `std::env::vars()` which leaked
    // every host-level secret (DATABASE_URL, WORKER_KEY, AWS credentials)
    // to every app. Multi-tenant apps must only see their own env vars.
    // The control plane can inject per-app secrets into the app bundle or
    // the deploy metadata; those arrive in `env_vars` via the worker.
    {
        let process = v8::Object::new(scope);
        let env_obj = v8::Object::new(scope);

        // Read per-app env_vars from the RuntimeState slot.
        let state: crate::state::SharedState = scope
            .get_slot::<crate::state::SharedState>()
            .expect("RuntimeState not in isolate slot")
            .clone();
        let app_env = state.borrow().env_vars.clone();
        for (key, value) in &app_env {
            let k = v8::String::new(scope, key).unwrap();
            let v = v8::String::new(scope, value).unwrap();
            env_obj.set(scope, k.into(), v.into());
        }

        let env_key = v8::String::new(scope, "env").unwrap();
        process.set(scope, env_key.into(), env_obj.into());

        let version = v8::String::new(scope, "v20.0.0").unwrap();
        let version_key = v8::String::new(scope, "version").unwrap();
        process.set(scope, version_key.into(), version.into());

        let process_key = v8::String::new(scope, "process").unwrap();
        global.set(scope, process_key.into(), process.into());
    }

}

// ===========================================================================
// env.get (absorbed from v8/env.rs)
// ===========================================================================

/// `env.get(key) → string | null`
///
/// Reads from the per-app environment variables injected at deploy time.
#[zeroship_op(state)]
fn env_get(state: SharedState, key: String) -> Option<String> {
    state.borrow().env_vars.get(&key).cloned()
}
