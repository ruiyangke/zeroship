//! Multi-tenant V8 isolate pool.
//!
//! One `Runtime` per app, created lazily on first request.
//! LRU eviction when pool reaches capacity or idle timeout expires.

use crate::core::config::IsolateConfig;
use crate::core::types::{IsolateStats, PoolStats, RpcResult};
use appbase_runtime::{IncomingRequest, Runtime};
use appbase_runtime::modules::ModuleEntry;
use appbase_runtime::state::RequestReply;
use appbase_runtime::init_v8;
use tokio_util::sync::CancellationToken;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// Per-app isolate entry in the pool.
struct IsolateEntry {
    /// Channel to send requests to the isolate's worker thread.
    request_tx: tokio::sync::mpsc::Sender<IncomingRequest>,
    /// Cancellation token to shut down the isolate.
    shutdown: CancellationToken,
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

/// Multi-tenant V8 isolate pool.
///
/// Each app gets its own `Runtime` on a dedicated thread.
/// Isolates are created lazily and evicted when idle or at capacity.
pub struct V8Pool {
    /// Map of app_id → isolate entry. RwLock for concurrent reads (dispatch)
    /// with rare writes (create/evict).
    isolates: RwLock<HashMap<String, Arc<IsolateEntry>>>,
    config: IsolateConfig,
    next_request_id: AtomicU64,
    wall_timeout: Duration,
}

impl V8Pool {
    /// Create a new multi-tenant V8 pool.
    ///
    /// Must be called from within a Tokio runtime context.
    pub fn new(config: &IsolateConfig) -> Self {
        init_v8();

        Self {
            isolates: RwLock::new(HashMap::new()),
            config: config.clone(),
            next_request_id: AtomicU64::new(1),
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
    ) -> Result<RpcResult, String> {
        let entry = self.get_or_create(app_id, server_js)?;

        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let cancel = CancellationToken::new();

        entry
            .request_tx
            .send(IncomingRequest {
                id,
                body,
                reply: reply_tx,
                cancel,
            })
            .await
            .map_err(|_| format!("V8 isolate for '{app_id}' is dead"))?;

        entry.request_count.fetch_add(1, Ordering::Relaxed);
        entry.last_used_ms.store(epoch_ms(), Ordering::Relaxed);

        match tokio::time::timeout(self.wall_timeout, reply_rx).await {
            Ok(Ok(Ok(RequestReply::Complete(result)))) => {
                // Store logs in per-app ring buffer
                if !result.logs.is_empty() {
                    let mut app_logs = entry.logs.lock().unwrap_or_else(|e| e.into_inner());
                    app_logs.extend(result.logs.iter().cloned());
                    // Keep last 100
                    if app_logs.len() > 100 {
                        let drain = app_logs.len() - 100;
                        app_logs.drain(..drain);
                    }
                }
                Ok(RpcResult {
                    json: result.json,
                    cpu_time: result.cpu_time,
                    logs: result.logs,
                })
            }
            Ok(Ok(Ok(RequestReply::Stream(_)))) => {
                // TODO: streaming responses not yet supported in platform layer
                Err("Streaming responses not yet supported".to_string())
            }
            Ok(Ok(Err(e))) => Err(e),
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

    /// Spawn a new Runtime on a dedicated thread.
    fn spawn_isolate(
        &self,
        app_id: &str,
        server_js: &str,
    ) -> Result<IsolateEntry, String> {
        let (request_tx, request_rx) = tokio::sync::mpsc::channel::<IncomingRequest>(256);
        let shutdown = CancellationToken::new();
        let shutdown_inner = shutdown.clone();

        let modules = vec![ModuleEntry {
            specifier: "index.js".into(),
            source: server_js.into(),
        }];
        let cpu_limit = self.config.cpu_limit();
        let thread_name = format!("v8-{app_id}");

        // Capture the server's multi-threaded tokio handle so fetch I/O
        // can be spawned on it instead of the isolate's single-threaded runtime.
        let server_handle = tokio::runtime::Handle::current();

        std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime build failed")
                    .block_on(async {
                        Runtime::new(
                            modules,
                            request_rx,
                            shutdown_inner,
                            cpu_limit,
                            Some(Duration::from_secs(30)),
                            HashMap::new(),
                            Some(server_handle),
                        )
                        .run()
                        .await
                    });
            })
            .map_err(|e| format!("Failed to spawn V8 thread: {e}"))?;

        // Warmup: send a ping and wait for the reply synchronously.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        request_tx
            .blocking_send(IncomingRequest {
                id: 0,
                body: r#"{"jsonrpc":"2.0","method":"__ping","params":[],"id":0}"#.to_string(),
                reply: reply_tx,
                cancel: CancellationToken::new(),
            })
            .map_err(|_| "Warmup send failed — isolate thread died".to_string())?;
        let _ = reply_rx.blocking_recv();

        Ok(IsolateEntry {
            request_tx,
            shutdown,
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
            if let Some(entry) = isolates.remove(&id) {
                entry.shutdown.cancel();
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
            if let Some(entry) = isolates.remove(&id) {
                entry.shutdown.cancel();
                eprintln!("[pool] Evicted '{id}' (idle {}s)", self.config.idle_timeout_secs);
            }
        }
    }

    /// Evict a specific app's isolate.
    pub fn evict_app(&self, app_id: &str) {
        let mut isolates = self.isolates.write().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = isolates.remove(app_id) {
            entry.shutdown.cancel();
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
        if let Ok(isolates) = self.isolates.read() {
            for (_, entry) in isolates.iter() {
                entry.shutdown.cancel();
            }
        }
    }
}
