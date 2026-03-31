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
#[cfg(target_os = "linux")]
use crate::cpu_timer::{CpuTimer, CpuTimerSystem};
use crate::isolate::{self, RpcPendingReplies, SharedRpcReceiver};
use crate::watchdog::{ExecutionLimits, GlobalWatchdog, OpWatchdogEntry, WatchdogEntry};

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
    watchdog: &GlobalWatchdog,
    limits: ExecutionLimits,
    #[cfg(target_os = "linux")] cpu_timer_system: Arc<CpuTimerSystem>,
) -> Result<SpawnResult, String> {
    let (tx, rx) = mpsc::channel::<IsolateMessage>(64);
    let cpu_usage = Arc::new(Mutex::new(CpuUsage::default()));
    let cpu_usage_clone = cpu_usage.clone();

    let app_id_owned = app_id.to_string();
    let server_js = server_js.to_string();
    let data_dir = data_dir.clone();

    // We register with the watchdog from inside the actor thread
    // (to get the correct pthread_t for cross-thread CPU measurement).
    let watchdog_ref = watchdog.entries_ref();

    let thread_handle = std::thread::Builder::new()
        .name(format!("v8-{app_id}"))
        .spawn(move || {
            // Get pthread_t for this thread and register with watchdog
            #[allow(unsafe_code)]
            let thread_id = unsafe { libc::pthread_self() };

            // We need the v8 handle, but we don't have it yet — it's created in actor_loop.
            // So we pass thread_id + limits into actor_loop and let it register after runtime creation.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(actor_loop(
                &app_id_owned,
                &server_js,
                &data_dir,
                plugins,
                rx,
                cpu_limit,
                cpu_usage_clone,
                meter,
                quota,
                thread_id,
                limits,
                watchdog_ref,
                #[cfg(target_os = "linux")]
                cpu_timer_system,
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
    thread_id: libc::pthread_t,
    limits: ExecutionLimits,
    watchdog_entries: Arc<Mutex<HashMap<String, Arc<WatchdogEntry>>>>,
    #[cfg(target_os = "linux")] cpu_timer_system: Arc<CpuTimerSystem>,
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

    // --- Global watchdog registration ---
    // Register this isolate with the global watchdog for wall-time + CPU-time enforcement.
    // The watchdog thread reads our CPU time cross-thread via pthread_getcpuclockid.
    let _thread_id = thread_id; // suppress unused warning on non-Linux
    let watchdog_entry = {
        let v8_handle = runtime.v8_isolate().thread_safe_handle();
        #[cfg(target_os = "linux")]
        let cpu_clock_id = {
            let mut clock_id: libc::clockid_t = 0;
            #[allow(unsafe_code)]
            unsafe { libc::pthread_getcpuclockid(_thread_id, &mut clock_id) };
            clock_id
        };

        let entry = Arc::new(WatchdogEntry::new(
            v8_handle,
            #[cfg(target_os = "linux")]
            cpu_clock_id,
            limits,
        ));
        watchdog_entries
            .lock()
            .unwrap()
            .insert(app_id.to_string(), entry.clone());
        entry
    };

    // --- POSIX CPU timer (Linux only) ---
    // Create a per-thread POSIX timer for precise CPU time enforcement.
    // This runs alongside the global watchdog (which handles wall-time + liveness).
    #[cfg(target_os = "linux")]
    let cpu_timer = {
        let id = crate::cpu_timer::app_id_hash(app_id);
        let v8_handle = runtime.v8_isolate().thread_safe_handle();
        cpu_timer_system.register(id, v8_handle);
        match CpuTimer::new(id) {
            Ok(timer) => {
                eprintln!("[cpu-timer] [{app_id}] Created POSIX CPU timer (id={id:#x})");
                Some(timer)
            }
            Err(e) => {
                eprintln!("[cpu-timer] [{app_id}] Failed to create POSIX timer: {e}");
                None
            }
        }
    };
    #[cfg(target_os = "linux")]
    let cpu_timer_active = std::cell::Cell::new(false);

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
        state.put(OpWatchdogEntry(watchdog_entry.clone()));
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
                            } else {
                                // Successfully injected — notify watchdog
                                watchdog_entry.start_request();

                                // Arm POSIX CPU timer on first request
                                #[cfg(target_os = "linux")]
                                if !cpu_timer_active.get() {
                                    if let Some(ref timer) = cpu_timer {
                                        timer.arm(limits.cpu_time);
                                        cpu_timer_active.set(true);
                                    }
                                }
                            }
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
            // Note: end_request() is called from op_rpc_respond via OpState

            // Disarm POSIX CPU timer when all requests complete
            #[cfg(target_os = "linux")]
            if current_pending == 0 && cpu_timer_active.get() {
                if let Some(ref timer) = cpu_timer {
                    timer.disarm();
                    cpu_timer_active.set(false);
                }
            }
        }
        prev_pending_count = current_pending;

        // Helper: handle V8 termination recovery (used by both Ok and Err paths).
        // When terminate_execution() is called from the watchdog/CPU-timer thread,
        // deno_core may surface it as Ready(Err("terminated")) OR as Ready(Ok(()))
        // depending on timing. We detect the latter by checking for pending replies
        // when the event loop claims to be done.
        let handle_termination = |runtime: &mut deno_core::JsRuntime| {
            // Cancel the termination flag so V8 can serve the next request.
            runtime.v8_isolate().cancel_terminate_execution();

            // Disarm POSIX CPU timer — will re-arm on next request
            #[cfg(target_os = "linux")]
            if cpu_timer_active.get() {
                if let Some(ref timer) = cpu_timer {
                    timer.disarm();
                    cpu_timer_active.set(false);
                }
            }

            // Drain pending replies with timeout error
            let drained = pending_replies.borrow_mut().drain().collect::<Vec<_>>();
            for (_, tx) in drained {
                let _ = tx.send(Err("Execution time limit exceeded".to_string()));
            }

            eprintln!("[isolate] [{app_id}] Recovered from execution timeout");
        };

        match poll_result {
            Poll::Ready(Ok(())) => {
                // Event loop drained. This normally happens when the JS dispatch
                // loop exits (rpc_tx was dropped, so op_rpc_recv returned null).
                if pending_replies.borrow().is_empty() {
                    return Poll::Ready(());
                }

                // Pending replies exist but the event loop reports done — this
                // means V8 was terminated externally (terminate_execution() from
                // the watchdog or CPU timer thread). deno_core sometimes surfaces
                // this as Ok(()) rather than Err("terminated").
                handle_termination(&mut runtime);
                prev_pending_count = 0;

                // Continue the poll_fn loop so the isolate can serve the next request
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Poll::Ready(Err(e)) => {
                let err_msg = format!("{e}");
                if err_msg.contains("terminated") || !pending_replies.borrow().is_empty() {
                    // V8 was terminated by the watchdog or POSIX CPU timer.
                    handle_termination(&mut runtime);
                    prev_pending_count = 0;

                    // DON'T exit — continue the poll_fn loop so the isolate can serve the next request
                    cx.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    eprintln!("[isolate] [{app_id}] Event loop error: {e}");
                    Poll::Ready(())
                }
            }
            Poll::Pending => Poll::Pending,
        }
    })
    .await;

    // Drain any remaining pending replies with errors so callers don't hang
    for (_, tx) in pending_replies.borrow_mut().drain() {
        let _ = tx.send(Err("Isolate shut down".to_string()));
    }

    // Unregister from POSIX CPU timer system
    #[cfg(target_os = "linux")]
    if let Some(ref timer) = cpu_timer {
        cpu_timer_system.unregister(timer.app_id());
    }

    eprintln!("[isolate] [{app_id}] Stopped");
}
