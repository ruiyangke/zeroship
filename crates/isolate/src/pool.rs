//! Isolate pool — manages one actor per app with idle eviction.
//!
//! Thread-safe: the pool is shared across axum handler threads via `Arc`.
//! Each app gets its own V8 isolate running on a dedicated thread.

use appbase_core::config::IsolateConfig;
use appbase_core::plugin::PluginFactory;
use appbase_core::types::RpcResult;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::actor::{self, ActorHandle};

/// Per-app entry in the pool.
struct PoolEntry {
    handle: ActorHandle,
    server_js: String,
    last_used: Instant,
}

/// Pool of isolate actors, one per app.
pub struct IsolatePool {
    entries: Mutex<HashMap<String, PoolEntry>>,
    config: IsolateConfig,
    data_dir: PathBuf,
    plugin_factory: PluginFactory,
}

impl IsolatePool {
    /// Create a new pool. Starts a background eviction task.
    pub fn new(
        config: IsolateConfig,
        data_dir: PathBuf,
        plugin_factory: PluginFactory,
    ) -> Arc<Self> {
        let pool = Arc::new(Self {
            entries: Mutex::new(HashMap::new()),
            config,
            data_dir,
            plugin_factory,
        });

        // Background eviction task
        let pool_ref = pool.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                pool_ref.evict_idle();
            }
        });

        pool
    }

    /// Dispatch an RPC request to the app's isolate.
    /// Creates a new isolate if one doesn't exist for this app.
    pub async fn dispatch(
        &self,
        app_id: &str,
        server_js: &str,
        body: String,
    ) -> Result<RpcResult, String> {
        let handle = self.get_or_create(app_id, server_js)?;
        handle.rpc(body).await
    }

    /// Get or create an actor for the given app.
    fn get_or_create(&self, app_id: &str, server_js: &str) -> Result<ActorHandle, String> {
        let mut entries = self.entries.lock().unwrap();

        // Return existing if alive
        if let Some(entry) = entries.get_mut(app_id) {
            if entry.handle.is_alive() {
                entry.last_used = Instant::now();
                return Ok(entry.handle.clone());
            }
            entries.remove(app_id);
        }

        // Evict oldest if at capacity
        if entries.len() >= self.config.max {
            if let Some(oldest_key) = entries
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())
            {
                eprintln!("[pool] Evicting: {oldest_key}");
                entries.remove(&oldest_key);
            }
        }

        // Spawn new actor
        let app_data_dir = self.data_dir.join(app_id);
        let plugins = (self.plugin_factory)(app_id);

        let handle = actor::spawn(
            app_id,
            server_js,
            &app_data_dir,
            plugins,
            self.config.cpu_limit(),
        )?;

        eprintln!("[pool] Started: {app_id}");

        entries.insert(
            app_id.to_string(),
            PoolEntry {
                handle: handle.clone(),
                server_js: server_js.to_string(),
                last_used: Instant::now(),
            },
        );

        Ok(handle)
    }

    /// Evict isolates that have been idle longer than the configured timeout.
    fn evict_idle(&self) {
        let timeout = self.config.idle_timeout();
        let mut entries = self.entries.lock().unwrap();
        let before = entries.len();

        entries.retain(|id, entry| {
            if entry.last_used.elapsed() > timeout {
                eprintln!("[pool] Evicting idle: {id}");
                false
            } else {
                true
            }
        });

        let evicted = before - entries.len();
        if evicted > 0 {
            eprintln!("[pool] Evicted {evicted}, {} active", entries.len());
        }
    }

    /// Get pool statistics.
    pub fn stats(&self) -> appbase_core::types::PoolStats {
        let entries = self.entries.lock().unwrap();
        let apps = entries
            .iter()
            .map(|(id, entry)| {
                let usage = entry.handle.cpu_usage.lock().unwrap();
                appbase_core::types::IsolateStats {
                    app_id: id.clone(),
                    total_cpu_ms: usage.total.as_secs_f64() * 1000.0,
                    request_count: usage.request_count,
                    idle_secs: entry.last_used.elapsed().as_secs_f64(),
                }
            })
            .collect();

        appbase_core::types::PoolStats {
            active_isolates: entries.len(),
            max_isolates: self.config.max,
            apps,
        }
    }
}
