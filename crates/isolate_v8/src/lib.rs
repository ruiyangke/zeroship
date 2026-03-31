//! Raw V8 isolate runtime -- per-request model.
//!
//! Each request gets a fresh V8 context on a pooled isolate.
//! CPU time measured per-request via CLOCK_THREAD_CPUTIME_ID.

#![allow(unsafe_code)]

use std::time::{Duration, Instant};

/// Initialize V8 (safe to call multiple times -- only first call has effect).
pub fn init_v8() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let platform = v8::new_default_platform(0, false).make_shared();
        v8::V8::initialize_platform(platform);
        v8::V8::initialize();
    });
}

/// Result of executing a JS request.
#[derive(Debug)]
pub struct RequestResult {
    pub json: String,
    pub cpu_time: Duration,
    pub wall_time: Duration,
}

/// Execute a single RPC request on a V8 isolate.
/// Creates a fresh context per call (like workerd per-event).
pub fn execute_request(
    isolate: &mut v8::OwnedIsolate,
    server_js: &str,
    request_json: &str,
) -> Result<RequestResult, String> {
    let wall_start = Instant::now();
    let cpu_start = thread_cpu_time();

    v8::scope!(let handle_scope, isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &v8::ContextScope::new(handle_scope, context);

    // Load server JS (registers __rpc methods)
    if !server_js.is_empty() {
        let code = v8::String::new(scope, server_js)
            .ok_or("Failed to create JS string")?;
        let script = v8::Script::compile(scope, code, None)
            .ok_or("Failed to compile server JS")?;
        script.run(scope).ok_or("Failed to run server JS")?;
    }

    // Build and run RPC dispatch
    let escaped = request_json
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "\\n");

    let dispatch = format!(
        r#"(function() {{
            var req = JSON.parse('{}');
            var fn = __rpc[req.method];
            if (!fn) return JSON.stringify({{jsonrpc:"2.0",error:{{code:-32601,message:"not found"}},id:req.id}});
            try {{
                var result = fn.apply(null, req.params || []);
                return JSON.stringify({{jsonrpc:"2.0",result:result,id:req.id}});
            }} catch(e) {{
                return JSON.stringify({{jsonrpc:"2.0",error:{{code:-32000,message:e.message}},id:req.id}});
            }}
        }})()"#,
        escaped
    );

    let code = v8::String::new(scope, &dispatch)
        .ok_or("Failed to create dispatch string")?;
    let script = v8::Script::compile(scope, code, None)
        .ok_or("Failed to compile dispatch")?;
    let result = script.run(scope).ok_or("Dispatch failed")?;
    let json_v8 = result.to_string(scope).ok_or("Failed to stringify")?;
    let json = json_v8.to_rust_string_lossy(scope);

    let cpu_time = thread_cpu_time().saturating_sub(cpu_start);
    let wall_time = wall_start.elapsed();

    Ok(RequestResult {
        json,
        cpu_time,
        wall_time,
    })
}

/// Pool of V8 isolates for per-request model.
pub struct IsolatePool {
    available: std::sync::Mutex<Vec<v8::OwnedIsolate>>,
    server_js: String,
    max_size: usize,
}

// SAFETY: V8 OwnedIsolate can be sent between threads when not in use.
// The pool only gives out isolates to one thread at a time.
unsafe impl Send for IsolatePool {}
unsafe impl Sync for IsolatePool {}

impl IsolatePool {
    pub fn new(server_js: &str, max_size: usize) -> Self {
        Self {
            available: std::sync::Mutex::new(Vec::new()),
            server_js: server_js.to_string(),
            max_size,
        }
    }

    pub fn execute(&self, request_json: &str) -> Result<RequestResult, String> {
        let mut isolate = {
            let mut pool = self.available.lock().unwrap();
            pool.pop()
        }
        .unwrap_or_else(|| {
            let params = v8::CreateParams::default().heap_limits(0, 128 * 1024 * 1024);
            v8::Isolate::new(params)
        });

        let result = execute_request(&mut isolate, &self.server_js, request_json);

        {
            let mut pool = self.available.lock().unwrap();
            if pool.len() < self.max_size {
                pool.push(isolate);
            }
        }

        result
    }
}

fn thread_cpu_time() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts);
    }
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_rpc() {
        init_v8();
        let js = r#"var __rpc = { ping: function() { return "pong"; } };"#;
        let params = v8::CreateParams::default();
        let mut isolate = v8::Isolate::new(params);
        let r = execute_request(&mut isolate, js, r#"{"jsonrpc":"2.0","method":"ping","params":[],"id":1}"#).unwrap();
        println!("ping: {} cpu={:.3}ms", r.json, r.cpu_time.as_secs_f64() * 1000.0);
        assert!(r.json.contains("pong"));
    }

    #[test]
    fn pool_works() {
        init_v8();
        let js = r#"var __rpc = { ping: function() { return "pong"; } };"#;
        let pool = IsolatePool::new(js, 4);
        for i in 0..10 {
            let r = pool.execute(&format!(
                r#"{{"jsonrpc":"2.0","method":"ping","params":[],"id":{i}}}"#
            )).unwrap();
            assert!(r.json.contains("pong"));
        }
    }
}
