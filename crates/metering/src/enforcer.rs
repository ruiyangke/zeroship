//! Quota enforcement — checks usage against plan limits.
//!
//! Three states:
//! - Allow: under all limits
//! - Warn: over 80% on any dimension (adds warning header)
//! - Deny: over 100% on any dimension (returns 429)

use crate::meter::AppMeter;
use crate::plan::{QuotaPlan, Period};
use serde::Serialize;

/// Result of a quota check.
#[derive(Debug)]
pub enum QuotaDecision {
    /// Request allowed, no warnings.
    Allow,
    /// Request allowed, but approaching limit on one or more dimensions.
    Warn(Vec<QuotaWarning>),
    /// Request denied — a hard limit has been exceeded.
    Deny(QuotaDenial),
}

/// Warning: a dimension is above 80% usage.
#[derive(Debug, Clone, Serialize)]
pub struct QuotaWarning {
    pub dimension: String,
    pub used: u64,
    pub limit: u64,
    pub usage_pct: f64,
}

/// Denial: a dimension has exceeded 100% usage.
#[derive(Debug, Clone, Serialize)]
pub struct QuotaDenial {
    pub dimension: String,
    pub used: u64,
    pub limit: u64,
    pub message: String,
    pub error_code: i32,
}

/// Check an app's current usage against its plan.
/// Call this BEFORE dispatching a request.
pub fn check_quota(meter: &AppMeter, plan: &QuotaPlan) -> QuotaDecision {
    let mut warnings = Vec::new();

    for (resource, quota) in &plan.quotas {
        // Skip per-request quotas (enforced in isolate, not here)
        if matches!(quota.period, Period::PerRequest) {
            continue;
        }

        let max = match quota.max {
            Some(m) => m,
            None => continue, // unlimited
        };

        let used = meter.get_counter(resource);

        let pct = if max > 0 { (used as f64 / max as f64) * 100.0 } else { 100.0 };

        if used > max {
            return QuotaDecision::Deny(QuotaDenial {
                dimension: resource.clone(),
                used,
                limit: max,
                message: format!("Monthly {resource} quota exceeded ({used}/{max})"),
                error_code: -32029,
            });
        }

        if pct >= 80.0 {
            warnings.push(QuotaWarning {
                dimension: resource.clone(),
                used,
                limit: max,
                usage_pct: pct,
            });
        }
    }

    if warnings.is_empty() {
        QuotaDecision::Allow
    } else {
        QuotaDecision::Warn(warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{QuotaPlan, QuotaDef, Period};
    use std::sync::atomic::Ordering;

    #[test]
    fn allow_under_limit() {
        let plan = QuotaPlan::free();
        let meter = AppMeter::new(plan.clone());
        let decision = check_quota(&meter, &plan);
        assert!(matches!(decision, QuotaDecision::Allow));
    }

    #[test]
    fn warn_at_80_percent() {
        let mut plan = QuotaPlan::unlimited();
        plan.quotas.insert("requests".into(), QuotaDef {
            max: Some(100),
            period: Period::Monthly,
            policy: "warn_then_block".into(),
        });
        let meter = AppMeter::new(plan.clone());
        meter.requests.store(85, Ordering::Relaxed);
        let decision = check_quota(&meter, &plan);
        assert!(matches!(decision, QuotaDecision::Warn(_)));
    }

    #[test]
    fn deny_over_limit() {
        let mut plan = QuotaPlan::unlimited();
        plan.quotas.insert("requests".into(), QuotaDef {
            max: Some(100),
            period: Period::Monthly,
            policy: "warn_then_block".into(),
        });
        let meter = AppMeter::new(plan.clone());
        meter.requests.store(101, Ordering::Relaxed);
        let decision = check_quota(&meter, &plan);
        assert!(matches!(decision, QuotaDecision::Deny(_)));
    }

    #[test]
    fn unlimited_always_allows() {
        let plan = QuotaPlan::unlimited();
        let meter = AppMeter::new(plan.clone());
        meter.requests.store(999_999_999, Ordering::Relaxed);
        let decision = check_quota(&meter, &plan);
        assert!(matches!(decision, QuotaDecision::Allow));
    }
}
