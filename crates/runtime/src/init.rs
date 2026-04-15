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

/// The JSON-RPC dispatch function compiled once and reused for every request.
/// Handles both sync and async (Promise-returning) handlers.
pub const DISPATCH_JS: &str = r#"(function(__req_json) {
    var req = JSON.parse(__req_json);
    var fn = __rpc[req.method];
    if (!fn) return JSON.stringify({jsonrpc:"2.0",error:{code:-32601,message:"not found"},id:req.id});
    try {
        var result = fn.apply(null, req.params || []);
        if (result && typeof result.then === 'function') {
            return result.then(function(v) {
                return JSON.stringify({jsonrpc:"2.0",result:v,id:req.id});
            }, function(e) {
                return JSON.stringify({jsonrpc:"2.0",error:{code:-32000,message:e && e.message ? e.message : String(e)},id:req.id});
            });
        }
        return JSON.stringify({jsonrpc:"2.0",result:result,id:req.id});
    } catch(e) {
        return JSON.stringify({jsonrpc:"2.0",error:{code:-32000,message:e.message},id:req.id});
    }
})"#;

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
    plugins: &[Box<dyn crate::plugin::NativePlugin>],
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

    // Load ES modules and copy exports to a plain object on globalThis.__rpc.
    // Module Namespace objects are V8 exotic objects with slower property access
    // (live binding resolution per lookup). Copying to a plain object restores
    // fast inline-cached property access on the dispatch hot path.
    let context = scope.get_current_context();
    match crate::modules::load_modules(scope, modules) {
        Ok(namespace) => {
            let global = context.global(scope);
            let ns_local = v8::Local::new(scope, &namespace);
            let ns_obj = ns_local.to_object(scope).unwrap();

            let plain = v8::Object::new(scope);
            if let Some(names) = ns_obj.get_own_property_names(scope, Default::default()) {
                for i in 0..names.length() {
                    let key = names.get_index(scope, i).unwrap();
                    if let Some(val) = ns_obj.get(scope, key) {
                        plain.set(scope, key, val);
                    }
                }
            }

            let rpc_key = v8::String::new(scope, "__rpc").unwrap();
            global.set(scope, rpc_key.into(), plain.into());
        }
        Err(e) => {
            eprintln!("[v8] Module loading failed: {e}");
        }
    }

    // Compile JSON-RPC dispatch function
    let code = v8::String::new(scope, DISPATCH_JS).unwrap();
    let script = v8::Script::compile(scope, code, None).unwrap();
    let result = script.run(scope).unwrap();
    let func = v8::Local::<v8::Function>::try_from(result).unwrap();
    v8::Global::new(scope, func)
}

// ===========================================================================
// Console polyfill (variadic — stays manual)
// ===========================================================================

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
    let line = parts.join(" ");
    println!("{}", line);

    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let mut s = state.borrow_mut();
    let req_id = s.executing_request_id.unwrap_or(0);
    let logs = s.per_request_logs.entry(req_id).or_default();
    logs.push(line);
    if logs.len() > 1000 {
        let drain = logs.len() - 1000;
        logs.drain(..drain);
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
static PERF_START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

fn performance_now_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let start = PERF_START.get_or_init(std::time::Instant::now);
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    rv.set(v8::Number::new(scope, elapsed_ms).into());
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
        // Fast path: fire inline without tokio::time::sleep overhead.
        s.ready_timers.push(id);
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

    // process.env polyfill — many npm packages (e.g. LangChain) read
    // `process.env.OPENAI_API_KEY`. Populate from host environment so
    // runtime detection and env-based config work out of the box.
    {
        let process = v8::Object::new(scope);
        let env_obj = v8::Object::new(scope);

        for (key, value) in std::env::vars() {
            let k = v8::String::new(scope, &key).unwrap();
            let v = v8::String::new(scope, &value).unwrap();
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
