//! Quota plan definitions.
//!
//! A plan defines entitlements, quotas, and rate limits for an app.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Time period for quota enforcement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Period {
    PerRequest,
    Daily,
    Monthly,
    Absolute,
}

/// A single quota definition — max usage within a period.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaDef {
    /// Maximum allowed value. `None` means unlimited.
    pub max: Option<u64>,
    /// Time period for the quota.
    pub period: Period,
    /// Policy name for enforcement (e.g. "warn_then_block", "hard_kill").
    pub policy: String,
}

/// A rate limit definition — requests per second with burst.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitDef {
    pub max_per_second: u32,
    pub burst: u32,
    pub policy: String,
}

/// Quota plan — configurable limits per app.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaPlan {
    pub name: String,
    pub version: u32,
    pub description: String,
    pub entitlements: HashMap<String, serde_json::Value>,
    pub quotas: HashMap<String, QuotaDef>,
    pub rate_limits: HashMap<String, RateLimitDef>,
}

impl QuotaPlan {
    /// Free tier — conservative limits.
    pub fn free() -> Self {
        let mut quotas = HashMap::new();
        quotas.insert("cpu_ms".into(), QuotaDef { max: Some(50_000), period: Period::Monthly, policy: "warn_then_block".into() });
        quotas.insert("requests".into(), QuotaDef { max: Some(100_000), period: Period::Monthly, policy: "warn_then_block".into() });
        quotas.insert("egress_bytes".into(), QuotaDef { max: Some(1_000_000_000), period: Period::Monthly, policy: "warn_then_block".into() });
        quotas.insert("db_reads".into(), QuotaDef { max: Some(500_000), period: Period::Monthly, policy: "warn_then_block".into() });
        quotas.insert("db_writes".into(), QuotaDef { max: Some(50_000), period: Period::Monthly, policy: "warn_then_block".into() });
        quotas.insert("kv_ops".into(), QuotaDef { max: Some(100_000), period: Period::Monthly, policy: "warn_then_block".into() });
        quotas.insert("db_storage_bytes".into(), QuotaDef { max: Some(500_000_000), period: Period::Absolute, policy: "block_writes_only".into() });
        quotas.insert("cpu_per_request".into(), QuotaDef { max: Some(10), period: Period::PerRequest, policy: "hard_kill".into() });

        let mut rate_limits = HashMap::new();
        rate_limits.insert("default".into(), RateLimitDef { max_per_second: 10, burst: 50, policy: "reject".into() });

        Self {
            name: "free".into(),
            version: 1,
            description: "Free tier".into(),
            entitlements: HashMap::new(),
            quotas,
            rate_limits,
        }
    }

    /// Pro tier — generous limits.
    pub fn pro() -> Self {
        let mut quotas = HashMap::new();
        quotas.insert("cpu_ms".into(), QuotaDef { max: Some(30_000_000), period: Period::Monthly, policy: "warn_then_block".into() });
        quotas.insert("requests".into(), QuotaDef { max: Some(10_000_000), period: Period::Monthly, policy: "warn_then_block".into() });
        quotas.insert("cpu_per_request".into(), QuotaDef { max: Some(30_000), period: Period::PerRequest, policy: "hard_kill".into() });

        let mut rate_limits = HashMap::new();
        rate_limits.insert("default".into(), RateLimitDef { max_per_second: 1000, burst: 5000, policy: "reject".into() });

        Self {
            name: "pro".into(),
            version: 1,
            description: "Pro tier".into(),
            entitlements: HashMap::new(),
            quotas,
            rate_limits,
        }
    }

    /// Development — no limits.
    pub fn unlimited() -> Self {
        Self {
            name: "unlimited".into(),
            version: 1,
            description: "No limits".into(),
            entitlements: HashMap::new(),
            quotas: HashMap::new(),
            rate_limits: HashMap::new(),
        }
    }
}
