//! Raw V8 isolate runtime -- per-request model with persistent context.
//!
//! Context and compiled code persist across requests (like workerd).
//! Each request enters the existing context, calls a pre-stored handler.
//! CPU time measured per-request via CLOCK_THREAD_CPUTIME_ID.
//!
//! Supports async/await, setTimeout/clearTimeout, setInterval/clearInterval
//! via a min-heap event loop (Node.js/libuv model). Zero CPU while idle.

#![allow(unsafe_code)]

pub mod concurrent;

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::rc::Rc;
use std::sync::mpsc;
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

// ---------------------------------------------------------------------------
// Timer min-heap entry
// ---------------------------------------------------------------------------

#[derive(Eq, PartialEq)]
pub(crate) struct TimerHeapEntry {
    pub(crate) fire_at: Instant,
    pub(crate) id: u32,
}

impl Ord for TimerHeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.fire_at
            .cmp(&other.fire_at)
            .then(self.id.cmp(&other.id))
    }
}

impl PartialOrd for TimerHeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

// ---------------------------------------------------------------------------
// Timer callback storage
// ---------------------------------------------------------------------------

#[allow(missing_debug_implementations)]
pub(crate) struct TimerCallback {
    pub(crate) callback: v8::Global<v8::Function>,
    pub(crate) interval: Option<Duration>, // None = setTimeout, Some = setInterval
}

// ---------------------------------------------------------------------------
// Async op result (for future fetch/DB support)
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub(crate) struct OpResult {
    pub(crate) id: u32,
    pub(crate) value: String,
}

// ---------------------------------------------------------------------------
// Event loop state
// ---------------------------------------------------------------------------

/// State shared between V8 callbacks and the event loop driver.
#[allow(missing_debug_implementations)]
pub(crate) struct EventLoopState {
    /// Timer min-heap: next-to-fire on top (via Reverse for BinaryHeap)
    pub(crate) timer_heap: BinaryHeap<Reverse<TimerHeapEntry>>,
    /// Timer callbacks stored separately (heap only has fire_at + id)
    pub(crate) timer_callbacks: HashMap<u32, TimerCallback>,
    pub(crate) next_timer_id: u32,
    /// Async op channel sender (for future fetch() etc)
    #[allow(dead_code)]
    pub(crate) op_tx: mpsc::Sender<OpResult>,
    /// Async op channel receiver
    pub(crate) op_rx: mpsc::Receiver<OpResult>,
    /// Pending promise resolvers for async ops
    pub(crate) pending_resolvers: HashMap<u32, v8::Global<v8::PromiseResolver>>,
    #[allow(dead_code)]
    pub(crate) next_op_id: u32,
}

impl EventLoopState {
    pub(crate) fn new() -> Self {
        let (op_tx, op_rx) = mpsc::channel();
        Self {
            timer_heap: BinaryHeap::new(),
            timer_callbacks: HashMap::new(),
            next_timer_id: 1,
            op_tx,
            op_rx,
            pending_resolvers: HashMap::new(),
            next_op_id: 1,
        }
    }
}

pub(crate) type SharedState = Rc<RefCell<EventLoopState>>;

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

    // Insert into min-heap (no tokio task!)
    s.timer_heap.push(Reverse(TimerHeapEntry {
        fire_at: Instant::now() + delay,
        id,
    }));
    s.timer_callbacks.insert(
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
        .expect("EventLoopState not in isolate slot")
        .clone();

    let id = if args.length() > 0 {
        args.get(0).uint32_value(scope).unwrap_or(0)
    } else {
        return;
    };

    let mut s = state.borrow_mut();
    // Lazy deletion: only remove from callbacks, heap entry will be skipped when popped
    s.timer_callbacks.remove(&id);
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

    // Insert into min-heap
    s.timer_heap.push(Reverse(TimerHeapEntry {
        fire_at: Instant::now() + dur,
        id,
    }));
    s.timer_callbacks.insert(
        id,
        TimerCallback {
            callback: global_cb,
            interval: Some(dur),
        },
    );

    rv.set(v8::Integer::new(scope, id as i32).into());
}

// ---------------------------------------------------------------------------
// Setup globals on context
// ---------------------------------------------------------------------------

pub(crate) fn setup_globals(scope: &mut v8::PinScope) {
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
// Event loop — min-heap + phased model (Node.js/libuv style)
// ---------------------------------------------------------------------------

/// Fire all timers whose fire_at <= now. Returns true if any timer fired.
pub(crate) fn fire_ready_timers(scope: &mut v8::PinScope, state: &SharedState) -> bool {
    let mut any_fired = false;
    let now = Instant::now();

    loop {
        let should_fire = {
            let s = state.borrow();
            s.timer_heap
                .peek()
                .map(|Reverse(e)| e.fire_at <= now)
                .unwrap_or(false)
        };
        if !should_fire {
            break;
        }

        let entry = state.borrow_mut().timer_heap.pop().unwrap().0;

        // Check if callback still exists (lazy deletion: cleared timers are skipped)
        let cb_opt = {
            let s = state.borrow();
            s.timer_callbacks
                .get(&entry.id)
                .map(|t| (t.callback.clone(), t.interval))
        };

        if let Some((callback, interval)) = cb_opt {
            any_fired = true;

            // Fire the callback
            let func = v8::Local::new(scope, &callback);
            let undefined = v8::undefined(scope).into();
            func.call(scope, undefined, &[]);
            scope.perform_microtask_checkpoint();

            // Handle interval: re-insert into heap with new fire_at
            if let Some(dur) = interval {
                let mut s = state.borrow_mut();
                s.timer_heap.push(Reverse(TimerHeapEntry {
                    fire_at: Instant::now() + dur,
                    id: entry.id,
                }));
                // DON'T remove callback from timer_callbacks
            } else {
                // setTimeout: remove callback
                state.borrow_mut().timer_callbacks.remove(&entry.id);
            }
        }
        // else: timer was cleared (lazy deletion) -- skip
    }

    any_fired
}

/// Drain completed async ops from the channel and resolve their promises.
fn drain_async_ops(scope: &mut v8::PinScope, state: &SharedState) {
    loop {
        let result = state.borrow_mut().op_rx.try_recv();
        match result {
            Ok(op_result) => {
                let resolver = state.borrow_mut().pending_resolvers.remove(&op_result.id);
                if let Some(resolver) = resolver {
                    let r = v8::Local::new(scope, &resolver);
                    let val = v8::String::new(scope, &op_result.value).unwrap();
                    r.resolve(scope, val.into());
                    scope.perform_microtask_checkpoint();
                }
            }
            Err(_) => break,
        }
    }
}

/// Compute the wait duration until the next valid timer fires.
/// Skips cleared timers (lazy deletion) by iterating the heap.
fn compute_wait_timeout(state: &SharedState) -> Option<Duration> {
    let s = state.borrow();

    // Find next valid timer in heap (skip cleared ones)
    let mut next_fire = None;
    for Reverse(entry) in s.timer_heap.iter() {
        if s.timer_callbacks.contains_key(&entry.id) {
            next_fire = Some(entry.fire_at);
            break; // heap is ordered, first valid entry is the soonest
        }
    }

    match next_fire {
        Some(fire_at) => Some(fire_at.saturating_duration_since(Instant::now())),
        None if !s.pending_resolvers.is_empty() => {
            // No timers but async ops pending: wait up to 60s for op completion
            Some(Duration::from_secs(60))
        }
        None => None, // nothing to wait for
    }
}

/// Drive the event loop until there is no more pending work.
/// Uses min-heap timers + computed sleep (zero CPU while idle).
fn run_event_loop(scope: &mut v8::PinScope, state: &SharedState) {
    loop {
        // Phase 1: Microtasks
        scope.perform_microtask_checkpoint();

        // Phase 2: Fire all ready timers
        fire_ready_timers(scope, state);

        // Phase 3: Drain completed async ops
        drain_async_ops(scope, state);

        // Phase 4: Check if done
        {
            let s = state.borrow();
            let has_timers = !s.timer_callbacks.is_empty();
            let has_ops = !s.pending_resolvers.is_empty();
            if !has_timers && !has_ops {
                break;
            }
        }

        // Phase 5: Wait for next event (computed timeout)
        let timeout = match compute_wait_timeout(state) {
            Some(d) => d,
            None => break, // nothing to wait for
        };

        if timeout.is_zero() {
            continue; // immediate timer ready
        }

        // Block until timeout or async op completion (whichever first)
        {
            let has_pending_ops = !state.borrow().pending_resolvers.is_empty();
            if has_pending_ops {
                // Wait for op completion OR timeout
                let result = state.borrow_mut().op_rx.recv_timeout(timeout);
                if let Ok(op_result) = result {
                    let resolver =
                        state.borrow_mut().pending_resolvers.remove(&op_result.id);
                    if let Some(resolver) = resolver {
                        let r = v8::Local::new(scope, &resolver);
                        let val = v8::String::new(scope, &op_result.value).unwrap();
                        r.resolve(scope, val.into());
                        scope.perform_microtask_checkpoint();
                    }
                }
                // Timeout or Disconnected: fall through to re-check timers
            } else {
                // No pending ops, just sleep until next timer
                std::thread::sleep(timeout);
            }
        }
    }
}

/// Drive the event loop until a specific promise settles or no more work.
/// Uses min-heap timers + computed sleep (zero CPU while idle).
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

        // Phase 1: Microtasks
        scope.perform_microtask_checkpoint();

        // Check again after microtasks
        {
            let local = v8::Local::new(scope, promise);
            match local.state() {
                v8::PromiseState::Fulfilled | v8::PromiseState::Rejected => return,
                v8::PromiseState::Pending => {}
            }
        }

        // Phase 2: Fire all ready timers
        fire_ready_timers(scope, state);

        // Check promise after timers
        {
            let local = v8::Local::new(scope, promise);
            match local.state() {
                v8::PromiseState::Fulfilled | v8::PromiseState::Rejected => return,
                v8::PromiseState::Pending => {}
            }
        }

        // Phase 3: Drain completed async ops
        drain_async_ops(scope, state);

        // Check promise after ops
        {
            let local = v8::Local::new(scope, promise);
            match local.state() {
                v8::PromiseState::Fulfilled | v8::PromiseState::Rejected => return,
                v8::PromiseState::Pending => {}
            }
        }

        // Phase 4: Check if done (no work left even if promise still pending)
        {
            let s = state.borrow();
            if s.timer_callbacks.is_empty() && s.pending_resolvers.is_empty() {
                drop(s);
                scope.perform_microtask_checkpoint();
                return;
            }
        }

        // Phase 5: Wait for next event (computed timeout)
        let timeout = match compute_wait_timeout(state) {
            Some(d) => d,
            None => {
                scope.perform_microtask_checkpoint();
                return;
            }
        };

        if timeout.is_zero() {
            continue; // immediate timer ready
        }

        // Block until timeout or async op completion (whichever first)
        {
            let has_pending_ops = !state.borrow().pending_resolvers.is_empty();
            if has_pending_ops {
                let result = state.borrow_mut().op_rx.recv_timeout(timeout);
                if let Ok(op_result) = result {
                    let resolver =
                        state.borrow_mut().pending_resolvers.remove(&op_result.id);
                    if let Some(resolver) = resolver {
                        let r = v8::Local::new(scope, &resolver);
                        let val = v8::String::new(scope, &op_result.value).unwrap();
                        r.resolve(scope, val.into());
                        scope.perform_microtask_checkpoint();
                    }
                }
            } else {
                std::thread::sleep(timeout);
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

        // Drain any stale timer heap entries and callbacks from prior requests
        {
            let mut s = self.state.borrow_mut();
            s.timer_heap.clear();
            s.timer_callbacks.clear();
            // Drain stale op results
            while s.op_rx.try_recv().is_ok() {}
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

pub(crate) fn thread_cpu_time() -> Duration {
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
