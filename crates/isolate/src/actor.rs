//! Isolate actor — runs a V8 isolate on a dedicated thread, handles messages.
//!
//! Each actor owns a `JsRuntime` (which is `!Send`) and runs on its own
//! single-threaded tokio runtime. Communication is via `IsolateMessage` enum
//! sent over an mpsc channel.

use appbase_core::plugin::Plugin;
use appbase_core::types::RpcResult;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use crate::cpu::CpuUsage;
use crate::isolate;

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
                &app_id, &server_js, &data_dir, plugins, rx, cpu_limit, cpu_usage_clone,
            ));
        })
        .map_err(|e| format!("Failed to spawn V8 thread: {e}"))?;

    Ok(ActorHandle { tx, cpu_usage })
}

/// The actor's main loop — runs on its own thread.
async fn actor_loop(
    app_id: &str,
    server_js: &str,
    data_dir: &PathBuf,
    plugins: Vec<Box<dyn Plugin>>,
    mut rx: mpsc::Receiver<IsolateMessage>,
    cpu_limit: Option<Duration>,
    cpu_usage: Arc<Mutex<CpuUsage>>,
) {
    // Ensure data directory exists
    let _ = std::fs::create_dir_all(data_dir);

    let (mut runtime, mut rpc_holder) = match isolate::create(&plugins, app_id, data_dir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[isolate] [{app_id}] Failed to create V8 runtime: {e}");
            return;
        }
    };

    // Load server code
    if !server_js.is_empty() {
        if let Err(e) = runtime.execute_script("<server>", server_js.to_string()) {
            eprintln!("[isolate] [{app_id}] Script error: {e}");
            return;
        }
        if let Err(e) = runtime.run_event_loop(Default::default()).await {
            eprintln!("[isolate] [{app_id}] Event loop error: {e}");
            return;
        }
    }

    eprintln!("[isolate] [{app_id}] Ready");

    // Message loop
    while let Some(msg) = rx.recv().await {
        match msg {
            IsolateMessage::Rpc { body, reply } => {
                let result =
                    isolate::handle_rpc(&mut runtime, &rpc_holder, &body, cpu_limit).await;
                let mapped = match result {
                    Ok(rpc_result) => {
                        cpu_usage.lock().unwrap().record(rpc_result.cpu_time);
                        Ok(rpc_result)
                    }
                    Err(e) => Err(e.to_string()),
                };
                let _ = reply.send(mapped);
            }
            IsolateMessage::Reload { server_js, reply } => {
                // Create fresh isolate — old one is dropped (V8 cleanup)
                match isolate::create(&plugins, app_id, data_dir) {
                    Ok((new_runtime, new_holder)) => {
                        runtime = new_runtime;
                        rpc_holder = new_holder;
                        if let Err(e) = runtime.execute_script("<server>", server_js) {
                            eprintln!("[isolate] [{app_id}] Reload script error: {e}");
                            let _ = reply.send(Err(e.to_string()));
                            continue;
                        }
                        let _ = runtime.run_event_loop(Default::default()).await;
                        eprintln!("[isolate] [{app_id}] Reloaded");
                        let _ = reply.send(Ok(()));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            IsolateMessage::Stats { reply } => {
                let usage = cpu_usage.lock().unwrap();
                let _ = reply.send(ActorStats {
                    total_cpu_ms: usage.total.as_secs_f64() * 1000.0,
                    request_count: usage.request_count,
                });
            }
            IsolateMessage::Shutdown => {
                eprintln!("[isolate] [{app_id}] Shutting down");
                break;
            }
        }
    }
}
