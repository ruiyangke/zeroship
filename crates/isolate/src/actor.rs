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
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use crate::cpu::CpuUsage;
use crate::isolate::{self, RpcPendingReplies, SharedRpcReceiver};

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

/// Spawn an isolate actor on a new thread.
///
/// Returns a handle for sending messages.
pub fn spawn(
    app_id: &str,
    server_js: &str,
    data_dir: &PathBuf,
    plugins: Vec<Box<dyn Plugin>>,
    cpu_limit: Option<Duration>,
    meter: Arc<dyn PluginMeter>,
    quota: Arc<dyn PluginQuota>,
) -> Result<ActorHandle, String> {
    let (tx, rx) = mpsc::channel::<IsolateMessage>(64);
    let cpu_usage = Arc::new(Mutex::new(CpuUsage::default()));
    let cpu_usage_clone = cpu_usage.clone();

    let app_id = app_id.to_string();
    let server_js = server_js.to_string();
    let data_dir = data_dir.clone();

    std::thread::Builder::new()
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

    Ok(ActorHandle { tx, cpu_usage })
}

/// The actor's main loop — runs on its own thread.
///
/// Uses a `poll_fn` to simultaneously:
/// 1. Drain incoming `IsolateMessage`s from the mpsc channel
/// 2. Drive the V8 event loop (which processes all in-flight JS promises)
///
/// Requests are injected into JS via `rpc_tx` → `op_rpc_recv()`.
/// Responses come back via `op_rpc_respond()` → oneshot senders in `pending_replies`.
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

    // Create the RPC channel pair
    let (rpc_tx, rpc_rx) = mpsc::channel::<(u64, String)>(64);
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
    static DISPATCH_LOOP: &str = r#"(async () => {
        while (true) {
            const result = await Deno.core.ops.op_rpc_recv();
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
                    Deno.core.ops.op_rpc_respond(requestId, response);
                } catch (e) {
                    Deno.core.ops.op_rpc_respond(requestId, JSON.stringify({
                        jsonrpc: '2.0',
                        error: { code: -32000, message: e.message || String(e) },
                        id: null,
                    }));
                }
            })();
        }
    })()"#;

    if let Err(e) = runtime.execute_script("<dispatch>", DISPATCH_LOOP) {
        eprintln!("[isolate] [{app_id}] Dispatch loop error: {e}");
        return;
    }

    eprintln!("[isolate] [{app_id}] Ready (concurrent mode)");

    // Concurrent event loop — poll_fn drives both message intake and V8 event loop.
    std::future::poll_fn(|cx| {
        // Phase 1: Drain incoming messages (non-blocking)
        loop {
            match rx.poll_recv(cx) {
                Poll::Ready(Some(msg)) => match msg {
                    IsolateMessage::Rpc { body, reply } => {
                        let id = next_id;
                        next_id += 1;
                        pending_replies.borrow_mut().insert(id, reply);
                        // Inject request into JS event loop via the channel
                        if rpc_tx.try_send((id, body)).is_err() {
                            // Channel full — backpressure: reject immediately
                            if let Some(tx) = pending_replies.borrow_mut().remove(&id) {
                                let _ = tx.send(Err(
                                    "Request queue full".to_string(),
                                ));
                            }
                        }
                    }
                    IsolateMessage::Shutdown => {
                        eprintln!("[isolate] [{app_id}] Shutting down");
                        return Poll::Ready(());
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
        match runtime.poll_event_loop(cx, Default::default()) {
            Poll::Ready(Ok(())) => {
                // Event loop drained — but our dispatch loop (the infinite
                // `while(true) { await op_rpc_recv() }`) should keep it alive.
                // If it actually drained, it means the dispatch loop exited
                // (channel closed). Re-wake to check for more messages.
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Poll::Ready(Err(e)) => {
                eprintln!("[isolate] [{app_id}] Event loop error: {e}");
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }
    })
    .await;
}
