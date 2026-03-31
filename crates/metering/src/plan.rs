//! Quota plan definitions.
//!
//! A plan defines entitlements, quotas, and rate limits for an app.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Policy action to take when a usage threshold is reached.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PolicyAction {
    Allow,
    Warn,
    Notify,
    Throttle,
    Block,
    BlockWrites,
    Kill,
}

/// Policy definition — actions at specific usage thresholds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyDef {
    pub at_50_pct: Option<PolicyAction>,
    pub at_80_pct: Option<PolicyAction>,
    pub at_95_pct: Option<PolicyAction>,
    pub at_100_pct: Option<PolicyAction>,
}

impl PolicyDef {
    pub fn warn_then_block() -> Self {
        Self {
            at_50_pct: None,
            at_80_pct: Some(PolicyAction::Warn),
            at_95_pct: None,
            at_100_pct: Some(PolicyAction::Block),
        }
    }
    pub fn hard_kill() -> Self {
        Self {
            at_50_pct: None,
            at_80_pct: None,
            at_95_pct: None,
            at_100_pct: Some(PolicyAction::Kill),
        }
    }
    pub fn reject() -> Self {
        Self {
            at_50_pct: None,
            at_80_pct: None,
            at_95_pct: None,
            at_100_pct: Some(PolicyAction::Block),
        }
    }
    pub fn block_writes_only() -> Self {
        Self {
            at_50_pct: None,
            at_80_pct: Some(PolicyAction::Warn),
            at_95_pct: None,
            at_100_pct: Some(PolicyAction::BlockWrites),
        }
    }
}

/// Resolve a policy name string to a PolicyDef.
pub fn resolve_policy(name: &str) -> PolicyDef {
    match name {
        "warn_then_block" => PolicyDef::warn_then_block(),
        "hard_kill" => PolicyDef::hard_kill(),
        "reject" => PolicyDef::reject(),
        "block_writes_only" => PolicyDef::block_writes_only(),
        _ => PolicyDef::warn_then_block(), // default fallback
    }
}

/// Evaluate a policy at a given usage percentage, returning the action to take.
pub fn evaluate_policy(policy_name: &str, usage_pct: f64) -> PolicyAction {
    let policy = resolve_policy(policy_name);
    if usage_pct >= 100.0 {
        return policy.at_100_pct.unwrap_or(PolicyAction::Block);
    }
    if usage_pct >= 95.0 {
        return policy.at_95_pct.unwrap_or(PolicyAction::Allow);
    }
    if usage_pct >= 80.0 {
        return policy.at_80_pct.unwrap_or(PolicyAction::Allow);
    }
    if usage_pct >= 50.0 {
        return policy.at_50_pct.unwrap_or(PolicyAction::Allow);
    }
    PolicyAction::Allow
}

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
    /// Per-app spending limit in cents. None = no limit.
    pub spending_limit_cents: Option<u64>,
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
            spending_limit_cents: Some(500), // $5.00 spending cap for free tier
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
            spending_limit_cents: None, // no spending limit for pro
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
            spending_limit_cents: None,
        }
    }
}
