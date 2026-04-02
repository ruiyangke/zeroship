//! Per-request V8 isolate with persistent context.
//!
//! Context and compiled code persist across requests (like workerd).
//! Each request enters the existing context, calls a pre-stored handler.
//! CPU time measured per-request via CLOCK_THREAD_CPUTIME_ID.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use crate::event_loop::{run_event_loop, run_event_loop_until_settled, EventLoopState, SharedState};
use crate::globals::setup_globals;
use crate::modules::ModuleEntry;
use crate::runtime::{thread_cpu_time, RequestResult, DISPATCH_JS, FETCH_JS};

/// How the app's code is provided.
#[derive(Clone)]
pub(crate) enum AppCode {
    /// Legacy: a script that sets `var __rpc = { ... }`.
    Script(String),
    /// New: ES modules with `export function ...`.
    Modules(Vec<ModuleEntry>),
}

/// A V8 isolate with persistent context -- compiled code stays across requests.
/// Server JS is compiled ONCE. Each request just calls the handler function.
pub struct Isolate {
    isolate: v8::OwnedIsolate,
    context: v8::Global<v8::Context>,
    dispatch_fn: Option<v8::Global<v8::Function>>,
    initialized: bool,
    app_code: AppCode,
    state: SharedState,
}

impl Isolate {
    /// Create a V8 isolate and context with common setup.
    fn create_raw(app_code: AppCode) -> Self {
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

        let state: SharedState = Rc::new(RefCell::new(EventLoopState::new()));
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
            initialized: false,
            app_code,
            state,
        }
    }

    /// Create a new isolate for legacy script format (`var __rpc = { ... }`).
    /// Call `init_v8()` before creating isolates.
    pub fn new(server_js: &str) -> Self {
        Self::create_raw(AppCode::Script(server_js.to_string()))
    }

    /// Create a new isolate for ES module format (`export function ...`).
    /// Call `init_v8()` before creating isolates.
    pub fn from_modules(modules: Vec<ModuleEntry>) -> Self {
        Self::create_raw(AppCode::Modules(modules))
    }

    /// Lazy initialization: load server JS + compile dispatch function (once).
    fn ensure_initialized(&mut self) {
        if self.initialized {
            return;
        }

        // Clone app_code so we don't borrow self during V8 scope
        let app_code = self.app_code.clone();

        v8::scope!(let handle_scope, &mut self.isolate);
        let context = v8::Local::new(handle_scope, &self.context);
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        setup_globals(scope);

        // Load Fetch API polyfill (works for both script and module modes)
        {
            let fetch_code = v8::String::new(scope, FETCH_JS).unwrap();
            let fetch_script = v8::Script::compile(scope, fetch_code, None).unwrap();
            fetch_script.run(scope).unwrap();
        }

        match &app_code {
            AppCode::Script(server_js) => {
                if !server_js.is_empty() {
                    let code = v8::String::new(scope, server_js).unwrap();
                    if let Some(script) = v8::Script::compile(scope, code, None) {
                        if script.run(scope).is_none() {
                            eprintln!("[v8] Server JS execution failed (syntax/runtime error)");
                        }
                    } else {
                        eprintln!("[v8] Server JS compilation failed (syntax error)");
                    }
                }
            }
            AppCode::Modules(modules) => {
                // Load ES modules and bind namespace to globalThis.__rpc
                // so the existing DISPATCH_JS works unchanged.
                match crate::modules::load_modules(scope, modules) {
                    Ok(namespace) => {
                        let global = context.global(scope);
                        let rpc_key = v8::String::new(scope, "__rpc").unwrap();
                        let ns_local = v8::Local::new(scope, &namespace);
                        global.set(scope, rpc_key.into(), ns_local);
                    }
                    Err(e) => {
                        eprintln!("[v8] Module loading failed: {e}");
                    }
                }
            }
        }

        let code = v8::String::new(scope, DISPATCH_JS).unwrap();
        let script = v8::Script::compile(scope, code, None).unwrap();
        let result = script.run(scope).unwrap();
        let func = v8::Local::<v8::Function>::try_from(result).unwrap();
        self.dispatch_fn = Some(v8::Global::new(scope, func));

        self.initialized = true;
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

// ---------------------------------------------------------------------------
// Isolate pool
// ---------------------------------------------------------------------------

/// Pool of V8 isolates for per-request model.
/// Each isolate has a persistent context with pre-compiled handlers.
pub struct IsolatePool {
    available: std::sync::Mutex<Vec<Isolate>>,
    app_code: AppCode,
    max_size: usize,
}

// SAFETY: Isolates are only accessed by one thread at a time via the Mutex.
#[allow(unsafe_code)]
unsafe impl Send for IsolatePool {}
#[allow(unsafe_code)]
unsafe impl Sync for IsolatePool {}

impl IsolatePool {
    pub fn new(server_js: &str, max_size: usize) -> Self {
        Self {
            available: std::sync::Mutex::new(Vec::new()),
            app_code: AppCode::Script(server_js.to_string()),
            max_size,
        }
    }

    pub fn from_modules(modules: Vec<ModuleEntry>, max_size: usize) -> Self {
        Self {
            available: std::sync::Mutex::new(Vec::new()),
            app_code: AppCode::Modules(modules),
            max_size,
        }
    }

    pub fn execute(&self, request_json: &str) -> Result<RequestResult, String> {
        let mut isolate = {
            let mut pool = self.available.lock().unwrap();
            pool.pop()
        }
        .unwrap_or_else(|| Isolate::create_raw(self.app_code.clone()));

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
