//! Isolate actor — runs a V8 isolate on a dedicated thread, handles messages.
//!
//! Each actor owns a `JsRuntime` (which is `!Send`) and runs on its own
//! single-threaded tokio runtime.
//!
//! **Concurrent mode**: multiple RPC requests can be in-flight simultaneously.
//! Requests are injected into the JS event loop via an mpsc channel; each one
//! runs as a fire-and-forget promise chain (like Deno.serve). The actor's
//! poll_fn simultaneously drains incoming messages AND drives V8's event loop.

use appbase_core::plugin::{Plugin, PluginMeter, PluginQuota};
use appbase_core::types::RpcResult;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

use crate::cpu::CpuUsage;
use crate::isolate::{self, RpcPendingReplies, SharedRpcReceiver};
use deno_core::v8;

/// Messages that can be sent to an isolate actor.
pub enum IsolateMessage {
    /// Execute an RPC request and return the result.
    Rpc {
        body: String,
        reply: oneshot::Sender<Result<RpcResult, String>>,
    },
    /// Reload the isolate with new server JS (creates fresh V8 isolate).
    Reload {
        server_js: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Get isolate statistics.
    Stats {
        reply: oneshot::Sender<ActorStats>,
    },
    /// Gracefully shut down the isolate.
    Shutdown,
}

/// Statistics reported by an actor.
#[derive(Debug, Clone)]
pub struct ActorStats {
    pub total_cpu_ms: f64,
    pub request_count: u64,
}

/// Handle to a running isolate actor. Send messages through this.
#[derive(Clone)]
pub struct ActorHandle {
    pub tx: mpsc::Sender<IsolateMessage>,
    pub cpu_usage: Arc<Mutex<CpuUsage>>,
}

impl ActorHandle {
    /// Send an RPC request and wait for the result.
    pub async fn rpc(&self, body: String) -> Result<RpcResult, String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(IsolateMessage::Rpc { body, reply: reply_tx })
            .await
            .map_err(|_| "Isolate actor channel closed".to_string())?;
        reply_rx
            .await
            .map_err(|_| "Isolate actor dropped reply".to_string())?
    }

    /// Check if the actor is still alive.
    pub fn is_alive(&self) -> bool {
        !self.tx.is_closed()
    }
}

/// Result of spawning an isolate actor: a cloneable handle plus the thread's JoinHandle.
pub struct SpawnResult {
    pub handle: ActorHandle,
    pub thread_handle: std::thread::JoinHandle<()>,
}

/// Spawn an isolate actor on a new thread.
///
/// Returns a handle for sending messages and the thread's JoinHandle.
pub fn spawn(
    app_id: &str,
    server_js: &str,
    data_dir: &PathBuf,
    plugins: Vec<Box<dyn Plugin>>,
    cpu_limit: Option<Duration>,
    meter: Arc<dyn PluginMeter>,
    quota: Arc<dyn PluginQuota>,
) -> Result<SpawnResult, String> {
    let (tx, rx) = mpsc::channel::<IsolateMessage>(64);
    let cpu_usage = Arc::new(Mutex::new(CpuUsage::default()));
    let cpu_usage_clone = cpu_usage.clone();

    let app_id = app_id.to_string();
    let server_js = server_js.to_string();
    let data_dir = data_dir.clone();

    let thread_handle = std::thread::Builder::new()
        .name(format!("v8-{app_id}"))
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(actor_loop(
                &app_id, &server_js, &data_dir, plugins, rx, cpu_limit, cpu_usage_clone, meter, quota,
            ));
        })
        .map_err(|e| format!("Failed to spawn V8 thread: {e}"))?;

    Ok(SpawnResult {
        handle: ActorHandle { tx, cpu_usage },
        thread_handle,
    })
}

/// The actor's main loop — runs on its own thread.
///
/// Uses a `poll_fn` to simultaneously:
/// 1. Drain incoming `IsolateMessage`s from the mpsc channel
/// 2. Drive the V8 event loop (which processes all in-flight JS promises)
///
/// Requests are injected into JS via `rpc_tx` → `op_rpc_recv()`.
/// Responses come back via `op_rpc_respond()` → oneshot senders in `pending_replies`.
/// Default wall-time limit per isolate: 30 seconds of no completed work.
const DEFAULT_WALL_LIMIT: Duration = Duration::from_secs(30);

/// Watchdog thread: terminates V8 execution if no activity for `wall_limit`.
///
/// Runs on a separate OS thread (not tokio) so it can fire even when V8
/// blocks the actor thread in a tight JS loop.
fn watchdog_loop(
    watchdog_rx: tokio::sync::watch::Receiver<Instant>,
    v8_handle: v8::IsolateHandle,
    wall_limit: Duration,
    app_id: &str,
) {
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let last_activity = *watchdog_rx.borrow();
        if last_activity.elapsed() > wall_limit {
            eprintln!(
                "[isolate] [{app_id}] Wall-time limit exceeded ({wall_limit:?}), terminating V8"
            );
            v8_handle.terminate_execution();
            break;
        }
        // If the sender is dropped (actor exited), stop the watchdog.
        if watchdog_rx.has_changed().is_err() {
            break;
        }
    }
}

async fn actor_loop(
    app_id: &str,
    server_js: &str,
    data_dir: &PathBuf,
    plugins: Vec<Box<dyn Plugin>>,
    mut rx: mpsc::Receiver<IsolateMessage>,
    _cpu_limit: Option<Duration>,
    cpu_usage: Arc<Mutex<CpuUsage>>,
    meter: Arc<dyn PluginMeter>,
    quota: Arc<dyn PluginQuota>,
) {
    // Ensure data directory exists
    let _ = std::fs::create_dir_all(data_dir);

    let mut runtime = match isolate::create(&plugins, app_id, data_dir, meter.clone(), quota.clone()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[isolate] [{app_id}] Failed to create V8 runtime: {e}");
            return;
        }
    };

    // --- Wall-time watchdog (Issue 1) ---
    // V8's IsolateHandle is Send and can terminate execution from another thread.
    // If V8 is stuck in a tight JS loop (e.g. `while(true){}`), the poll_fn
    // never gets polled, so we MUST use a separate OS thread for the watchdog.
    let v8_handle = runtime.v8_isolate().thread_safe_handle();
    let (watchdog_tx, watchdog_rx) = tokio::sync::watch::channel(Instant::now());
    let wall_limit = DEFAULT_WALL_LIMIT;
    let watchdog_app_id = app_id.to_string();
    let _watchdog_thread = std::thread::Builder::new()
        .name(format!("watchdog-{app_id}"))
        .spawn(move || {
            watchdog_loop(watchdog_rx, v8_handle, wall_limit, &watchdog_app_id);
        });

    // Create the RPC channel pair.
    // Wrapped in Option so Shutdown can drop the sender, causing the JS dispatch
    // loop to exit naturally (op_rpc_recv returns null when channel closes).
    let (rpc_tx, rpc_rx) = mpsc::channel::<(u64, String)>(4096);
    let mut rpc_tx: Option<mpsc::Sender<(u64, String)>> = Some(rpc_tx);
    let pending_replies: Rc<RefCell<HashMap<u64, oneshot::Sender<Result<RpcResult, String>>>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let mut next_id: u64 = 0;

    // Store channel endpoints in OpState for the JS ops
    {
        let op_state = runtime.op_state();
        let mut state = op_state.borrow_mut();
        state.put(SharedRpcReceiver(Rc::new(tokio::sync::Mutex::new(rpc_rx))));
        state.put(RpcPendingReplies(pending_replies.clone()));
    }

    // Step 1: Load user code (registers __rpc methods)
    if !server_js.is_empty() {
        if let Err(e) = runtime.execute_script("<server>", server_js.to_string()) {
            eprintln!("[isolate] [{app_id}] Script error: {e}");
            return;
        }
    }

    // Step 2: Start the concurrent dispatch loop (AFTER channels + user code are ready)
    // This fire-and-forget IIFE waits for requests via op_rpc_recv and dispatches them.
    // Capture internal ops into closure-scoped variables, then delete them from
    // the ops object so user JS cannot call op_rpc_recv() or op_rpc_respond()
    // directly (Issue 8: internal ops exposed to user JS).
    static DISPATCH_LOOP: &str = r#"{
        const __recv = Deno.core.ops.op_rpc_recv;
        const __respond = Deno.core.ops.op_rpc_respond;
        delete Deno.core.ops.op_rpc_recv;
        delete Deno.core.ops.op_rpc_respond;

        (async () => {
            while (true) {
                const result = await __recv();
                if (result === null) break;
                const [requestId, requestJson] = result;
                (async () => {
                    try {
                        const request = JSON.parse(requestJson);
                        let response;
                        if (Array.isArray(request)) {
                            response = JSON.stringify(await Promise.all(request.map(__dispatch)));
                        } else {
                            response = JSON.stringify(await __dispatch(request));
                        }
                        __respond(requestId, response);
                    } catch (e) {
                        __respond(requestId, JSON.stringify({
                            jsonrpc: '2.0',
                            error: { code: -32000, message: e.message || String(e) },
                            id: null,
                        }));
                    }
                })();
            }
        })()
    }"#;

    if let Err(e) = runtime.execute_script("<dispatch>", DISPATCH_LOOP) {
        eprintln!("[isolate] [{app_id}] Dispatch loop error: {e}");
        return;
    }

    eprintln!("[isolate] [{app_id}] Ready (concurrent mode)");

    // Track the number of pending replies at each poll so we can detect completions.
    let mut prev_pending_count: usize = 0;

    // Concurrent event loop — poll_fn drives both message intake and V8 event loop.
    std::future::poll_fn(|cx| {
        // Phase 1: Drain incoming messages (non-blocking)
        loop {
            match rx.poll_recv(cx) {
                Poll::Ready(Some(msg)) => match msg {
                    IsolateMessage::Rpc { body, reply } => {
                        if let Some(ref tx) = rpc_tx {
                            let id = next_id;
                            next_id += 1;
                            pending_replies.borrow_mut().insert(id, reply);
                            // Inject request into JS event loop via the channel
                            if tx.try_send((id, body)).is_err() {
                                // Channel full — backpressure: reject immediately
                                if let Some(sender) = pending_replies.borrow_mut().remove(&id) {
                                    let _ = sender.send(Err(
                                        "Request queue full".to_string(),
                                    ));
                                }
                            }
                            // Ping watchdog: new request injected = activity
                            let _ = watchdog_tx.send(Instant::now());
                        } else {
                            // Shutting down — reject immediately
                            let _ = reply.send(Err("Isolate shutting down".to_string()));
                        }
                    }
                    IsolateMessage::Shutdown => {
                        eprintln!("[isolate] [{app_id}] Shutting down");
                        // Drop rpc_tx to close the channel; JS dispatch loop
                        // will see null from op_rpc_recv and exit, allowing
                        // the V8 event loop to drain in-flight requests.
                        rpc_tx.take();
                    }
                    IsolateMessage::Stats { reply } => {
                        let usage = cpu_usage.lock().unwrap();
                        let _ = reply.send(ActorStats {
                            total_cpu_ms: usage.total.as_secs_f64() * 1000.0,
                            request_count: usage.request_count,
                        });
                    }
                    IsolateMessage::Reload { server_js: _, reply } => {
                        let _ = reply.send(Err(
                            "Reload not supported in concurrent mode yet".to_string(),
                        ));
                    }
                },
                Poll::Ready(None) => {
                    // Actor channel closed — all senders dropped
                    return Poll::Ready(());
                }
                Poll::Pending => break,
            }
        }

        // Phase 2: Drive the V8 event loop (one tick)
        // Measure thread CPU time around the poll to track actual CPU usage (Issue 3).
        let cpu_before = crate::cpu::thread_cpu_time();
        let poll_result = runtime.poll_event_loop(cx, Default::default());
        let cpu_after = crate::cpu::thread_cpu_time();
        let cpu_delta = cpu_after.saturating_sub(cpu_before);

        // Record CPU usage if any was consumed
        if !cpu_delta.is_zero() {
            if let Ok(mut usage) = cpu_usage.lock() {
                usage.total += cpu_delta;
            }
        }

        // Detect completed responses: if pending_replies shrank, requests were completed
        let current_pending = pending_replies.borrow().len();
        if current_pending < prev_pending_count {
            let completed = prev_pending_count - current_pending;
            if let Ok(mut usage) = cpu_usage.lock() {
                usage.request_count += completed as u64;
            }
            // Ping watchdog: completed work = activity
            let _ = watchdog_tx.send(Instant::now());
        }
        prev_pending_count = current_pending;

        match poll_result {
            Poll::Ready(Ok(())) => {
                // Event loop drained. This happens when the JS dispatch loop
                // exits (rpc_tx was dropped, so op_rpc_recv returned null).
                // If no pending replies remain, we can exit cleanly.
                if pending_replies.borrow().is_empty() {
                    return Poll::Ready(());
                }
                // Still have in-flight responses — keep polling to let them complete
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Poll::Ready(Err(e)) => {
                // V8 terminated (possibly by watchdog). Drain pending replies with error.
                let err_msg = format!("{e}");
                if err_msg.contains("terminated") {
                    for (_, tx) in pending_replies.borrow_mut().drain() {
                        let _ = tx.send(Err("Wall-time limit exceeded".to_string()));
                    }
                }
                eprintln!("[isolate] [{app_id}] Event loop error: {e}");
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }
    })
    .await;

    // Drain any remaining pending replies with errors so callers don't hang
    for (_, tx) in pending_replies.borrow_mut().drain() {
        let _ = tx.send(Err("Isolate shut down".to_string()));
    }
    eprintln!("[isolate] [{app_id}] Stopped");
}
