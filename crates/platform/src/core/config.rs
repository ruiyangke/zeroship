//! Configuration types for the zeroship platform.
//!
//! These structs define all configurable behavior of the runtime.
//! They can be constructed from:
//! - `zeroship.toml` config file
//! - CLI arguments
//! - The Builder API
//! - Environment variables

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

/// Top-level configuration for an zeroship instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppbaseConfig {
    /// HTTP server configuration.
    #[serde(default)]
    pub server: ServerConfig,

    /// V8 isolate pool configuration.
    #[serde(default)]
    pub isolates: IsolateConfig,

    /// Plugin-specific configuration, keyed by plugin name.
    /// e.g., { "db": { "path": "data/{app_id}.db" }, "kv": { "enabled": true } }
    #[serde(default)]
    pub plugins: HashMap<String, toml::Value>,
}

/// HTTP server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Port to listen on.
    #[serde(default = "default_port")]
    pub port: u16,

    /// Host to bind to.
    #[serde(default = "default_host")]
    pub host: String,
}

/// V8 isolate pool configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IsolateConfig {
    /// Maximum number of warm isolates kept alive.
    #[serde(default = "default_max_isolates")]
    pub max: usize,

    /// Evict idle isolates after this many seconds.
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,

    /// Maximum CPU time per request in milliseconds.
    /// None = unlimited (dev mode).
    #[serde(default = "default_cpu_limit")]
    pub cpu_limit_ms: Option<u64>,

    /// Maximum total CPU time per app in milliseconds.
    /// Apps exceeding this are evicted. None = unlimited.
    #[serde(default)]
    pub cpu_quota_ms: Option<u64>,

    /// Maximum total RSS (in MB) for the pool.
    /// When exceeded, least-recently-used isolates are evicted.
    /// None = no memory-based eviction.
    #[serde(default)]
    pub max_memory_mb: Option<usize>,
}

impl IsolateConfig {
    /// Convert `idle_timeout_secs` to a `Duration`.
    pub fn idle_timeout(&self) -> Duration {
        Duration::from_secs(self.idle_timeout_secs)
    }

    /// Maximum CPU time allowed per request.
    pub fn cpu_limit(&self) -> Option<Duration> {
        self.cpu_limit_ms.map(Duration::from_millis)
    }

    /// Maximum total CPU time allowed per app before eviction.
    pub fn cpu_quota(&self) -> Option<Duration> {
        self.cpu_quota_ms.map(Duration::from_millis)
    }
}

// --- Defaults ---

impl Default for AppbaseConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            isolates: IsolateConfig::default(),
            plugins: HashMap::new(),
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: default_port(),
            host: default_host(),
        }
    }
}

impl Default for IsolateConfig {
    fn default() -> Self {
        Self {
            max: default_max_isolates(),
            idle_timeout_secs: default_idle_timeout(),
            cpu_limit_ms: default_cpu_limit(),
            cpu_quota_ms: None,
            max_memory_mb: None,
        }
    }
}

fn default_port() -> u16 {
    3000
}
fn default_host() -> String {
    "0.0.0.0".into()
}
fn default_max_isolates() -> usize {
    1000
}
fn default_idle_timeout() -> u64 {
    60
}
fn default_cpu_limit() -> Option<u64> {
    Some(50) // 50ms, same as Cloudflare Workers default
}
