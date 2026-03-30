//! Quota plan definitions.
//!
//! A plan defines the limits for an app. `None` means unlimited.

use serde::{Deserialize, Serialize};

/// Quota plan — configurable limits per app.
/// `None` on any field means unlimited (no enforcement).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaPlan {
    pub name: String,

    // --- Per-request hard limits ---
    /// Max CPU time per request (ms). Exceeding kills the request.
    pub max_cpu_per_request_ms: Option<u64>,
    /// Max wall time per request (ms).
    pub max_wall_time_per_request_ms: Option<u64>,

    // --- Monthly quotas ---
    /// Max requests per billing period.
    pub monthly_requests: Option<u64>,
    /// Max CPU milliseconds per billing period.
    pub monthly_cpu_ms: Option<u64>,
    /// Max egress bytes per billing period.
    pub monthly_egress_bytes: Option<u64>,
    /// Max DB read operations per billing period.
    pub monthly_db_reads: Option<u64>,
    /// Max DB write operations per billing period.
    pub monthly_db_writes: Option<u64>,
    /// Max KV operations per billing period.
    pub monthly_kv_ops: Option<u64>,

    // --- Static resource limits ---
    /// Max DB storage in bytes.
    pub max_db_storage_bytes: Option<u64>,
    /// Max V8 heap memory in MB per isolate.
    pub max_memory_mb: Option<u64>,

    // --- Rate limits ---
    /// Max requests per second.
    pub rate_limit_rps: Option<u32>,
    /// Burst allowance (token bucket capacity).
    pub rate_limit_burst: Option<u32>,
}

impl QuotaPlan {
    /// Free tier — conservative limits.
    pub fn free() -> Self {
        Self {
            name: "free".into(),
            max_cpu_per_request_ms: Some(10),
            max_wall_time_per_request_ms: Some(30_000),
            monthly_requests: Some(100_000),
            monthly_cpu_ms: Some(10_000),
            monthly_egress_bytes: Some(1_000_000_000),       // 1GB
            monthly_db_reads: Some(500_000),
            monthly_db_writes: Some(50_000),
            monthly_kv_ops: Some(100_000),
            max_db_storage_bytes: Some(500_000_000),          // 500MB
            max_memory_mb: Some(128),
            rate_limit_rps: Some(10),
            rate_limit_burst: Some(50),
        }
    }

    /// Pro tier — generous limits.
    pub fn pro() -> Self {
        Self {
            name: "pro".into(),
            max_cpu_per_request_ms: Some(30_000),
            max_wall_time_per_request_ms: Some(300_000),
            monthly_requests: Some(10_000_000),
            monthly_cpu_ms: Some(30_000_000),
            monthly_egress_bytes: Some(100_000_000_000),      // 100GB
            monthly_db_reads: None,                            // unlimited
            monthly_db_writes: Some(5_000_000),
            monthly_kv_ops: Some(10_000_000),
            max_db_storage_bytes: Some(10_000_000_000),       // 10GB
            max_memory_mb: Some(128),
            rate_limit_rps: Some(1000),
            rate_limit_burst: Some(5000),
        }
    }

    /// Enterprise tier — effectively unlimited.
    pub fn enterprise() -> Self {
        Self {
            name: "enterprise".into(),
            max_cpu_per_request_ms: Some(300_000),
            max_wall_time_per_request_ms: Some(600_000),
            monthly_requests: None,
            monthly_cpu_ms: None,
            monthly_egress_bytes: None,
            monthly_db_reads: None,
            monthly_db_writes: None,
            monthly_kv_ops: None,
            max_db_storage_bytes: Some(100_000_000_000),      // 100GB
            max_memory_mb: Some(256),
            rate_limit_rps: None,
            rate_limit_burst: None,
        }
    }

    /// Development — no limits.
    pub fn unlimited() -> Self {
        Self {
            name: "unlimited".into(),
            max_cpu_per_request_ms: None,
            max_wall_time_per_request_ms: None,
            monthly_requests: None,
            monthly_cpu_ms: None,
            monthly_egress_bytes: None,
            monthly_db_reads: None,
            monthly_db_writes: None,
            monthly_kv_ops: None,
            max_db_storage_bytes: None,
            max_memory_mb: None,
            rate_limit_rps: None,
            rate_limit_burst: None,
        }
    }
}
