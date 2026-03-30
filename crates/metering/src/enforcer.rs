//! Quota enforcement — checks usage against plan limits.
//!
//! Three states:
//! - Allow: under all limits
//! - Warn: over 80% on any dimension (adds warning header)
//! - Deny: over 100% on any dimension (returns 429)

use crate::meter::AppMeter;
use crate::plan::QuotaPlan;
use serde::Serialize;
use std::sync::atomic::Ordering;

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
}

/// Check an app's current usage against its plan.
/// Call this BEFORE dispatching a request.
pub fn check_quota(meter: &AppMeter) -> QuotaDecision {
    let plan = &meter.plan;
    let mut warnings = Vec::new();

    // Check each dimension that has a limit
    if let Some(denial) = check_dimension(
        "requests",
        meter.requests.load(Ordering::Relaxed),
        plan.monthly_requests,
        &mut warnings,
    ) {
        return QuotaDecision::Deny(denial);
    }

    if let Some(denial) = check_dimension(
        "cpu_ms",
        meter.cpu_time_us.load(Ordering::Relaxed) / 1000, // us → ms
        plan.monthly_cpu_ms,
        &mut warnings,
    ) {
        return QuotaDecision::Deny(denial);
    }

    if let Some(denial) = check_dimension(
        "egress_bytes",
        meter.egress_bytes.load(Ordering::Relaxed),
        plan.monthly_egress_bytes,
        &mut warnings,
    ) {
        return QuotaDecision::Deny(denial);
    }

    if let Some(denial) = check_dimension(
        "db_reads",
        meter.db_reads.load(Ordering::Relaxed),
        plan.monthly_db_reads,
        &mut warnings,
    ) {
        return QuotaDecision::Deny(denial);
    }

    if let Some(denial) = check_dimension(
        "db_writes",
        meter.db_writes.load(Ordering::Relaxed),
        plan.monthly_db_writes,
        &mut warnings,
    ) {
        return QuotaDecision::Deny(denial);
    }

    if let Some(denial) = check_dimension(
        "kv_ops",
        meter.kv_ops.load(Ordering::Relaxed),
        plan.monthly_kv_ops,
        &mut warnings,
    ) {
        return QuotaDecision::Deny(denial);
    }

    if warnings.is_empty() {
        QuotaDecision::Allow
    } else {
        QuotaDecision::Warn(warnings)
    }
}

/// Check a single dimension. Returns Some(denial) if over limit,
/// or pushes a warning if over 80%.
fn check_dimension(
    name: &str,
    used: u64,
    limit: Option<u64>,
    warnings: &mut Vec<QuotaWarning>,
) -> Option<QuotaDenial> {
    let limit = match limit {
        Some(l) => l,
        None => return None, // No limit = always allow
    };

    if limit == 0 {
        return Some(QuotaDenial {
            dimension: name.into(),
            used,
            limit,
            message: format!("{name} is disabled on this plan"),
        });
    }

    let pct = (used as f64 / limit as f64) * 100.0;

    if used > limit {
        return Some(QuotaDenial {
            dimension: name.into(),
            used,
            limit,
            message: format!(
                "Monthly {name} quota exceeded ({used}/{limit})",
            ),
        });
    }

    if pct >= 80.0 {
        warnings.push(QuotaWarning {
            dimension: name.into(),
            used,
            limit,
            usage_pct: pct,
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::QuotaPlan;

    #[test]
    fn allow_under_limit() {
        let meter = AppMeter::new(QuotaPlan::free());
        let decision = check_quota(&meter);
        assert!(matches!(decision, QuotaDecision::Allow));
    }

    #[test]
    fn warn_at_80_percent() {
        let plan = QuotaPlan {
            monthly_requests: Some(100),
            ..QuotaPlan::unlimited()
        };
        let meter = AppMeter::new(plan);
        meter.requests.store(85, Ordering::Relaxed);
        let decision = check_quota(&meter);
        assert!(matches!(decision, QuotaDecision::Warn(_)));
    }

    #[test]
    fn deny_over_limit() {
        let plan = QuotaPlan {
            monthly_requests: Some(100),
            ..QuotaPlan::unlimited()
        };
        let meter = AppMeter::new(plan);
        meter.requests.store(101, Ordering::Relaxed);
        let decision = check_quota(&meter);
        assert!(matches!(decision, QuotaDecision::Deny(_)));
    }

    #[test]
    fn unlimited_always_allows() {
        let meter = AppMeter::new(QuotaPlan::unlimited());
        meter.requests.store(999_999_999, Ordering::Relaxed);
        let decision = check_quota(&meter);
        assert!(matches!(decision, QuotaDecision::Allow));
    }
}
