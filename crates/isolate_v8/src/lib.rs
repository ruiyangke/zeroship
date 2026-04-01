//! Raw V8 isolate runtime -- per-request model with persistent context.
//!
//! Context and compiled code persist across requests (like workerd).
//! Each request enters the existing context, calls a pre-stored handler.
//! CPU time measured per-request via CLOCK_THREAD_CPUTIME_ID.
//!
//! Supports async/await, setTimeout/clearTimeout, setInterval/clearInterval
//! via a channel-driven event loop backed by tokio timers (zero CPU while idle).

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

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

// ---------------------------------------------------------------------------
// Shared tokio runtime for timer/op scheduling
// ---------------------------------------------------------------------------

fn timer_runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
    })
}

// ---------------------------------------------------------------------------
// Event types for channel-driven event loop
// ---------------------------------------------------------------------------

enum Event {
    TimerFired(u32),
    #[allow(dead_code)] // Used by future fetch() support
    OpCompleted(u32, String),
}

// ---------------------------------------------------------------------------
// Event loop state
// ---------------------------------------------------------------------------

#[allow(missing_debug_implementations)]
struct Timer {
    callback: v8::Global<v8::Function>,
    interval: Option<Duration>,
    /// Handle to cancel the spawned tokio timer task
    task_handle: Option<tokio::task::JoinHandle<()>>,
}

/// State shared between V8 callbacks and the event loop driver.
#[allow(missing_debug_implementations)]
struct EventLoopState {
    timers: HashMap<u32, Timer>,
    next_timer_id: u32,
    /// Pending async op promises: id -> resolver
    pending_resolvers: HashMap<u32, v8::Global<v8::PromiseResolver>>,
    #[allow(dead_code)] // Used by future fetch() support
    next_op_id: u32,
    /// Completed ops waiting to be resolved: (id, json_value)
    completed_ops: Vec<(u32, String)>,
    /// Timer IDs ready to fire immediately (0ms delay, no channel round-trip)
    ready_timer_ids: Vec<u32>,
    /// Channel sender for event notifications (cloned into spawned tasks)
    event_tx: mpsc::Sender<Event>,
    /// Channel receiver for event notifications (used by event loop)
    event_rx: mpsc::Receiver<Event>,
}

impl EventLoopState {
    fn new() -> Self {
        let (event_tx, event_rx) = mpsc::channel(1024);
        Self {
            timers: HashMap::new(),
            next_timer_id: 1,
            pending_resolvers: HashMap::new(),
            next_op_id: 1,
            completed_ops: Vec::new(),
            ready_timer_ids: Vec::new(),
            event_tx,
            event_rx,
        }
    }
}

type SharedState = Rc<RefCell<EventLoopState>>;

/// Spawn a timer task on the shared tokio runtime.
/// Returns a `JoinHandle` that can be aborted for `clearTimeout`.
fn spawn_timer_task(
    tx: mpsc::Sender<Event>,
    timer_id: u32,
    delay: Duration,
) -> tokio::task::JoinHandle<()> {
    timer_runtime().spawn(async move {
        tokio::time::sleep(delay).await;
        let _ = tx.send(Event::TimerFired(timer_id)).await;
    })
}

// ---------------------------------------------------------------------------
// Console polyfill
// ---------------------------------------------------------------------------

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
    println!("{}", parts.join(" "));
}

// ---------------------------------------------------------------------------
// setTimeout / clearTimeout / setInterval / clearInterval callbacks
// ---------------------------------------------------------------------------

fn set_timeout_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("EventLoopState not in isolate slot")
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
    let id = s.next_timer_id;
    s.next_timer_id += 1;

    let delay = Duration::from_millis(u64::from(ms));

    if ms == 0 {
        // 0ms timer: queue directly, no channel round-trip
        s.timers.insert(
            id,
            Timer {
                callback: global_cb,
                interval: None,
                task_handle: None,
            },
        );
        s.ready_timer_ids.push(id);
    } else {
        // Non-zero timer: spawn tokio task
        let tx = s.event_tx.clone();
        let handle = spawn_timer_task(tx, id, delay);
        s.timers.insert(
            id,
            Timer {
                callback: global_cb,
                interval: None,
                task_handle: Some(handle),
            },
        );
    }
    rv.set(v8::Integer::new(scope, id as i32).into());
}

fn clear_timeout_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("EventLoopState not in isolate slot")
        .clone();

    let id = if args.length() > 0 {
        args.get(0).uint32_value(scope).unwrap_or(0)
    } else {
        return;
    };

    let mut s = state.borrow_mut();
    if let Some(timer) = s.timers.remove(&id) {
        // Abort the spawned tokio task to prevent it from firing
        if let Some(handle) = timer.task_handle {
            handle.abort();
        }
    }
}

fn set_interval_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("EventLoopState not in isolate slot")
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
    let id = s.next_timer_id;
    s.next_timer_id += 1;

    let tx = s.event_tx.clone();
    let handle = spawn_timer_task(tx, id, dur);

    s.timers.insert(
        id,
        Timer {
            callback: global_cb,
            interval: Some(dur),
            task_handle: Some(handle),
        },
    );
    rv.set(v8::Integer::new(scope, id as i32).into());
}

// ---------------------------------------------------------------------------
// Setup globals on context
// ---------------------------------------------------------------------------

fn setup_globals(scope: &mut v8::PinScope) {
    let global = scope.get_current_context().global(scope);

    // console.log
    {
        let console = v8::Object::new(scope);
        let log_fn = v8::Function::new(scope, console_log_callback).unwrap();
        let log_key = v8::String::new(scope, "log").unwrap();
        console.set(scope, log_key.into(), log_fn.into());

        // Also alias warn/error/info to log for basic compat
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
}

// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------

/// Process all pending events from the channel without blocking.
/// Marks fired timers in the state so they can be handled by V8.
fn drain_ready_events(state: &SharedState) -> Vec<Event> {
    let mut events = Vec::new();
    let mut s = state.borrow_mut();
    while let Ok(event) = s.event_rx.try_recv() {
        events.push(event);
    }
    events
}

/// Fire timer callbacks for the given fired timer IDs.
/// For setInterval timers, re-spawns the next timer task.
fn fire_timers(
    scope: &mut v8::PinScope,
    state: &SharedState,
    fired_ids: &[u32],
) {
    for &id in fired_ids {
        let callback_opt = {
            let mut s = state.borrow_mut();
            if let Some(timer) = s.timers.remove(&id) {
                let cb = timer.callback.clone();
                if let Some(dur) = timer.interval {
                    // setInterval: re-spawn timer task for next interval
                    let tx = s.event_tx.clone();
                    let handle = spawn_timer_task(tx, id, dur);
                    s.timers.insert(
                        id,
                        Timer {
                            callback: timer.callback,
                            interval: Some(dur),
                            task_handle: Some(handle),
                        },
                    );
                }
                Some(cb)
            } else {
                None
            }
        };

        if let Some(callback) = callback_opt {
            let func = v8::Local::new(scope, &callback);
            let undefined = v8::undefined(scope).into();
            func.call(scope, undefined, &[]);
            scope.perform_microtask_checkpoint();
        }
    }
}

/// Resolve completed async ops in V8.
fn resolve_ops(
    scope: &mut v8::PinScope,
    state: &SharedState,
) {
    let completed: Vec<(u32, String)> =
        state.borrow_mut().completed_ops.drain(..).collect();
    for (id, value) in completed {
        let resolver = state.borrow_mut().pending_resolvers.remove(&id);
        if let Some(resolver) = resolver {
            let r = v8::Local::new(scope, &resolver);
            let val = v8::String::new(scope, &value).unwrap();
            r.resolve(scope, val.into());
            scope.perform_microtask_checkpoint();
        }
    }
}

/// Drive the event loop until there is no more pending work.
/// Uses channel-based blocking (zero CPU while idle).
fn run_event_loop(
    scope: &mut v8::PinScope,
    state: &SharedState,
) {
    loop {
        // Phase 1: V8 work (synchronous)
        scope.perform_microtask_checkpoint();

        // Process immediately-ready timers (0ms, no channel)
        let ready_ids: Vec<u32> = state.borrow_mut().ready_timer_ids.drain(..).collect();
        if !ready_ids.is_empty() {
            fire_timers(scope, state, &ready_ids);
            scope.perform_microtask_checkpoint();
            continue; // re-check for more ready work before blocking
        }

        // Drain and process channel events
        let events = drain_ready_events(state);
        let fired_ids: Vec<u32> = events
            .into_iter()
            .filter_map(|e| match e {
                Event::TimerFired(id) => {
                    if state.borrow().timers.contains_key(&id) {
                        Some(id)
                    } else {
                        None
                    }
                }
                Event::OpCompleted(id, value) => {
                    state.borrow_mut().completed_ops.push((id, value));
                    None
                }
            })
            .collect();

        fire_timers(scope, state, &fired_ids);
        resolve_ops(scope, state);

        // Check if done
        {
            let s = state.borrow();
            if s.timers.is_empty() && s.pending_resolvers.is_empty() && s.ready_timer_ids.is_empty() {
                break;
            }
        }

        // Phase 2: wait for next event (blocks, zero CPU)
        {
            let mut s = state.borrow_mut();
            match s.event_rx.blocking_recv() {
                Some(event) => {
                    match event {
                        Event::TimerFired(id) => {
                            if s.timers.contains_key(&id) {
                                drop(s);
                                fire_timers(scope, state, &[id]);
                            }
                        }
                        Event::OpCompleted(id, value) => {
                            s.completed_ops.push((id, value));
                            drop(s);
                            resolve_ops(scope, state);
                        }
                    }
                }
                None => break, // channel closed
            }
        }
    }
}

/// Drive the event loop until a specific promise settles or no more work.
/// Uses channel-based blocking (zero CPU while idle).
fn run_event_loop_until_settled(
    scope: &mut v8::PinScope,
    state: &SharedState,
    promise: &v8::Global<v8::Promise>,
) {
    loop {
        // Check if promise already settled
        {
            let local = v8::Local::new(scope, promise);
            match local.state() {
                v8::PromiseState::Fulfilled | v8::PromiseState::Rejected => return,
                v8::PromiseState::Pending => {}
            }
        }

        // Phase 1: V8 work (synchronous)
        scope.perform_microtask_checkpoint();

        // Check again after microtasks
        {
            let local = v8::Local::new(scope, promise);
            match local.state() {
                v8::PromiseState::Fulfilled | v8::PromiseState::Rejected => return,
                v8::PromiseState::Pending => {}
            }
        }

        // Process immediately-ready timers (0ms, no channel)
        let ready_ids: Vec<u32> = state.borrow_mut().ready_timer_ids.drain(..).collect();
        if !ready_ids.is_empty() {
            fire_timers(scope, state, &ready_ids);
            scope.perform_microtask_checkpoint();
            continue; // re-check promise state
        }

        // Drain and process channel events
        let events = drain_ready_events(state);
        let fired_ids: Vec<u32> = events
            .into_iter()
            .filter_map(|e| match e {
                Event::TimerFired(id) => {
                    if state.borrow().timers.contains_key(&id) {
                        Some(id)
                    } else {
                        None
                    }
                }
                Event::OpCompleted(id, value) => {
                    state.borrow_mut().completed_ops.push((id, value));
                    None
                }
            })
            .collect();

        fire_timers(scope, state, &fired_ids);
        resolve_ops(scope, state);

        // Check promise after processing events
        {
            let local = v8::Local::new(scope, promise);
            match local.state() {
                v8::PromiseState::Fulfilled | v8::PromiseState::Rejected => return,
                v8::PromiseState::Pending => {}
            }
        }

        // Check if done (no work left even if promise still pending)
        {
            let s = state.borrow();
            if s.timers.is_empty() && s.pending_resolvers.is_empty() {
                drop(s);
                scope.perform_microtask_checkpoint();
                return;
            }
        }

        // Phase 2: wait for next event (blocks, zero CPU)
        {
            let mut s = state.borrow_mut();
            match s.event_rx.blocking_recv() {
                Some(event) => {
                    match event {
                        Event::TimerFired(id) => {
                            if s.timers.contains_key(&id) {
                                drop(s);
                                fire_timers(scope, state, &[id]);
                            }
                        }
                        Event::OpCompleted(id, value) => {
                            s.completed_ops.push((id, value));
                            drop(s);
                            resolve_ops(scope, state);
                        }
                    }
                }
                None => break, // channel closed
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Isolate
// ---------------------------------------------------------------------------

/// A V8 isolate with persistent context -- compiled code stays across requests.
/// Server JS is compiled ONCE. Each request just calls the handler function.
pub struct Isolate {
    isolate: v8::OwnedIsolate,
    context: v8::Global<v8::Context>,
    /// Pre-compiled dispatch script source (avoids recompilation).
    dispatch_fn: Option<v8::Global<v8::Function>>,
    initialized: bool,
    server_js: String,
    state: SharedState,
}

impl Isolate {
    /// Create a new isolate and load server JS (compiled once).
    pub fn new(server_js: &str) -> Self {
        let params = v8::CreateParams::default().heap_limits(0, 128 * 1024 * 1024);
        let mut isolate = v8::Isolate::new(params);

        let state: SharedState = Rc::new(RefCell::new(EventLoopState::new()));

        // Store state in isolate slot so callbacks can access it
        isolate.set_slot(state.clone());

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
            state,
        }
    }

    /// Initialize: load server JS + compile dispatch function (once).
    fn ensure_initialized(&mut self) {
        if self.initialized {
            return;
        }

        v8::scope!(let handle_scope, &mut self.isolate);
        let context = v8::Local::new(handle_scope, &self.context);
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        // Register globals (console, setTimeout, etc.)
        setup_globals(scope);

        // Load server JS (registers __rpc handlers)
        if !self.server_js.is_empty() {
            let code = v8::String::new(scope, &self.server_js).unwrap();
            let script = v8::Script::compile(scope, code, None).unwrap();
            script.run(scope).unwrap();
        }

        // Compile the dispatch function ONCE -- reused for every request.
        // Updated to handle async handlers (returns Promise for async results).
        let dispatch_src = r#"(function(__req_json) {
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

        // Reset completed ops from prior requests
        self.state.borrow_mut().completed_ops.clear();

        // Drain any stale events from prior requests
        {
            let mut s = self.state.borrow_mut();
            while s.event_rx.try_recv().is_ok() {}
        }

        let wall_start = Instant::now();
        let cpu_start = thread_cpu_time();

        v8::scope!(let handle_scope, &mut self.isolate);
        let context = v8::Local::new(handle_scope, &self.context);
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        // Call the pre-compiled dispatch function with request JSON
        let dispatch_fn = self.dispatch_fn.as_ref().unwrap();
        let func = v8::Local::new(scope, dispatch_fn);

        let arg =
            v8::String::new(scope, request_json).ok_or("Failed to create arg string")?;
        let undefined = v8::undefined(scope).into();

        let result = func
            .call(scope, undefined, &[arg.into()])
            .ok_or("Dispatch call failed")?;

        let json = if result.is_promise() {
            let promise = v8::Local::<v8::Promise>::try_from(result)
                .map_err(|e| format!("Promise cast failed: {e}"))?;
            let global_promise = v8::Global::new(scope, promise);

            // Drive event loop until promise settles
            run_event_loop_until_settled(scope, &self.state, &global_promise);

            let promise = v8::Local::new(scope, &global_promise);
            match promise.state() {
                v8::PromiseState::Fulfilled => {
                    let value = promise.result(scope);
                    let s = value.to_string(scope).ok_or("Failed to stringify promise result")?;
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
            // Synchronous result -- also run event loop for any side-effect timers
            let json_v8 = result.to_string(scope).ok_or("Failed to stringify")?;
            let json = json_v8.to_rust_string_lossy(scope);

            // Run any pending timers/ops (fire-and-forget side effects)
            run_event_loop(scope, &self.state);

            json
        };

        let cpu_time = thread_cpu_time().saturating_sub(cpu_start);
        let wall_time = wall_start.elapsed();

        Ok(RequestResult {
            json,
            cpu_time,
            wall_time,
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
        println!(
            "ping: {} cpu={:.3}ms",
            r.json,
            r.cpu_time.as_secs_f64() * 1000.0
        );
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
        println!("Context persists: 1->2->3 across 3 requests");
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
        println!(
            "fib(20): cpu={:.3}ms",
            r1.cpu_time.as_secs_f64() * 1000.0
        );
        println!(
            "fib(35): cpu={:.3}ms",
            r2.cpu_time.as_secs_f64() * 1000.0
        );
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

    // -----------------------------------------------------------------------
    // Async / timer tests
    // -----------------------------------------------------------------------

    #[test]
    fn async_timeout() {
        init_v8();
        let js = r#"
            var __rpc = {
                delayed: function() {
                    return new Promise(function(resolve) {
                        setTimeout(function() { resolve("done after delay"); }, 10);
                    });
                }
            };
        "#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(
                r#"{"jsonrpc":"2.0","method":"delayed","params":[],"id":1}"#,
            )
            .unwrap();
        assert!(r.json.contains("done after delay"));
        println!(
            "Async timeout: {} wall={:.0}ms",
            r.json,
            r.wall_time.as_millis()
        );
    }

    #[test]
    fn async_await_syntax() {
        init_v8();
        // Use async/await (V8 supports this natively)
        let js = r#"
            var __rpc = {
                greeting: async function(name) {
                    var msg = await new Promise(function(resolve) {
                        setTimeout(function() { resolve("Hello, " + name + "!"); }, 5);
                    });
                    return msg;
                }
            };
        "#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(
                r#"{"jsonrpc":"2.0","method":"greeting","params":["world"],"id":1}"#,
            )
            .unwrap();
        assert!(r.json.contains("Hello, world!"));
        println!("Async/await: {}", r.json);
    }

    #[test]
    fn clear_timeout_works() {
        init_v8();
        let js = r#"
            var __rpc = {
                test_clear: function() {
                    return new Promise(function(resolve) {
                        var id = setTimeout(function() { resolve("should not fire"); }, 5000);
                        clearTimeout(id);
                        setTimeout(function() { resolve("cleared ok"); }, 5);
                    });
                }
            };
        "#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(
                r#"{"jsonrpc":"2.0","method":"test_clear","params":[],"id":1}"#,
            )
            .unwrap();
        assert!(r.json.contains("cleared ok"));
        println!("clearTimeout: {}", r.json);
    }

    #[test]
    fn promise_chain() {
        init_v8();
        let js = r#"
            var __rpc = {
                chain: function() {
                    return new Promise(function(resolve) {
                        setTimeout(function() { resolve(1); }, 5);
                    }).then(function(v) {
                        return v + 10;
                    }).then(function(v) {
                        return v * 2;
                    });
                }
            };
        "#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(
                r#"{"jsonrpc":"2.0","method":"chain","params":[],"id":1}"#,
            )
            .unwrap();
        // (1 + 10) * 2 = 22
        assert!(r.json.contains("\"result\":22"));
        println!("Promise chain: {}", r.json);
    }

    #[test]
    fn set_timeout_zero_delay() {
        init_v8();
        let js = r#"
            var __rpc = {
                immediate: function() {
                    return new Promise(function(resolve) {
                        setTimeout(function() { resolve("immediate"); }, 0);
                    });
                }
            };
        "#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(
                r#"{"jsonrpc":"2.0","method":"immediate","params":[],"id":1}"#,
            )
            .unwrap();
        assert!(r.json.contains("immediate"));
    }

    #[test]
    fn multiple_timeouts_ordered() {
        init_v8();
        let js = r#"
            var __rpc = {
                ordered: function() {
                    var results = [];
                    return new Promise(function(resolve) {
                        setTimeout(function() { results.push("c"); resolve(results.join(",")); }, 30);
                        setTimeout(function() { results.push("a"); }, 5);
                        setTimeout(function() { results.push("b"); }, 15);
                    });
                }
            };
        "#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(
                r#"{"jsonrpc":"2.0","method":"ordered","params":[],"id":1}"#,
            )
            .unwrap();
        assert!(r.json.contains("a,b,c"));
        println!("Multiple timeouts ordered: {}", r.json);
    }

    #[test]
    fn sync_still_works_with_event_loop() {
        init_v8();
        // Sync handlers should still work fine
        let js = r#"var __rpc = { add: function(a, b) { return a + b; } };"#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(
                r#"{"jsonrpc":"2.0","method":"add","params":[3,4],"id":1}"#,
            )
            .unwrap();
        assert!(r.json.contains("\"result\":7"));
    }

    #[test]
    fn console_log_works() {
        init_v8();
        let js = r#"
            var __rpc = {
                greet: function() {
                    console.log("Hello from JS!");
                    return "logged";
                }
            };
        "#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(
                r#"{"jsonrpc":"2.0","method":"greet","params":[],"id":1}"#,
            )
            .unwrap();
        assert!(r.json.contains("logged"));
    }
}
