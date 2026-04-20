use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A registered application record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppRecord {
    pub id: Uuid,
    pub name: String,
    pub plan_id: String,
    pub deploy_hash: Option<String>,
    #[serde(skip_serializing)]
    pub api_key: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Worker-facing runtime limits for a specific app.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AppRuntimeLimits {
    pub cpu_limit_ms: Option<u64>,
    pub wall_timeout_ms: Option<u64>,
    /// Maximum V8 heap in megabytes. `None` → 128 MB default in the worker.
    /// Free-tier apps should be capped low (64 MB); paid tiers can go higher.
    pub heap_limit_mb: Option<u32>,
}

/// Worker-facing metadata for an app version/config snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppVersionInfo {
    pub deploy_hash: Option<String>,
    pub plan_id: String,
    pub runtime: AppRuntimeLimits,
}

/// A routing entry resolved from an incoming request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteEntry {
    pub name: String,
    pub plan_id: String,
    pub api_key_hash: String,
    pub deploy_hash: Option<String>,
    /// Whether the app exports a `default.fetch` handler. When true, the
    /// gateway proxies HTTP requests to the worker's unified `/dispatch/`
    /// endpoint so the JS handler receives a proper `Request` object and
    /// can return streaming responses (SSE). When false, the gateway only
    /// serves static assets.
    #[serde(default)]
    pub has_http_handler: bool,
}

/// Map of app id → current deploy/config snapshot.
pub type VersionMap = HashMap<Uuid, AppVersionInfo>;

/// Map of app id → route entry for fast lookup.
pub type RouteMap = HashMap<Uuid, RouteEntry>;

/// Usage counters reported by a worker to the control plane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageReport {
    pub worker_id: String,
    pub counters: HashMap<Uuid, AppUsage>,
}

/// Per-application usage counters for a billing interval.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppUsage {
    pub requests: u64,
    pub cpu_us: u64,
    pub wall_us: u64,
    pub egress_bytes: u64,
    pub ingress_bytes: u64,
}

/// Events emitted by the control plane to workers/gates.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ControlEvent {
    Deploy { app_id: Uuid, hash: String },
    Delete { app_id: Uuid },
    PlanChange { app_id: Uuid, plan_id: String },
}

/// Canonical errors for zeroship-common operations.
#[derive(Debug, thiserror::Error)]
pub enum CommonError {
    #[error("not found")]
    NotFound,
    #[error("already exists")]
    AlreadyExists,
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("internal error: {0}")]
    Internal(String),
}
