//! V8 platform initialization and shared utilities.

use std::time::Duration;

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
