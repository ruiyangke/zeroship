//! Isolate pool — manages one actor per app with multiple eviction strategies.
//!
//! Eviction policies (all run every 10s):
//! - **Idle timeout**: evict isolates not used within `idle_timeout_secs`
//! - **CPU quota**: evict apps that exceeded `cpu_quota_ms` total CPU time
//! - **Memory pressure**: evict LRU when total process RSS > `max_memory_mb`
//! - **Capacity**: evict oldest when pool is full and new app needs a slot
//! - **Manual**: `evict_app(id)` for admin API
//!
//! Graceful shutdown: sends `IsolateMessage::Shutdown` before dropping the actor,
//! giving the isolate a chance to flush state and close resources.

use appbase_core::config::IsolateConfig;
use appbase_core::plugin::{MeterFactory, PluginFactory};
use appbase_core::types::RpcResult;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::actor::{self, ActorHandle, IsolateMessage};

/// Per-app entry in the pool.
struct PoolEntry {
    handle: ActorHandle,
    last_used: Instant,
}

impl PoolEntry {
    /// Send a graceful shutdown message to the actor before dropping.
    fn graceful_shutdown(&self) {
        let tx = self.handle.tx.clone();
        // Best-effort: if the channel is full or closed, we just drop
        let _ = tx.try_send(IsolateMessage::Shutdown);
    }
}

impl Drop for PoolEntry {
    fn drop(&mut self) {
        self.graceful_shutdown();
    }
}

/// Pool of isolate actors, one per app.
pub struct IsolatePool {
    entries: Mutex<HashMap<String, PoolEntry>>,
    config: IsolateConfig,
    data_dir: PathBuf,
    plugin_factory: PluginFactory,
    meter_factory: MeterFactory,
}

impl IsolatePool {
    /// Create a new pool. Starts a background eviction task.
    pub fn new(
        config: IsolateConfig,
        data_dir: PathBuf,
        plugin_factory: PluginFactory,
        meter_factory: MeterFactory,
    ) -> Arc<Self> {
        let pool = Arc::new(Self {
            entries: Mutex::new(HashMap::new()),
            config,
            data_dir,
            plugin_factory,
            meter_factory,
        });

        // Background eviction task — runs all policies every 10s
        let pool_ref = pool.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                pool_ref.run_eviction();
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

    /// Manually evict a specific app's isolate (for admin API).
    /// Returns true if the app was found and evicted.
    pub fn evict_app(&self, app_id: &str) -> bool {
        let mut entries = self.entries.lock().unwrap();
        if entries.remove(app_id).is_some() {
            eprintln!("[pool] Manually evicted: {app_id}");
            true
        } else {
            false
        }
    }

    /// Shut down all isolates gracefully.
    pub fn shutdown_all(&self) {
        let mut entries = self.entries.lock().unwrap();
        let count = entries.len();
        entries.clear(); // Drop triggers graceful shutdown via PoolEntry::drop
        eprintln!("[pool] Shut down {count} isolates");
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
            // Dead actor — remove without graceful shutdown (already dead)
            entries.remove(app_id);
        }

        // Evict oldest if at capacity
        if entries.len() >= self.config.max {
            if let Some(oldest_key) = entries
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())
            {
                eprintln!("[pool] Capacity eviction: {oldest_key}");
                entries.remove(&oldest_key);
            }
        }

        // Spawn new actor
        let app_data_dir = self.data_dir.join(app_id);
        let plugins = (self.plugin_factory)(app_id);
        let meter = (self.meter_factory)(app_id);

        let handle = actor::spawn(
            app_id,
            server_js,
            &app_data_dir,
            plugins,
            self.config.cpu_limit(),
            meter,
        )?;

        eprintln!("[pool] Started: {app_id}");

        entries.insert(
            app_id.to_string(),
            PoolEntry {
                handle: handle.clone(),
                last_used: Instant::now(),
            },
        );

        Ok(handle)
    }

    /// Run all eviction policies.
    fn run_eviction(&self) {
        self.evict_idle();
        self.evict_cpu_quota();
        self.evict_memory_pressure();
    }

    /// Evict isolates idle longer than the configured timeout.
    fn evict_idle(&self) {
        let timeout = self.config.idle_timeout();
        let mut entries = self.entries.lock().unwrap();
        let before = entries.len();

        entries.retain(|id, entry| {
            if entry.last_used.elapsed() > timeout {
                eprintln!("[pool] Idle eviction: {id}");
                false
            } else {
                true
            }
        });

        let evicted = before - entries.len();
        if evicted > 0 {
            eprintln!("[pool] Evicted {evicted} idle, {} active", entries.len());
        }
    }

    /// Evict apps that exceeded their total CPU quota.
    fn evict_cpu_quota(&self) {
        let quota = match self.config.cpu_quota() {
            Some(q) => q,
            None => return, // No quota configured
        };

        let mut entries = self.entries.lock().unwrap();
        let before = entries.len();

        entries.retain(|id, entry| {
            let usage = entry.handle.cpu_usage.lock().unwrap();
            if usage.total > quota {
                eprintln!(
                    "[pool] CPU quota eviction: {id} (used {:.1}ms, limit {:.1}ms)",
                    usage.total.as_secs_f64() * 1000.0,
                    quota.as_secs_f64() * 1000.0,
                );
                false
            } else {
                true
            }
        });

        let evicted = before - entries.len();
        if evicted > 0 {
            eprintln!("[pool] Evicted {evicted} over CPU quota, {} active", entries.len());
        }
    }

    /// Evict LRU isolates when process memory exceeds the configured limit.
    fn evict_memory_pressure(&self) {
        let max_mb = match self.config.max_memory_mb {
            Some(m) => m,
            None => return, // No memory limit configured
        };

        let current_rss_mb = process_rss_mb();
        if current_rss_mb <= max_mb {
            return; // Under limit
        }

        let mut entries = self.entries.lock().unwrap();

        // Evict LRU entries one at a time until under limit or pool is empty
        while process_rss_mb() > max_mb && !entries.is_empty() {
            let oldest_key = entries
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone());

            if let Some(key) = oldest_key {
                eprintln!(
                    "[pool] Memory pressure eviction: {key} (RSS={current_rss_mb}MB, limit={max_mb}MB)"
                );
                entries.remove(&key);
            } else {
                break;
            }
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

/// Read current process RSS in MB from /proc/self/status.
fn process_rss_mb() -> usize {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                if line.starts_with("VmRSS:") {
                    line.split_whitespace()
                        .nth(1)?
                        .parse::<usize>()
                        .ok()
                        .map(|kb| kb / 1024)
                } else {
                    None
                }
            })
        })
        .unwrap_or(0)
}
