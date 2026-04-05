//! V8 platform initialization, global bindings, and shared constants.
//!
//! Consolidates everything needed to boot an isolate:
//! - `init_v8()` — one-time V8 platform init
//! - `setup_globals()` — console, timers, fetch, URL, KV, crypto, env, streams
//! - Polyfill constants (`FETCH_JS`, `URL_JS`, `CRYPTO_JS`, `STREAMS_JS`)
//! - Dispatch scripts (`DISPATCH_JS`, `HTTP_DISPATCH_JS`)
//! - Result types (`RequestResult`, `HttpResult`)

use std::cmp::Reverse;
use std::time::Duration;

use crate::event_loop::SharedState;
use crate::timers::{TimerCallback, TimerHeapEntry};

// ===========================================================================
// V8 platform init
// ===========================================================================

/// Initialize V8 (safe to call multiple times).
pub fn init_v8() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
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
pub(crate) fn thread_cpu_time() -> Duration {
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
pub(crate) const FETCH_JS: &str = include_str!("embed/fetch.js");

/// Embedded URL/URLSearchParams polyfill backed by ada-url native parser.
pub(crate) const URL_JS: &str = include_str!("embed/url.js");

/// Embedded crypto polyfill (getRandomValues, SubtleCrypto.digest, base64 helpers).
pub(crate) const CRYPTO_JS: &str = include_str!("embed/crypto.js");

/// Embedded ReadableStream polyfill (backed by native __streams callbacks).
pub(crate) const STREAMS_JS: &str = include_str!("embed/streams.js");

/// HTTP dispatch function — calls onRequest(Request) if exported.
/// Returns a JSON string with { status, headers, body } or null if onRequest is not defined.
pub(crate) const HTTP_DISPATCH_JS: &str = r#"(function(__method, __url, __headers_json, __body) {
    var handler = globalThis.__rpc && globalThis.__rpc.onRequest;
    if (!handler || typeof handler !== 'function') return null;

    try {
        var hdrs = __headers_json ? JSON.parse(__headers_json) : [];
        var reqInit = { method: __method, headers: hdrs };
        if (__body && __method !== "GET" && __method !== "HEAD") reqInit.body = __body;
        var req = new Request(__url, reqInit);

        var result = handler(req);
        if (result && typeof result.then === 'function') {
            return result.then(function(resp) {
                return resp.text().then(function(body) {
                    var respHeaders = [];
                    resp.headers.forEach(function(v, k) { respHeaders.push([k, v]); });
                    return JSON.stringify({ status: resp.status, headers: respHeaders, body: body });
                });
            }, function(e) {
                return JSON.stringify({ status: 500, headers: [], body: e.message || String(e) });
            });
        }
        // Sync Response
        if (result && result.status !== undefined) {
            var respHeaders = [];
            result.headers.forEach(function(v, k) { respHeaders.push([k, v]); });
            return result.text().then(function(body) {
                return JSON.stringify({ status: result.status, headers: respHeaders, body: body });
            });
        }
        return JSON.stringify({ status: 200, headers: [], body: String(result) });
    } catch(e) {
        return JSON.stringify({ status: 500, headers: [], body: e.message || String(e) });
    }
})"#;

/// The JSON-RPC dispatch function compiled once and reused for every request.
/// Handles both sync and async (Promise-returning) handlers.
pub(crate) const DISPATCH_JS: &str = r#"(function(__req_json) {
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
pub(crate) fn load_polyfills_and_modules(
    scope: &mut v8::PinScope,
    modules: &[crate::modules::ModuleEntry],
) -> v8::Global<v8::Function> {
    setup_globals(scope);

    // Load polyfills
    for polyfill in [FETCH_JS, URL_JS, CRYPTO_JS, STREAMS_JS] {
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
        .expect("EventLoopInner not in isolate slot")
        .clone();
    let mut s = state.borrow_mut();
    s.log_buffer.push(line);
    if s.log_buffer.len() > 1000 {
        let drain = s.log_buffer.len() - 1000;
        s.log_buffer.drain(..drain);
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
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("EventLoopInner not in isolate slot")
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
    let mut s = state.borrow_mut();
    let id = s.timers.next_id;
    s.timers.next_id += 1;

    let delay = Duration::from_millis(u64::from(ms));

    s.timers.heap.push(Reverse(TimerHeapEntry {
        fire_at: std::time::Instant::now() + delay,
        id,
    }));
    s.timers.callbacks.insert(
        id,
        TimerCallback {
            callback: global_cb,
            interval: None,
        },
    );

    rv.set(v8::Integer::new(scope, id as i32).into());
}

fn clear_timeout_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("EventLoopInner not in isolate slot")
        .clone();

    let id = if args.length() > 0 {
        args.get(0).uint32_value(scope).unwrap_or(0)
    } else {
        return;
    };

    state.borrow_mut().timers.callbacks.remove(&id);
}

fn set_interval_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("EventLoopInner not in isolate slot")
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
    let dur = Duration::from_millis(u64::from(ms));

    let global_cb = v8::Global::new(scope, callback);
    let mut s = state.borrow_mut();
    let id = s.timers.next_id;
    s.timers.next_id += 1;

    s.timers.heap.push(Reverse(TimerHeapEntry {
        fire_at: std::time::Instant::now() + dur,
        id,
    }));
    s.timers.callbacks.insert(
        id,
        TimerCallback {
            callback: global_cb,
            interval: Some(dur),
        },
    );

    rv.set(v8::Integer::new(scope, id as i32).into());
}

// ===========================================================================
// Setup all globals on a V8 context
// ===========================================================================

/// Install console, timers, fetch, URL, KV, crypto, env on the global object.
///
/// Callbacks from `#[appbase_op]` modules are referenced as `crate::{mod}::{fn}_callback`.
pub(crate) fn setup_globals(scope: &mut v8::PinScope) {
    let global = scope.get_current_context().global(scope);

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

    // kv namespace
    {
        let kv = v8::Object::new(scope);

        let get_fn = v8::Function::new(scope, crate::kv::kv_get_callback).unwrap();
        let get_key = v8::String::new(scope, "get").unwrap();
        kv.set(scope, get_key.into(), get_fn.into());

        let set_fn = v8::Function::new(scope, crate::kv::kv_set_callback).unwrap();
        let set_key = v8::String::new(scope, "set").unwrap();
        kv.set(scope, set_key.into(), set_fn.into());

        let del_fn = v8::Function::new(scope, crate::kv::kv_delete_callback).unwrap();
        let del_key = v8::String::new(scope, "delete").unwrap();
        kv.set(scope, del_key.into(), del_fn.into());

        let list_fn = v8::Function::new(scope, crate::kv::kv_list_callback).unwrap();
        let list_key = v8::String::new(scope, "list").unwrap();
        kv.set(scope, list_key.into(), list_fn.into());

        let kv_key = v8::String::new(scope, "kv").unwrap();
        global.set(scope, kv_key.into(), kv.into());
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

    // env namespace
    {
        let env = v8::Object::new(scope);

        let get_fn = v8::Function::new(scope, crate::env::env_get_callback).unwrap();
        let get_key = v8::String::new(scope, "get").unwrap();
        env.set(scope, get_key.into(), get_fn.into());

        let env_key = v8::String::new(scope, "env").unwrap();
        global.set(scope, env_key.into(), env.into());
    }
}
