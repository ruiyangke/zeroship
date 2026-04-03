//! Per-request V8 isolate with persistent context.
//!
//! Context and compiled code persist across requests (like workerd).
//! Each request enters the existing context, calls a pre-stored handler.
//! CPU time measured per-request via CLOCK_THREAD_CPUTIME_ID.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use std::collections::HashMap;

use crate::event_loop::{run_event_loop, run_event_loop_until_settled, EventLoopState, SharedState};
use crate::globals::setup_globals;
use crate::modules::ModuleEntry;
use crate::runtime::{thread_cpu_time, HttpResult, RequestResult, DISPATCH_JS, FETCH_JS, URL_JS, CRYPTO_JS};

/// A V8 isolate with persistent context -- compiled code stays across requests.
/// ES modules are compiled ONCE. Each request just calls the handler function.
pub struct Isolate {
    isolate: v8::OwnedIsolate,
    context: v8::Global<v8::Context>,
    dispatch_fn: Option<v8::Global<v8::Function>>,
    /// Cached direct handle to the user's `onRequest` function.
    on_request_fn: Option<v8::Global<v8::Function>>,
    /// Single JS dispatch: `(handler, method, url, headers_json, body) → Response object`
    /// Handler is passed as first arg — no property lookup needed.
    http_dispatch_fn: Option<v8::Global<v8::Function>>,
    initialized: bool,
    modules: Vec<ModuleEntry>,
    state: SharedState,
}

impl Isolate {
    /// Create a new isolate for ES module format (`export function ...`).
    /// Call `init_v8()` before creating isolates.
    ///
    /// `env_vars` — per-app environment variables accessible via `env.get(key)`.
    pub fn new(modules: Vec<ModuleEntry>, env_vars: HashMap<String, String>) -> Self {
        let params = v8::CreateParams::default().heap_limits(0, 128 * 1024 * 1024);
        let mut isolate = v8::Isolate::new(params);

        // Register near-heap-limit callback to prevent OOM crashes
        unsafe extern "C" fn near_heap_limit_callback(
            _data: *mut std::ffi::c_void,
            current_heap_limit: usize,
            _initial_heap_limit: usize,
        ) -> usize {
            eprintln!(
                "[v8] Near heap limit: {}MB, not increasing",
                current_heap_limit / 1024 / 1024
            );
            current_heap_limit
        }
        isolate.add_near_heap_limit_callback(near_heap_limit_callback, std::ptr::null_mut());

        let state: SharedState = Rc::new(RefCell::new(EventLoopState::with_env(env_vars)));
        state.borrow_mut().tokio_handle = tokio::runtime::Handle::try_current().ok();
        isolate.set_slot(state.clone());

        let context = {
            v8::scope!(let handle_scope, &mut isolate);
            let ctx = v8::Context::new(handle_scope, Default::default());
            v8::Global::new(handle_scope, ctx)
        };

        Self {
            isolate,
            context,
            dispatch_fn: None,
            on_request_fn: None,
            http_dispatch_fn: None,
            initialized: false,
            modules,
            state,
        }
    }

    /// Lazy initialization: load ES modules + compile dispatch function (once).
    fn ensure_initialized(&mut self) {
        if self.initialized {
            return;
        }

        // Clone modules so we don't borrow self during V8 scope
        let modules = self.modules.clone();

        v8::scope!(let handle_scope, &mut self.isolate);
        let context = v8::Local::new(handle_scope, &self.context);
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        setup_globals(scope);

        // Load polyfills
        for polyfill in [FETCH_JS, URL_JS, CRYPTO_JS] {
            let code = v8::String::new(scope, polyfill).unwrap();
            let script = v8::Script::compile(scope, code, None).unwrap();
            script.run(scope).unwrap();
        }

        // Load ES modules and copy exports to a plain object on globalThis.__rpc.
        // Module Namespace objects are V8 exotic objects with slower property access
        // (live binding resolution per lookup). Copying to a plain object restores
        // fast inline-cached property access on the dispatch hot path.
        match crate::modules::load_modules(scope, &modules) {
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
        self.dispatch_fn = Some(v8::Global::new(scope, func));

        // Cache onRequest handler directly as Global<Function> (if exported).
        // Eliminates the __rpc.onRequest property lookup on every request.
        {
            let global = context.global(scope);
            let rpc_key = v8::String::new(scope, "__rpc").unwrap();
            if let Some(rpc_obj) = global.get(scope, rpc_key.into()) {
                let on_request_key = v8::String::new(scope, "onRequest").unwrap();
                if let Some(handler) = rpc_obj.to_object(scope).and_then(|obj| obj.get(scope, on_request_key.into())) {
                    if handler.is_function() {
                        let func = v8::Local::<v8::Function>::try_from(handler).unwrap();
                        self.on_request_fn = Some(v8::Global::new(scope, func));
                    }
                }
            }
        }

        // Compile optimized HTTP dispatch function:
        //   1. Trusted headers: build _map directly, skip validateName/validateValue
        //   2. Sync body read: resp._bodyText directly, not resp.text().then()
        //   3. Direct _map access: read resp.headers._map, skip forEach
        //   4. Handler as arg: no property lookup
        // Note: Object.create + defineProperty for lazy Request was SLOWER (causes V8
        // dictionary mode transition). Using new Request() with trusted headers instead.
        if self.on_request_fn.is_some() {
            let code = v8::String::new(scope, r#"(function(__handler, __method, __url, __headers_json, __body) {
    try {
        // Build trusted headers map — skip validation (like workerd's appendUnguarded).
        // Inbound headers from Hyper are already validated.
        var map = Object.create(null);
        if (__headers_json) {
            var arr = JSON.parse(__headers_json);
            for (var i = 0; i < arr.length; i++) {
                var k = arr[i][0].toLowerCase(), v = arr[i][1];
                if (map[k]) map[k].push(v); else map[k] = [v];
            }
        }
        var reqInit = { method: __method, headers: Headers._fromTrusted(map) };
        if (__body && __method !== "GET" && __method !== "HEAD") reqInit.body = __body;
        var req = new Request(__url, reqInit);

        var result = __handler(req);

        // Return Response object directly — Rust reads properties via V8 API.
        // No JSON.stringify/parse on the hot path.
        if (result && typeof result.then === "function") {
            return result.then(function(resp) {
                if (!resp || resp.status === undefined) return new Response(String(resp), { status: 200 });
                return resp;
            }, function(e) {
                return new Response(e.message || String(e), { status: 500 });
            });
        }
        if (!result || result.status === undefined) {
            return new Response(String(result), { status: 200 });
        }
        return result;
    } catch(e) {
        return new Response(e.message || String(e), { status: 500 });
    }
})"#).unwrap();
            let s = v8::Script::compile(scope, code, None).unwrap();
            let r = s.run(scope).unwrap();
            self.http_dispatch_fn = Some(v8::Global::new(scope, v8::Local::<v8::Function>::try_from(r).unwrap()));
        }

        self.initialized = true;
    }

    /// Check if the app exports an onRequest handler.
    pub fn has_http_handler(&mut self) -> bool {
        self.ensure_initialized();
        self.on_request_fn.is_some()
    }

    /// Execute an HTTP request via cached onRequest `Global<Function>`.
    /// Single Rust→JS call: handler passed as first arg, no property lookup.
    /// Returns None if onRequest is not exported.
    pub fn execute_http(
        &mut self,
        method: &str,
        url: &str,
        headers_json: &str,
        body: &str,
    ) -> Option<Result<HttpResult, String>> {
        self.ensure_initialized();

        if self.on_request_fn.is_none() {
            return None;
        }

        // Drain stale state
        {
            let mut s = self.state.borrow_mut();
            s.timers.heap.clear();
            s.timers.callbacks.clear();
            while s.op_rx.try_recv().is_ok() {}
        }

        let wall_start = std::time::Instant::now();
        let cpu_start = thread_cpu_time();

        v8::scope!(let handle_scope, &mut self.isolate);
        let context = v8::Local::new(handle_scope, &self.context);
        let scope = &mut v8::ContextScope::new(handle_scope, context);
        let undefined = v8::undefined(scope).into();

        // Single JS call: dispatch_fn(handler, method, url, headers_json, body)
        let dispatch = v8::Local::new(scope, self.http_dispatch_fn.as_ref().unwrap());
        let handler = v8::Local::new(scope, self.on_request_fn.as_ref().unwrap());
        let arg_method = v8::String::new(scope, method).unwrap();
        let arg_url = v8::String::new(scope, url).unwrap();
        let arg_headers = v8::String::new(scope, headers_json).unwrap();
        let arg_body = v8::String::new(scope, body).unwrap();

        let result = dispatch.call(scope, undefined, &[
            handler.into(), arg_method.into(), arg_url.into(), arg_headers.into(), arg_body.into()
        ]);

        // Resolve the dispatch result to a V8 Response object.
        let resp_val = match result {
            Some(val) if val.is_null_or_undefined() => return None,
            Some(val) if val.is_promise() => {
                let promise = v8::Local::<v8::Promise>::try_from(val).unwrap();
                let global_promise = v8::Global::new(scope, promise);
                run_event_loop_until_settled(scope, &self.state, &global_promise);
                let promise = v8::Local::new(scope, &global_promise);
                match promise.state() {
                    v8::PromiseState::Fulfilled => promise.result(scope),
                    v8::PromiseState::Rejected => {
                        let msg = promise.result(scope).to_rust_string_lossy(scope);
                        return Some(Err(format!("onRequest rejected: {msg}")));
                    }
                    v8::PromiseState::Pending => return Some(Err("onRequest promise still pending".into())),
                }
            }
            Some(val) => val,
            None => return Some(Err("onRequest call failed".into())),
        };

        // Extract response fields directly from V8 object — no JSON serialization.
        Some(extract_http_result(scope, resp_val, cpu_start, wall_start, &self.state))
    }

    /// Execute a single RPC request. Returns the JSON response + timing info.
    pub fn execute_request(&mut self, request_json: &str) -> Result<RequestResult, String> {
        self.ensure_initialized();

        // Drain stale timer state from prior requests
        {
            let mut s = self.state.borrow_mut();
            s.timers.heap.clear();
            s.timers.callbacks.clear();
            while s.op_rx.try_recv().is_ok() {}
        }

        let wall_start = Instant::now();
        let cpu_start = thread_cpu_time();

        v8::scope!(let handle_scope, &mut self.isolate);
        let context = v8::Local::new(handle_scope, &self.context);
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        let dispatch_fn = self.dispatch_fn.as_ref().unwrap();
        let func = v8::Local::new(scope, dispatch_fn);

        let arg = v8::String::new(scope, request_json).ok_or("Failed to create arg string")?;
        let undefined = v8::undefined(scope).into();

        let result = func
            .call(scope, undefined, &[arg.into()])
            .ok_or("Dispatch call failed")?;

        let json = if result.is_promise() {
            let promise = v8::Local::<v8::Promise>::try_from(result)
                .map_err(|e| format!("Promise cast failed: {e}"))?;
            let global_promise = v8::Global::new(scope, promise);

            run_event_loop_until_settled(scope, &self.state, &global_promise);

            let promise = v8::Local::new(scope, &global_promise);
            match promise.state() {
                v8::PromiseState::Fulfilled => {
                    let value = promise.result(scope);
                    let s = value
                        .to_string(scope)
                        .ok_or("Failed to stringify promise result")?;
                    s.to_rust_string_lossy(scope)
                }
                v8::PromiseState::Rejected => {
                    let value = promise.result(scope);
                    let s = value.to_string(scope).ok_or("Failed to stringify rejection")?;
                    let msg = s.to_rust_string_lossy(scope);
                    return Err(format!("Promise rejected: {msg}"));
                }
                v8::PromiseState::Pending => {
                    return Err("Promise still pending after event loop exhausted".into());
                }
            }
        } else {
            let json_v8 = result.to_string(scope).ok_or("Failed to stringify")?;
            let json = json_v8.to_rust_string_lossy(scope);

            // Run any pending timers/ops (fire-and-forget side effects)
            run_event_loop(scope, &self.state);

            json
        };

        let cpu_time = thread_cpu_time().saturating_sub(cpu_start);
        let wall_time = wall_start.elapsed();
        let logs = self.state.borrow_mut().log_buffer.drain(..).collect();

        Ok(RequestResult {
            json,
            cpu_time,
            wall_time,
            logs,
        })
    }
}

/// Extract HTTP response fields directly from a V8 Response object.
/// Reads `status`, `headers._map`, and `_bodyText` via the V8 API,
/// avoiding JSON.stringify on the JS side and serde_json::from_str on Rust side.
fn extract_http_result(
    scope: &mut v8::PinScope,
    resp_val: v8::Local<v8::Value>,
    cpu_start: std::time::Duration,
    wall_start: std::time::Instant,
    state: &SharedState,
) -> Result<HttpResult, String> {
    let resp = resp_val.to_object(scope).ok_or("Response is not an object")?;

    // status
    let status_key = v8::String::new(scope, "status").unwrap();
    let status = resp.get(scope, status_key.into())
        .and_then(|v| v.uint32_value(scope))
        .unwrap_or(200) as u16;

    // body (read _bodyText directly — avoids the async .text() Promise chain)
    let body_key = v8::String::new(scope, "_bodyText").unwrap();
    let body = resp.get(scope, body_key.into())
        .map(|v| v.to_rust_string_lossy(scope))
        .unwrap_or_default();

    // headers (read _map directly from headers object)
    let headers_key = v8::String::new(scope, "headers").unwrap();
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(headers_obj) = resp.get(scope, headers_key.into()) {
        if let Some(headers_obj) = headers_obj.to_object(scope) {
            let map_key = v8::String::new(scope, "_map").unwrap();
            if let Some(map_val) = headers_obj.get(scope, map_key.into()) {
                if let Some(map_obj) = map_val.to_object(scope) {
                    if let Some(names) = map_obj.get_own_property_names(scope, Default::default()) {
                        for i in 0..names.length() {
                            let key = names.get_index(scope, i).unwrap();
                            let key_str = key.to_rust_string_lossy(scope);
                            if let Some(val_arr) = map_obj.get(scope, key) {
                                // Each value in _map is an array of strings
                                if let Ok(arr) = v8::Local::<v8::Array>::try_from(val_arr) {
                                    for j in 0..arr.length() {
                                        if let Some(v) = arr.get_index(scope, j) {
                                            headers.push((key_str.clone(), v.to_rust_string_lossy(scope)));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let logs = state.borrow_mut().log_buffer.drain(..).collect();

    Ok(HttpResult {
        status,
        headers,
        body,
        cpu_time: thread_cpu_time().saturating_sub(cpu_start),
        wall_time: wall_start.elapsed(),
        logs,
    })
}

// ---------------------------------------------------------------------------
// Isolate pool
// ---------------------------------------------------------------------------

/// Pool of V8 isolates for per-request model.
/// Each isolate has a persistent context with pre-compiled handlers.
pub struct IsolatePool {
    available: std::sync::Mutex<Vec<Isolate>>,
    modules: Vec<ModuleEntry>,
    env_vars: HashMap<String, String>,
    max_size: usize,
}

// SAFETY: Isolates are only accessed by one thread at a time via the Mutex.
#[allow(unsafe_code)]
unsafe impl Send for IsolatePool {}
#[allow(unsafe_code)]
unsafe impl Sync for IsolatePool {}

impl IsolatePool {
    pub fn new(modules: Vec<ModuleEntry>, env_vars: HashMap<String, String>, max_size: usize) -> Self {
        Self {
            available: std::sync::Mutex::new(Vec::new()),
            modules,
            env_vars,
            max_size,
        }
    }

    pub fn execute(&self, request_json: &str) -> Result<RequestResult, String> {
        let mut isolate = {
            let mut pool = self.available.lock().unwrap();
            pool.pop()
        }
        .unwrap_or_else(|| Isolate::new(self.modules.clone(), self.env_vars.clone()));

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
