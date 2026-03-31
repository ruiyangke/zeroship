//! Quota enforcement — checks usage against plan limits.
//!
//! Three states:
//! - Allow: under all limits
//! - Warn: over 80% on any dimension (adds warning header)
//! - Deny: over 100% on any dimension (returns 429)

use crate::error_codes;
use appbase_plan::{evaluate_policy, PolicyAction, QuotaPlan, Period};
use serde::Serialize;
use std::collections::HashMap;

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

/// Denial: a required entitlement is not available.
#[derive(Debug, Clone, Serialize)]
pub struct EntitlementDenial {
    pub feature: String,
    pub message: String,
    pub error_code: i32,
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
/// Takes a usage snapshot (resource name → accumulated value) and the plan.
/// Pure function — no dependency on metering internals.
pub fn check_quota(usage: &HashMap<String, u64>, plan: &QuotaPlan) -> QuotaDecision {
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

        let used = usage.get(resource.as_str()).copied().unwrap_or(0);

        let pct = if max > 0 { (used as f64 / max as f64) * 100.0 } else { 100.0 };

        let action = evaluate_policy(&quota.policy, pct);

        match action {
            PolicyAction::Block | PolicyAction::Kill => {
                return QuotaDecision::Deny(QuotaDenial {
                    dimension: resource.clone(),
                    used,
                    limit: max,
                    message: format!("Monthly {resource} quota exceeded ({used}/{max})"),
                    error_code: error_codes::QUOTA_EXCEEDED,
                });
            }
            PolicyAction::BlockWrites => {
                return QuotaDecision::Deny(QuotaDenial {
                    dimension: resource.clone(),
                    used,
                    limit: max,
                    message: format!("{resource} quota exceeded — writes blocked ({used}/{max})"),
                    error_code: error_codes::QUOTA_EXCEEDED,
                });
            }
            PolicyAction::Warn | PolicyAction::Notify => {
                warnings.push(QuotaWarning {
                    dimension: resource.clone(),
                    used,
                    limit: max,
                    usage_pct: pct,
                });
            }
            PolicyAction::Throttle => {
                warnings.push(QuotaWarning {
                    dimension: resource.clone(),
                    used,
                    limit: max,
                    usage_pct: pct,
                });
            }
            PolicyAction::Allow => {}
        }
    }

    if warnings.is_empty() {
        QuotaDecision::Allow
    } else {
        QuotaDecision::Warn(warnings)
    }
}

/// Check if an app has a specific entitlement (boolean feature gate).
/// Returns Ok(()) if the entitlement is granted, Err with denial details if not.
pub fn check_entitlement(plan: &QuotaPlan, feature: &str) -> Result<(), EntitlementDenial> {
    match plan.entitlements.get(feature) {
        Some(val) => {
            if val.as_bool().unwrap_or(false) {
                Ok(())
            } else {
                Err(EntitlementDenial {
                    feature: feature.to_string(),
                    message: format!("Feature '{feature}' is not enabled on the {plan_name} plan", plan_name = plan.name),
                    error_code: error_codes::ENTITLEMENT_DENIED,
                })
            }
        }
        None => Err(EntitlementDenial {
            feature: feature.to_string(),
            message: format!("Feature '{feature}' is not available on the {plan_name} plan", plan_name = plan.name),
            error_code: error_codes::ENTITLEMENT_DENIED,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use appbase_plan::{QuotaPlan, QuotaDef, Period};

    fn usage(pairs: &[(&str, u64)]) -> HashMap<String, u64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn allow_under_limit() {
        let plan = QuotaPlan::free();
        let decision = check_quota(&HashMap::new(), &plan);
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
        let decision = check_quota(&usage(&[("requests", 85)]), &plan);
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
        let decision = check_quota(&usage(&[("requests", 101)]), &plan);
        assert!(matches!(decision, QuotaDecision::Deny(_)));
    }

    #[test]
    fn unlimited_always_allows() {
        let plan = QuotaPlan::unlimited();
        let decision = check_quota(&usage(&[("requests", 999_999_999)]), &plan);
        assert!(matches!(decision, QuotaDecision::Allow));
    }

    #[test]
    fn entitlement_granted() {
        let plan = QuotaPlan::pro();
        assert!(check_entitlement(&plan, "custom_domains").is_ok());
    }

    #[test]
    fn entitlement_denied() {
        let plan = QuotaPlan::free();
        let result = check_entitlement(&plan, "custom_domains");
        assert!(result.is_err());
        let denial = result.unwrap_err();
        assert_eq!(denial.feature, "custom_domains");
        assert_eq!(denial.error_code, error_codes::ENTITLEMENT_DENIED);
    }

    #[test]
    fn entitlement_missing_is_denied() {
        let plan = QuotaPlan::free();
        let result = check_entitlement(&plan, "nonexistent_feature");
        assert!(result.is_err());
    }
}
