//! V8 platform initialization, global bindings, and shared constants.
//!
//! Consolidates everything needed to boot an isolate:
//! - `init_v8()` — one-time V8 platform init
//! - `setup_globals()` — console, timers, fetch, URL, KV, crypto, env, streams
//! - Polyfill constants (`FETCH_JS`, `URL_JS`, `CRYPTO_JS`, `STREAMS_JS`, `EVENTS_JS`, `BLOB_JS`, `FORMDATA_JS`)
//! - Dispatch scripts (`DISPATCH_JS`)
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

/// The RPC dispatch function compiled once and reused for every request.
///
/// Takes the method name + raw args-JSON slice, returns the *raw* handler
/// return value (Promise, async generator, plain JS value, or Response).
/// The HTTP response is built in Rust — no JSON-RPC envelope.
///
/// Contract:
/// - Args: `(method, argsJson)` — `argsJson` is a JSON array (may be null/empty).
/// - Return: whatever the handler returned. Rust inspects:
///     * Async generator (has Symbol.asyncIterator + `.next` yields) →
///       wrap in a Response(ReadableStream) that emits SSE `event:yield`
///       frames per yield and `event:return`/`event:error` on termination.
///     * `Response` instance → pass through unchanged.
///     * plain value → JSON.stringify and return as `application/json` body.
/// - Throw: any thrown value bubbles via V8 TryCatch. Rust builds an error
///   body `{"message","name","stack"}` with HTTP 500 (or `err.status` if
///   numeric 400-599 — users can throw `HttpError` to set the status).
///
/// `__rpc` is the RPC registry — a plain object populated from module
/// exports AND from `__register(name, fn)` calls that the vite-plugin
/// transform emits into server modules so path-based keys
/// (`src/api/users/getUser`) resolve to the right function.
pub const DISPATCH_JS: &str = r#"(function(__method, __argsJson) {
    var fn = __rpc[__method];
    if (typeof fn !== 'function') {
        var err = new Error('Method not found: ' + __method);
        err.status = 404;
        throw err;
    }
    var args;
    if (!__argsJson) {
        args = [];
    } else {
        var parsed;
        try {
            parsed = JSON.parse(__argsJson);
        } catch (_e) {
            // Client-side error: malformed JSON body. Classify as 400 Bad
            // Request and use a generic message so we don't leak internal
            // V8 parser diagnostics.
            var err = new Error('Invalid args JSON');
            err.status = 400;
            throw err;
        }
        if (parsed == null) {
            args = [];
        } else if (Array.isArray(parsed)) {
            args = parsed;
        } else {
            // Wire contract: args body must be a JSON array. A non-array
            // would otherwise get silently coerced to zero args via
            // Function.prototype.apply's CreateListFromArrayLike step.
            var err = new Error('RPC args body must be a JSON array');
            err.status = 400;
            throw err;
        }
    }
    var result = fn.apply(null, args);

    // Wrap async generators in a Response(ReadableStream) that emits SSE
    // frames. Detection: the returned object has a Symbol.asyncIterator
    // method AND a generator-style `next/return/throw` triple. This matches
    // `async function*` output but NOT a plain async function returning
    // a value or a Response (both of which go through the passthrough path).
    if (result != null && typeof result === 'object'
        && typeof result[Symbol.asyncIterator] === 'function'
        && typeof result.next === 'function'
        && typeof result.return === 'function') {
        return __wrapAsyncGenerator(result);
    }
    return result;
})"#;

/// Wraps an async generator in a Response(ReadableStream) that emits SSE
/// frames. Installed as a global helper alongside DISPATCH_JS.
///
/// Frames:
/// - `event: yield\ndata: <json>\n\n` for each `yield` value
/// - `event: return\ndata: <json>\n\n` on clean completion (value is the
///   generator's return value, or `null` if the body used a bare `return`)
/// - `event: error\ndata: {"message":"...","name":"..."}\n\n` on throw
///
/// One SSE `data:` line per frame — yielded values that are multi-line
/// JSON strings round-trip correctly because the JSON is on a single line
/// (JSON.stringify never emits embedded newlines unless the caller passed
/// an indentation argument, which we don't).
pub const WRAP_ASYNC_GENERATOR_JS: &str = r#"globalThis.__wrapAsyncGenerator = function(gen) {
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
};
globalThis.__register = function(name, fn) {
    if (typeof globalThis.__rpc !== 'object' || globalThis.__rpc === null) {
        globalThis.__rpc = Object.create(null);
    }
    globalThis.__rpc[name] = fn;
};"#;

// ===========================================================================
// Shared initialization: polyfills + module loading + dispatch compilation
// ===========================================================================

/// Load polyfills, ES modules, and compile the JSON-RPC dispatch function.
///
/// Shared by both `Isolate::ensure_initialized` and `ConcurrentIsolate::ensure_initialized`.
/// Returns the compiled dispatch `Global<Function>`.
pub fn load_polyfills_and_modules(
    scope: &mut v8::PinScope,
    modules: &[crate::modules::ModuleEntry],
    plugins: &[std::sync::Arc<dyn crate::plugin::NativePlugin>],
) -> v8::Global<v8::Function> {
    setup_globals(scope);

    // Register plugins on zeroship.* namespace (does NOT freeze yet)
    crate::plugin::register_plugins(scope, plugins);

    // Register zeroship.auth (built-in, always available)
    {
        let global = scope.get_current_context().global(scope);
        let zs_key = v8::String::new(scope, "zeroship").unwrap();
        let zeroship = global
            .get(scope, zs_key.into())
            .and_then(|v| v8::Local::<v8::Object>::try_from(v).ok())
            .expect("zeroship namespace must exist after register_plugins");

        let auth = v8::Object::new(scope);

        let get_user_fn = v8::Function::new(scope, crate::auth::get_user_callback).unwrap();
        let get_user_key = v8::String::new(scope, "getUser").unwrap();
        auth.set(scope, get_user_key.into(), get_user_fn.into());

        let require_user_fn = v8::Function::new(scope, crate::auth::require_user_callback).unwrap();
        let require_user_key = v8::String::new(scope, "requireUser").unwrap();
        auth.set(scope, require_user_key.into(), require_user_fn.into());

        let auth_key = v8::String::new(scope, "auth").unwrap();
        zeroship.set(scope, auth_key.into(), auth.into());
    }

    // Freeze the entire zeroship namespace (plugins + built-ins)
    crate::plugin::freeze_zeroship(scope);

    // Load polyfills
    for polyfill in [FETCH_JS, URL_JS, CRYPTO_JS, STREAMS_JS, EVENTS_JS, BLOB_JS, FORMDATA_JS, WEBSOCKET_JS] {
        let code = v8::String::new(scope, polyfill).unwrap();
        let script = v8::Script::compile(scope, code, None).unwrap();
        script.run(scope).unwrap();
    }

    // Install the async-generator wrapper helper + __register before the
    // entry module evaluates, because module top-level code (e.g. a transformed
    // "use server" file's `__register("src/index/ping", ping)` footer) runs
    // during load_modules and needs both globals already present.
    {
        let code = v8::String::new(scope, WRAP_ASYNC_GENERATOR_JS).unwrap();
        let script = v8::Script::compile(scope, code, None).unwrap();
        script.run(scope).unwrap();
    }

    // Seed the RPC registry as a plain object so `__register` side effects
    // from user modules land somewhere before module exports are merged in.
    {
        let global = scope.get_current_context().global(scope);
        let rpc_key = v8::String::new(scope, "__rpc").unwrap();
        let initial = v8::Object::new(scope);
        global.set(scope, rpc_key.into(), initial.into());
    }

    // Load ES modules and merge exports into `__rpc`. Module Namespace
    // objects are V8 exotic objects with slower property access (live
    // binding resolution per lookup). Copying to a plain object restores
    // fast inline-cached property access on the dispatch hot path.
    //
    // `__register` side effects already populated `__rpc` with path-based
    // keys (e.g. `src/index/ping`); module-export merging additionally
    // registers bare names (`ping`) so legacy tests and benchmarks that
    // don't run through the vite-plugin transform still work.
    let context = scope.get_current_context();
    match crate::modules::load_modules(scope, modules) {
        Ok(namespace) => {
            let global = context.global(scope);
            let ns_local = v8::Local::new(scope, &namespace);
            let ns_obj = ns_local.to_object(scope).unwrap();

            // Read the current __rpc (may already have __register entries
            // from module top-level `__register(...)` calls).
            let rpc_key = v8::String::new(scope, "__rpc").unwrap();
            let plain = global
                .get(scope, rpc_key.into())
                .and_then(|v| v8::Local::<v8::Object>::try_from(v).ok())
                .unwrap_or_else(|| v8::Object::new(scope));

            if let Some(names) = ns_obj.get_own_property_names(scope, Default::default()) {
                for i in 0..names.length() {
                    let key = names.get_index(scope, i).unwrap();
                    if let Some(val) = ns_obj.get(scope, key) {
                        plain.set(scope, key, val);
                    }
                }
            }

            global.set(scope, rpc_key.into(), plain.into());
        }
        Err(e) => {
            eprintln!("[v8] Module loading failed: {e}");
        }
    }

    // Compile the RPC dispatch function.
    let code = v8::String::new(scope, DISPATCH_JS).unwrap();
    let script = v8::Script::compile(scope, code, None).unwrap();
    let result = script.run(scope).unwrap();
    let func = v8::Local::<v8::Function>::try_from(result).unwrap();
    v8::Global::new(scope, func)
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

/// `__zs_env()` — returns the JSON.parsed env snapshot.
///
/// Stashed on `RuntimeState.env_json` by `call_fetch_handler` before
/// dispatch. JS wraps this via `const env = Object.freeze(__zs_env());`
/// in the zeroship module so SDK code can read `env.*` without threading
/// it through every call. Returns the same data as the 2nd argument of
/// `fetch(request, env, ctx)`.
fn zs_env_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let json = state.borrow().env_json.clone();
    match v8::String::new(scope, &json) {
        Some(s) => match v8::json::parse(scope, s) {
            Some(val) => rv.set(val),
            None => rv.set(v8::Object::new(scope).into()),
        },
        None => rv.set(v8::Object::new(scope).into()),
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
