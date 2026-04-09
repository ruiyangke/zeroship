//! Multi-tenant V8 isolate pool.
//!
//! One `Runtime` per app, created lazily on first request.
//! LRU eviction when pool reaches capacity or idle timeout expires.
//!
//! Each V8 worker runs on a dedicated thread with its own compio runtime.
//! `flume` channels bridge the tokio HTTP handler ↔ compio V8 worker.

use crate::core::config::IsolateConfig;
use crate::core::types::{IsolateStats, PoolStats, RpcResult};
use appbase_runtime::modules::ModuleEntry;
use appbase_runtime::init_v8;
use appbase_runtime::runtime::Runtime;
use appbase_runtime::{AsyncWork, AsyncEvent, DispatchOutcome};

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

// ---------------------------------------------------------------------------
// WorkRequest / WorkResult — flume payload
// ---------------------------------------------------------------------------

/// A request sent from the tokio side (axum handler) to a compio V8 worker.
struct WorkRequest {
    body: String,
    reply: flume::Sender<WorkResult>,
}

/// A result sent back from the compio V8 worker to the tokio side.
enum WorkResult {
    Complete {
        json: String,
        cpu_time: Duration,
        logs: Vec<String>,
    },
    Error(String),
}

/// Result from V8Pool dispatch — currently only RPC (streaming can be added later).
#[derive(Debug)]
pub enum PoolDispatchResult {
    /// Complete response (JSON-RPC).
    Rpc(RpcResult),
}

// ---------------------------------------------------------------------------
// IsolateEntry
// ---------------------------------------------------------------------------

/// Per-app isolate entry in the pool.
struct IsolateEntry {
    /// Channel to send requests to the isolate's compio worker thread.
    request_tx: flume::Sender<WorkRequest>,
    /// Last time a request was dispatched (epoch millis, atomic for concurrent updates).
    last_used_ms: AtomicU64,
    /// Total requests dispatched.
    request_count: AtomicU64,
    /// Per-app log ring buffer (last 100 entries).
    logs: std::sync::Mutex<Vec<String>>,
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ---------------------------------------------------------------------------
// V8Pool
// ---------------------------------------------------------------------------

/// Multi-tenant V8 isolate pool.
///
/// Each app gets its own `Runtime` on a dedicated thread running a compio
/// event loop. Isolates are created lazily and evicted when idle or at capacity.
pub struct V8Pool {
    /// Map of app_id → isolate entry. RwLock for concurrent reads (dispatch)
    /// with rare writes (create/evict).
    isolates: RwLock<HashMap<String, Arc<IsolateEntry>>>,
    config: IsolateConfig,
    wall_timeout: Duration,
}

impl V8Pool {
    /// Create a new multi-tenant V8 pool.
    pub fn new(config: &IsolateConfig) -> Self {
        init_v8();

        Self {
            isolates: RwLock::new(HashMap::new()),
            config: config.clone(),
            wall_timeout: Duration::from_secs(30),
        }
    }

    /// Dispatch an RPC request to the app's isolate.
    /// Creates a new isolate if one doesn't exist for this app.
    pub async fn dispatch(
        &self,
        app_id: &str,
        server_js: &str,
        body: String,
    ) -> Result<PoolDispatchResult, String> {
        let entry = self.get_or_create(app_id, server_js)?;

        let (reply_tx, reply_rx) = flume::bounded(1);

        entry
            .request_tx
            .send_async(WorkRequest { body, reply: reply_tx })
            .await
            .map_err(|_| format!("V8 isolate for '{app_id}' is dead"))?;

        entry.request_count.fetch_add(1, Ordering::Relaxed);
        entry.last_used_ms.store(epoch_ms(), Ordering::Relaxed);

        // flume::Receiver::recv_async() is runtime-agnostic — works with tokio
        match tokio::time::timeout(self.wall_timeout, reply_rx.recv_async()).await {
            Ok(Ok(WorkResult::Complete { json, cpu_time, logs })) => {
                // Store logs in per-app ring buffer
                if !logs.is_empty() {
                    let mut app_logs = entry.logs.lock().unwrap_or_else(|e| e.into_inner());
                    app_logs.extend(logs.iter().cloned());
                    // Keep last 100
                    if app_logs.len() > 100 {
                        let drain = app_logs.len() - 100;
                        app_logs.drain(..drain);
                    }
                }
                Ok(PoolDispatchResult::Rpc(RpcResult {
                    json,
                    cpu_time,
                    logs,
                }))
            }
            Ok(Ok(WorkResult::Error(e))) => Err(e),
            Ok(Err(_)) => Err("V8 worker dropped reply".to_string()),
            Err(_) => Err("Request timed out (30s wall time)".to_string()),
        }
    }

    /// Get an existing isolate or create a new one for this app.
    fn get_or_create(
        &self,
        app_id: &str,
        server_js: &str,
    ) -> Result<Arc<IsolateEntry>, String> {
        // Fast path: read lock
        {
            let isolates = self.isolates.read().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = isolates.get(app_id) {
                return Ok(entry.clone());
            }
        }

        // Slow path: write lock, create isolate
        let mut isolates = self.isolates.write().unwrap_or_else(|e| e.into_inner());

        // Double-check (another thread may have created it)
        if let Some(entry) = isolates.get(app_id) {
            return Ok(entry.clone());
        }

        // Evict if at capacity
        if isolates.len() >= self.config.max {
            self.evict_lru(&mut isolates);
        }

        // Spawn new isolate
        let entry = self.spawn_isolate(app_id, server_js)?;
        let entry = Arc::new(entry);
        isolates.insert(app_id.to_string(), entry.clone());

        eprintln!(
            "[pool] Started isolate for '{}' ({}/{} slots)",
            app_id,
            isolates.len(),
            self.config.max
        );

        Ok(entry)
    }

    /// Spawn a new Runtime on a dedicated thread with a compio event loop.
    fn spawn_isolate(
        &self,
        app_id: &str,
        server_js: &str,
    ) -> Result<IsolateEntry, String> {
        let (request_tx, request_rx) = flume::bounded::<WorkRequest>(256);

        let modules = vec![ModuleEntry {
            specifier: "index.js".into(),
            source: server_js.into(),
        }];
        let cpu_limit = self.config.cpu_limit();
        let wall_timeout = Some(Duration::from_secs(30));
        let thread_name = format!("v8-{app_id}");

        std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                run_compio_worker(request_rx, modules, cpu_limit, wall_timeout);
            })
            .map_err(|e| format!("Failed to spawn V8 thread: {e}"))?;

        // Warmup: send a ping and wait for the reply synchronously.
        let (reply_tx, reply_rx) = flume::bounded(1);
        request_tx
            .send(WorkRequest {
                body: r#"{"jsonrpc":"2.0","method":"__ping","params":[],"id":0}"#.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| "Warmup send failed — isolate thread died".to_string())?;
        let _ = reply_rx.recv_timeout(Duration::from_secs(10));

        Ok(IsolateEntry {
            request_tx,
            last_used_ms: AtomicU64::new(epoch_ms()),
            request_count: AtomicU64::new(0),
            logs: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// Evict the least-recently-used isolate. Called under write lock.
    fn evict_lru(&self, isolates: &mut HashMap<String, Arc<IsolateEntry>>) {
        let oldest = isolates
            .iter()
            .min_by_key(|(_, entry)| entry.last_used_ms.load(Ordering::Relaxed))
            .map(|(id, _)| id.clone());

        if let Some(id) = oldest {
            if let Some(_entry) = isolates.remove(&id) {
                // Dropping the Sender half causes the compio worker to exit
                // when it tries recv_async().
                eprintln!("[pool] Evicted '{id}' (LRU, pool full)");
            }
        }
    }

    /// Evict idle isolates (called periodically by background task).
    pub fn evict_idle(&self) {
        let timeout = Duration::from_secs(self.config.idle_timeout_secs);
        let mut isolates = self.isolates.write().unwrap_or_else(|e| e.into_inner());
        let now_ms = epoch_ms();

        let idle: Vec<String> = isolates
            .iter()
            .filter(|(_, entry)| {
                let idle_ms = now_ms.saturating_sub(entry.last_used_ms.load(Ordering::Relaxed));
                idle_ms > timeout.as_millis() as u64
            })
            .map(|(id, _)| id.clone())
            .collect();

        for id in idle {
            if let Some(_entry) = isolates.remove(&id) {
                eprintln!("[pool] Evicted '{id}' (idle {}s)", self.config.idle_timeout_secs);
            }
        }
    }

    /// Evict a specific app's isolate.
    pub fn evict_app(&self, app_id: &str) {
        let mut isolates = self.isolates.write().unwrap_or_else(|e| e.into_inner());
        if let Some(_entry) = isolates.remove(app_id) {
            eprintln!("[pool] Evicted '{app_id}' (manual)");
        }
    }

    /// Return recent console logs for an app.
    pub fn get_logs(&self, app_id: &str) -> Vec<String> {
        let isolates = self.isolates.read().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = isolates.get(app_id) {
            if let Ok(logs) = entry.logs.lock() {
                return logs.clone();
            }
        }
        Vec::new()
    }

    /// Return pool statistics.
    pub fn stats(&self) -> PoolStats {
        let isolates = self.isolates.read().unwrap_or_else(|e| e.into_inner());
        let apps: Vec<IsolateStats> = isolates
            .iter()
            .map(|(id, entry)| IsolateStats {
                app_id: id.clone(),
                total_cpu_ms: 0.0, // TODO: track per-isolate CPU
                request_count: entry.request_count.load(Ordering::Relaxed),
                idle_secs: (epoch_ms().saturating_sub(entry.last_used_ms.load(Ordering::Relaxed))) as f64 / 1000.0,
            })
            .collect();

        PoolStats {
            active_isolates: isolates.len(),
            max_isolates: self.config.max,
            apps,
        }
    }
}

impl Drop for V8Pool {
    fn drop(&mut self) {
        // Dropping all IsolateEntry (and their request_tx) causes workers to exit.
        if let Ok(mut isolates) = self.isolates.write() {
            isolates.clear();
        }
    }
}

// ===========================================================================
// Compio V8 worker — runs on a dedicated thread
// ===========================================================================

/// Yield control back to the compio event loop so other tasks (pump) can run.
fn yield_now() -> impl std::future::Future<Output = ()> {
    let mut yielded = false;
    std::future::poll_fn(move |cx| {
        if yielded {
            std::task::Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
}

/// Run a compio event loop on the current thread, processing WorkRequests.
fn run_compio_worker(
    request_rx: flume::Receiver<WorkRequest>,
    modules: Vec<ModuleEntry>,
    cpu_limit: Option<Duration>,
    wall_timeout: Option<Duration>,
) {
    compio::runtime::RuntimeBuilder::new()
        .build()
        .unwrap()
        .block_on(async {
            let runtime = Rc::new(RefCell::new(
                Runtime::new_direct(modules, HashMap::new(), cpu_limit, wall_timeout),
            ));

            // Spawn the pump task for async V8 work (fetch, timers, etc.)
            let mut async_work = AsyncWork::new();
            let (notify_tx, notify_rx) = futures::channel::mpsc::channel::<()>(1);
            runtime.borrow_mut().set_pump_notify(notify_tx);
            runtime.borrow_mut().drain_new_tasks_into(&mut async_work);

            let rt_pump = runtime.clone();
            compio::runtime::spawn(pump_task(rt_pump, async_work, notify_rx)).detach();

            // Request loop: receive work from flume, dispatch into V8
            while let Ok(req) = request_rx.recv_async().await {
                let outcome = runtime.borrow_mut().dispatch_start(&req.body);

                match outcome {
                    DispatchOutcome::Complete(Ok(result)) => {
                        let _ = req.reply.send(WorkResult::Complete {
                            json: result.json,
                            cpu_time: result.cpu_time,
                            logs: result.logs,
                        });
                    }
                    DispatchOutcome::Complete(Err(e)) => {
                        let _ = req.reply.send(WorkResult::Error(e));
                    }
                    DispatchOutcome::Pending(rx) => {
                        // Async handler — poll the result slot while pump drives
                        // the promise to completion. yield_now lets the pump task run.
                        let wall_limit = runtime.borrow().wall_timeout();
                        let deadline = wall_limit.map(|d| std::time::Instant::now() + d);
                        let result = loop {
                            if let Some(result) = rx.try_recv() {
                                break Some(result);
                            }
                            if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                                break None;
                            }
                            yield_now().await;
                        };
                        match result {
                            Some(Ok(r)) => {
                                let _ = req.reply.send(WorkResult::Complete {
                                    json: r.json,
                                    cpu_time: r.cpu_time,
                                    logs: r.logs,
                                });
                            }
                            Some(Err(e)) => {
                                let _ = req.reply.send(WorkResult::Error(e));
                            }
                            None => {
                                let _ = req.reply.send(WorkResult::Error(
                                    "Request timed out".to_string(),
                                ));
                            }
                        }
                    }
                    // HTTP variants — not used from the V8Pool RPC path
                    DispatchOutcome::HttpComplete { body, logs, .. } => {
                        let _ = req.reply.send(WorkResult::Complete {
                            json: body,
                            cpu_time: Duration::ZERO,
                            logs,
                        });
                    }
                    DispatchOutcome::HttpStream { .. }
                    | DispatchOutcome::HttpPending(_)
                    | DispatchOutcome::WebSocketUpgrade { .. } => {
                        let _ = req.reply.send(WorkResult::Error(
                            "Streaming/WebSocket not supported through V8Pool".to_string(),
                        ));
                    }
                }
            }
            // request_rx closed — worker exits naturally
        });
}

// ===========================================================================
// Pump task — drives async V8 work (ops, timers) to completion
// ===========================================================================

/// Background task that owns `AsyncWork` and drives pending ops/timers.
/// Same architecture as runtime-compio's standalone server.
async fn pump_task(
    runtime: Rc<RefCell<Runtime>>,
    mut work: AsyncWork,
    mut notify_rx: futures::channel::mpsc::Receiver<()>,
) {
    use futures::StreamExt;

    loop {
        // Drain any newly spawned tasks (from dispatch_start calls)
        {
            let mut rt = runtime.borrow_mut();
            rt.drain_new_tasks_into(&mut work);
        }

        let event = {
            let has_ops = !work.pending_ops.is_empty();
            let has_timers = !work.pending_timers.is_empty();

            match (has_ops, has_timers) {
                (true, true) => {
                    futures::select! {
                        r = work.pending_ops.select_next_some() => Some(AsyncEvent::Op(r)),
                        r = work.pending_timers.select_next_some() => Some(AsyncEvent::Timer(r)),
                        _ = notify_rx.next() => None,
                    }
                }
                (true, false) => {
                    futures::select! {
                        r = work.pending_ops.select_next_some() => Some(AsyncEvent::Op(r)),
                        _ = notify_rx.next() => None,
                    }
                }
                (false, true) => {
                    futures::select! {
                        r = work.pending_timers.select_next_some() => Some(AsyncEvent::Timer(r)),
                        _ = notify_rx.next() => None,
                    }
                }
                (false, false) => {
                    let _ = notify_rx.next().await;
                    None
                }
            }
        };

        if let Some(event) = event {
            let mut rt = runtime.borrow_mut();
            rt.handle_async_event(event, &mut work);
        }
    }
}
