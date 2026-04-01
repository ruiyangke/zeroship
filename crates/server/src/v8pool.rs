//! V8 isolate pool adapter for the appbase server.
//!
//! Wraps `appbase_isolate_v8::concurrent::ConcurrentIsolate` with the interface
//! that `router.rs` expects: `dispatch(&app_id, &server_js, body) -> Result<RpcResult, String>`.

use appbase_core::config::IsolateConfig;
use appbase_core::types::{PoolStats, RpcResult};
use appbase_isolate_v8::concurrent::{ConcurrentIsolate, Event};
use appbase_isolate_v8::init_v8;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// A pool of V8 worker threads that dispatch RPC requests via channels.
///
/// Currently single-app mode: one set of workers, round-robin dispatch.
pub struct V8Pool {
    senders: Vec<std::sync::mpsc::Sender<Event>>,
    next: AtomicU64,
    next_id: AtomicU64,
    wall_timeout: Duration,
}

impl V8Pool {
    /// Create a new V8Pool, spawning worker threads and warming them up.
    ///
    /// Must be called from within a Tokio runtime context.
    pub fn new(server_js: &str, config: &IsolateConfig) -> Self {
        init_v8();

        let num_workers = 1; // single concurrent thread per app (can scale later)
        let cpu_limit = config.cpu_limit();
        let tokio_handle = tokio::runtime::Handle::current();

        let mut senders = Vec::with_capacity(num_workers);
        for i in 0..num_workers {
            let (event_tx, event_rx) = std::sync::mpsc::channel();
            let event_tx_clone = event_tx.clone();
            let js = server_js.to_string();
            let handle = tokio_handle.clone();
            let limit = cpu_limit;

            std::thread::Builder::new()
                .name(format!("v8-worker-{i}"))
                .spawn(move || {
                    let mut isolate =
                        ConcurrentIsolate::new(&js, event_rx, event_tx_clone, Some(handle), limit);
                    isolate.run_event_loop();
                })
                .expect("Failed to spawn V8 worker thread");

            // Warmup: send a ping request and wait for response to ensure the
            // isolate is initialized before we start serving traffic.
            let warmup_tx = event_tx.clone();
            std::thread::spawn(move || {
                let (tx, rx) = tokio::sync::oneshot::channel();
                warmup_tx
                    .send(Event::NewRequest {
                        id: 0,
                        body: r#"{"jsonrpc":"2.0","method":"__ping","params":[],"id":0}"#
                            .to_string(),
                        reply: tx,
                    })
                    .unwrap();
                let _ = rx.blocking_recv();
            })
            .join()
            .unwrap();

            senders.push(event_tx);
        }

        Self {
            senders,
            next: AtomicU64::new(0),
            next_id: AtomicU64::new(1),
            wall_timeout: Duration::from_secs(30),
        }
    }

    /// Dispatch an RPC request to a V8 worker via round-robin.
    ///
    /// `_app_id` and `_server_js` are accepted for interface compatibility with
    /// the router but currently unused (single-app mode).
    pub async fn dispatch(
        &self,
        _app_id: &str,
        _server_js: &str,
        body: String,
    ) -> Result<RpcResult, String> {
        let idx = (self.next.fetch_add(1, Ordering::Relaxed) as usize) % self.senders.len();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();

        self.senders[idx]
            .send(Event::NewRequest {
                id,
                body,
                reply: reply_tx,
            })
            .map_err(|_| "V8 worker channel closed".to_string())?;

        match tokio::time::timeout(self.wall_timeout, reply_rx).await {
            Ok(Ok(Ok(result))) => Ok(RpcResult {
                json: result.json,
                cpu_time: result.cpu_time,
            }),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => Err("V8 worker dropped reply".to_string()),
            Err(_) => Err("Request timed out (30s wall time)".to_string()),
        }
    }

    /// Return pool statistics. Minimal for now since V8Pool doesn't track
    /// per-app isolate state the way the deno_core pool did.
    pub fn stats(&self) -> PoolStats {
        PoolStats {
            active_isolates: self.senders.len(),
            max_isolates: self.senders.len(),
            apps: Vec::new(),
        }
    }

    /// Evict an app's isolate. No-op in single-app V8Pool mode.
    pub fn evict_app(&self, _app_id: &str) {
        // V8Pool currently runs a single set of persistent workers.
        // Multi-app eviction will be added when we support per-app isolates.
    }
}

impl Drop for V8Pool {
    fn drop(&mut self) {
        for tx in &self.senders {
            let _ = tx.send(Event::Shutdown);
        }
    }
}
