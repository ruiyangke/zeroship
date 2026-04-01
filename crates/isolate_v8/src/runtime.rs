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

/// Result of executing a JS request.
#[derive(Debug)]
pub struct RequestResult {
    pub json: String,
    pub cpu_time: Duration,
    pub wall_time: Duration,
    /// Console output captured during execution.
    pub logs: Vec<String>,
}

/// Embedded Fetch API polyfill -- loaded after globals are set up.
pub(crate) const FETCH_JS: &str = include_str!("embed/fetch.js");

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
