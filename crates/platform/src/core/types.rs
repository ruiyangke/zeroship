//! Core types used across all appbase crates.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Unique identifier for an app on the platform.
/// In single-app mode, this is "default".
/// In multi-tenant mode, derived from subdomain, path prefix, or header.
pub type AppId = String;

/// JSON-RPC 2.0 request (parsed from HTTP body).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
    pub id: Option<serde_json::Value>,
}

/// JSON-RPC 2.0 response (serialized to HTTP body).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
    pub id: Option<serde_json::Value>,
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

/// Result of executing an RPC call, including metering data.
#[derive(Debug)]
pub struct RpcResult {
    /// The JSON-RPC response body.
    pub json: String,
    /// CPU time consumed by V8 during this call (excludes I/O wait).
    pub cpu_time: Duration,
    /// Console output captured during execution.
    pub logs: Vec<String>,
}

/// Statistics for a single isolate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IsolateStats {
    pub app_id: AppId,
    pub total_cpu_ms: f64,
    pub request_count: u64,
    pub idle_secs: f64,
}

/// Statistics for the isolate pool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolStats {
    pub active_isolates: usize,
    pub max_isolates: usize,
    pub apps: Vec<IsolateStats>,
}

/// An app's compiled artifacts — ready to be loaded into an isolate.
#[derive(Debug, Clone)]
pub struct AppBundle {
    /// Compiled server-side JavaScript (loaded into V8).
    pub server_js: String,
    /// Compiled client-side HTML (served to browser).
    pub client_html: Option<Vec<u8>>,
}
