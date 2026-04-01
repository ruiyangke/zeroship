//! Raw V8 isolate runtime -- per-request model with persistent context.
//!
//! Context and compiled code persist across requests (like workerd).
//! Each request enters the existing context, calls a pre-stored handler.
//! CPU time measured per-request via CLOCK_THREAD_CPUTIME_ID.

#![allow(unsafe_code)]

use std::time::{Duration, Instant};

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

/// Result of executing a JS request.
#[derive(Debug)]
pub struct RequestResult {
    pub json: String,
    pub cpu_time: Duration,
    pub wall_time: Duration,
}

/// A V8 isolate with persistent context -- compiled code stays across requests.
/// Server JS is compiled ONCE. Each request just calls the handler function.
pub struct Isolate {
    isolate: v8::OwnedIsolate,
    context: v8::Global<v8::Context>,
    /// Pre-compiled dispatch script source (avoids recompilation).
    dispatch_fn: Option<v8::Global<v8::Function>>,
    initialized: bool,
    server_js: String,
}

impl Isolate {
    /// Create a new isolate and load server JS (compiled once).
    pub fn new(server_js: &str) -> Self {
        let params = v8::CreateParams::default().heap_limits(0, 128 * 1024 * 1024);
        let mut isolate = v8::Isolate::new(params);

        // Create persistent context
        let context = {
            v8::scope!(let handle_scope, &mut isolate);
            let ctx = v8::Context::new(handle_scope, Default::default());
            v8::Global::new(handle_scope, ctx)
        };

        Self {
            isolate,
            context,
            dispatch_fn: None,
            initialized: false,
            server_js: server_js.to_string(),
        }
    }

    /// Initialize: load server JS + compile dispatch function (once).
    fn ensure_initialized(&mut self) {
        if self.initialized {
            return;
        }

        v8::scope!(let handle_scope, &mut self.isolate);
        let context = v8::Local::new(handle_scope, &self.context);
        let scope = &v8::ContextScope::new(handle_scope, context);

        // Load server JS (registers __rpc handlers)
        if !self.server_js.is_empty() {
            let code = v8::String::new(scope, &self.server_js).unwrap();
            let script = v8::Script::compile(scope, code, None).unwrap();
            script.run(scope).unwrap();
        }

        // Compile the dispatch function ONCE -- reused for every request
        let dispatch_src = r#"(function(__req_json) {
            var req = JSON.parse(__req_json);
            var fn = __rpc[req.method];
            if (!fn) return JSON.stringify({jsonrpc:"2.0",error:{code:-32601,message:"not found"},id:req.id});
            try {
                var result = fn.apply(null, req.params || []);
                return JSON.stringify({jsonrpc:"2.0",result:result,id:req.id});
            } catch(e) {
                return JSON.stringify({jsonrpc:"2.0",error:{code:-32000,message:e.message},id:req.id});
            }
        })"#;

        let code = v8::String::new(scope, dispatch_src).unwrap();
        let script = v8::Script::compile(scope, code, None).unwrap();
        let result = script.run(scope).unwrap();
        let func = v8::Local::<v8::Function>::try_from(result).unwrap();
        self.dispatch_fn = Some(v8::Global::new(scope, func));

        self.initialized = true;
    }

    /// Execute a single RPC request -- enters persistent context, calls pre-compiled function.
    pub fn execute_request(&mut self, request_json: &str) -> Result<RequestResult, String> {
        self.ensure_initialized();

        let wall_start = Instant::now();
        let cpu_start = thread_cpu_time();

        v8::scope!(let handle_scope, &mut self.isolate);
        let context = v8::Local::new(handle_scope, &self.context);
        let scope = &v8::ContextScope::new(handle_scope, context);

        // Call the pre-compiled dispatch function with request JSON
        let dispatch_fn = self.dispatch_fn.as_ref().unwrap();
        let func = v8::Local::new(scope, dispatch_fn);

        let arg = v8::String::new(scope, request_json)
            .ok_or("Failed to create arg string")?;
        let undefined = v8::undefined(scope).into();

        let result = func
            .call(scope, undefined, &[arg.into()])
            .ok_or("Dispatch call failed")?;

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
}

/// Pool of V8 isolates for per-request model.
/// Each isolate has a persistent context with pre-compiled handlers.
pub struct IsolatePool {
    available: std::sync::Mutex<Vec<Isolate>>,
    server_js: String,
    max_size: usize,
}

// SAFETY: Isolates are only accessed by one thread at a time via the Mutex.
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
        .unwrap_or_else(|| Isolate::new(&self.server_js));

        let result = isolate.execute_request(request_json);

        {
            let mut pool = self.available.lock().unwrap();
            if pool.len() < self.max_size {
                pool.push(isolate);
            }
        }

        result
    }
}

/// Convenience: create an isolate and execute directly.
pub fn execute_request(
    server_js: &str,
    request_json: &str,
) -> Result<RequestResult, String> {
    let mut isolate = Isolate::new(server_js);
    isolate.execute_request(request_json)
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
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"ping","params":[],"id":1}"#)
            .unwrap();
        println!("ping: {} cpu={:.3}ms", r.json, r.cpu_time.as_secs_f64() * 1000.0);
        assert!(r.json.contains("pong"));
    }

    #[test]
    fn persistent_context() {
        init_v8();
        let js = r#"var __rpc = { count: (function() { var n=0; return function() { return ++n; }; })() };"#;
        let mut isolate = Isolate::new(js);

        let r1 = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"count","params":[],"id":1}"#)
            .unwrap();
        let r2 = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"count","params":[],"id":2}"#)
            .unwrap();
        let r3 = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"count","params":[],"id":3}"#)
            .unwrap();

        // State persists across requests (same context)
        assert!(r1.json.contains("\"result\":1"));
        assert!(r2.json.contains("\"result\":2"));
        assert!(r3.json.contains("\"result\":3"));
        println!("Context persists: 1→2→3 across 3 requests");
    }

    #[test]
    fn per_request_cpu() {
        init_v8();
        let js = r#"
            var __rpc = {
                fib: function(n) {
                    function f(n) { return n <= 1 ? n : f(n-1) + f(n-2); }
                    return f(n);
                }
            };
        "#;
        let mut isolate = Isolate::new(js);
        let r1 = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"fib","params":[20],"id":1}"#)
            .unwrap();
        let r2 = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"fib","params":[35],"id":2}"#)
            .unwrap();
        println!("fib(20): cpu={:.3}ms", r1.cpu_time.as_secs_f64() * 1000.0);
        println!("fib(35): cpu={:.3}ms", r2.cpu_time.as_secs_f64() * 1000.0);
        assert!(r2.cpu_time > r1.cpu_time * 5);
    }

    #[test]
    fn pool_reuse() {
        init_v8();
        let js = r#"var __rpc = { ping: function() { return "pong"; } };"#;
        let pool = IsolatePool::new(js, 4);
        for i in 0..10 {
            let r = pool
                .execute(&format!(
                    r#"{{"jsonrpc":"2.0","method":"ping","params":[],"id":{i}}}"#
                ))
                .unwrap();
            assert!(r.json.contains("pong"));
        }
    }
}
