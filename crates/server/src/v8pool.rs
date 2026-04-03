//! Multi-tenant V8 isolate pool.
//!
//! One `ConcurrentIsolate` per app, created lazily on first request.
//! LRU eviction when pool reaches capacity or idle timeout expires.

use appbase_core::config::IsolateConfig;
use appbase_core::types::{IsolateStats, PoolStats, RpcResult};
use appbase_runtime::concurrent::{ConcurrentIsolate, Event};
use appbase_runtime::modules::ModuleEntry;
use appbase_runtime::init_v8;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// Per-app isolate entry in the pool.
struct IsolateEntry {
    /// Channel to send requests to the isolate's worker thread.
    sender: std::sync::mpsc::Sender<Event>,
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
/// Each app gets its own `ConcurrentIsolate` on a dedicated thread.
/// Isolates are created lazily and evicted when idle or at capacity.
pub struct V8Pool {
    /// Map of app_id → isolate entry. RwLock for concurrent reads (dispatch)
    /// with rare writes (create/evict).
    isolates: RwLock<HashMap<String, Arc<IsolateEntry>>>,
    config: IsolateConfig,
    tokio_handle: tokio::runtime::Handle,
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
            tokio_handle: tokio::runtime::Handle::current(),
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

        entry
            .sender
            .send(Event::NewRequest {
                id,
                body,
                reply: reply_tx,
            })
            .map_err(|_| format!("V8 isolate for '{app_id}' is dead"))?;

        entry.request_count.fetch_add(1, Ordering::Relaxed);
        entry.last_used_ms.store(epoch_ms(), Ordering::Relaxed);

        match tokio::time::timeout(self.wall_timeout, reply_rx).await {
            Ok(Ok(Ok(result))) => {
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
                // Update last_used (interior mutability via atomic would be better,
                // but Instant isn't atomic. We accept a slight staleness here —
                // eviction checks under write lock will see the latest value.)
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

    /// Spawn a new ConcurrentIsolate on a dedicated thread.
    fn spawn_isolate(
        &self,
        app_id: &str,
        server_js: &str,
    ) -> Result<IsolateEntry, String> {
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let event_tx_clone = event_tx.clone();
        let modules = vec![ModuleEntry {
            specifier: "index.js".into(),
            source: server_js.into(),
        }];
        let handle = self.tokio_handle.clone();
        let cpu_limit = self.config.cpu_limit();
        let thread_name = format!("v8-{app_id}");

        std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let mut isolate =
                    ConcurrentIsolate::new(modules, event_rx, event_tx_clone, Some(handle), cpu_limit, std::collections::HashMap::new());
                isolate.run_event_loop();
            })
            .map_err(|e| format!("Failed to spawn V8 thread: {e}"))?;

        // Warmup
        let warmup_tx = event_tx.clone();
        std::thread::spawn(move || {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = warmup_tx.send(Event::NewRequest {
                id: 0,
                body: r#"{"jsonrpc":"2.0","method":"__ping","params":[],"id":0}"#.to_string(),
                reply: tx,
            });
            let _ = rx.blocking_recv();
        })
        .join()
        .map_err(|_| "Warmup thread panicked".to_string())?;

        Ok(IsolateEntry {
            sender: event_tx,
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
                let _ = entry.sender.send(Event::Shutdown);
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
                let _ = entry.sender.send(Event::Shutdown);
                eprintln!("[pool] Evicted '{id}' (idle {}s)", self.config.idle_timeout_secs);
            }
        }
    }

    /// Evict a specific app's isolate.
    pub fn evict_app(&self, app_id: &str) {
        let mut isolates = self.isolates.write().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = isolates.remove(app_id) {
            let _ = entry.sender.send(Event::Shutdown);
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
                let _ = entry.sender.send(Event::Shutdown);
            }
        }
    }
}
