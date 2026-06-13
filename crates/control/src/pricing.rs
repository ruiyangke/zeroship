//! Pure tier math for the configurable pricing catalog (billing PR4).
//!
//! Ported from the dead `crates/platform/src/billing/pricing.rs`, converting
//! **millicents → cents** (D3): the catalog and reconciler standardize on
//! cents (Stripe's unit). All arithmetic is integer with a `u128` intermediate
//! and **half-up rounding at the line-item boundary**, so a fractional cent
//! never silently truncates downward.
//!
//! This module is DB-free and unit-pure: the plan catalog ([`crate::plan_catalog`])
//! deserializes the JSONB plan columns into these types, and the reconciler
//! (PR6) consumes [`ChargeBreakdown::lines`] to build Stripe invoice items.
//!
//! Charge model (FINALIZED DESIGN, locked 2026-06-13):
//!
//! ```text
//! charge = base_fee + Σ max(0, usage[m] − included[m]) × overage_rate[m]
//! ```
//!
//! Usage past the included quota is pay-as-you-go overage; usage within the
//! quota is free (only the base fee applies).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A single overage pricing rule for one metric. Charges apply only to the
/// **billable** units (usage past the metric's included quota — the caller
/// computes `max(0, usage − included)` before invoking [`rule_cost`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PricingRule {
    /// Flat rate: `rate_cents` per `per_units` units consumed.
    Flat {
        rate_cents: u64,
        per_units: u64,
    },
    /// Tiered (graduated) pricing.
    Tiered {
        tiers: Vec<PricingTier>,
    },
}

/// One tier in graduated pricing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PricingTier {
    /// Usage up to this amount uses this rate. `None` = unlimited (final tier).
    pub up_to: Option<u64>,
    /// Rate in cents per `per_units`.
    pub rate_cents: u64,
    /// Number of units the rate applies to.
    pub per_units: u64,
}

/// The fully-resolved price model for one plan tier.
///
/// `base_fee_cents` is charged unconditionally each period. For each metric in
/// `overage`, usage past `included[metric]` (default 0) is charged at that
/// metric's rule. `spend_limit_default_cents` is the cap a new app inherits
/// from the plan (the creator may override it per-app in PR5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlanPrice {
    pub base_fee_cents: u64,
    /// Included quota per metric (units). A metric absent from this map has
    /// an included quota of 0 ⇒ all usage is billable overage.
    #[serde(default)]
    pub included: HashMap<String, u64>,
    /// Overage rule per metric. A metric absent from this map is FREE (no
    /// overage charge, regardless of usage) — matching the platform port's
    /// "resources without pricing rules are free, not an error".
    #[serde(default)]
    pub overage: HashMap<String, PricingRule>,
    pub spend_limit_default_cents: u64,
}

/// One line item in a charge breakdown — the reconciler maps each to a Stripe
/// invoice item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineItem {
    pub metric: String,
    /// Units past the included quota that were billed (`max(0, usage −
    /// included)`).
    pub billable_units: u64,
    /// Cents charged for this line (half-up rounded).
    pub cents: u64,
}

/// The full per-period charge: the base fee, the per-metric overage lines, and
/// the total. `total_cents == base_cents + Σ lines.cents` by construction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChargeBreakdown {
    pub base_cents: u64,
    pub lines: Vec<LineItem>,
    pub total_cents: u64,
}

/// Compute the cents charged for `billable_units` of a metric under `rule`.
///
/// `u128` intermediate; **half-up rounding** at the boundary (`(a*r + d/2) / d`).
/// A `per_units == 0` rule is treated as free (avoids divide-by-zero), matching
/// the platform port.
#[must_use]
pub fn rule_cost(rule: &PricingRule, billable_units: u64) -> u64 {
    match rule {
        PricingRule::Flat { rate_cents, per_units } => {
            div_round_half_up(
                u128::from(billable_units) * u128::from(*rate_cents),
                u128::from(*per_units),
            )
        }
        PricingRule::Tiered { tiers } => {
            let mut remaining = billable_units;
            let mut cost: u128 = 0;
            let mut prev_boundary: u64 = 0;
            for tier in tiers {
                if remaining == 0 {
                    break;
                }
                let tier_capacity = match tier.up_to {
                    Some(up_to) => up_to.saturating_sub(prev_boundary),
                    None => remaining, // final tier covers the rest
                };
                let consumed = remaining.min(tier_capacity);
                cost += tiered_line_cost(consumed, tier.rate_cents, tier.per_units);
                remaining -= consumed;
                if let Some(up_to) = tier.up_to {
                    prev_boundary = up_to;
                }
            }
            // Round the accumulated (still-unrounded) tiered total half-up once
            // at the boundary. `tiered_line_cost` returns the scaled numerator
            // pre-division so graduated tiers compose without per-tier rounding
            // drift.
            u64::try_from(cost).unwrap_or(u64::MAX)
        }
    }
}

/// Numerator contribution of one tier (already divided per-tier with half-up
/// rounding). Kept separate so a `per_units == 0` tier contributes 0.
fn tiered_line_cost(consumed: u64, rate_cents: u64, per_units: u64) -> u128 {
    if per_units == 0 {
        return 0;
    }
    u128::from(div_round_half_up(
        u128::from(consumed) * u128::from(rate_cents),
        u128::from(per_units),
    ))
}

/// `round(numer / denom)` half-up, in `u128`, saturating into `u64`. A zero
/// denominator yields 0 (free rule).
fn div_round_half_up(numer: u128, denom: u128) -> u64 {
    if denom == 0 {
        return 0;
    }
    let rounded = (numer + denom / 2) / denom;
    u64::try_from(rounded).unwrap_or(u64::MAX)
}

/// Compute the full period charge for a plan price against a usage map.
///
/// `usage` is `metric → total` (the `i64` totals the metering aggregator
/// returns; negative or zero totals contribute no billable units). The result
/// breaks down into the base fee plus one [`LineItem`] per metric that has BOTH
/// an overage rule AND billable usage past its included quota. Lines are sorted
/// by metric name so the output (and the reconciler's invoice items) is
/// deterministic.
#[must_use]
pub fn charge_cents(price: &PlanPrice, usage: &HashMap<String, i64>) -> ChargeBreakdown {
    let mut lines: Vec<LineItem> = Vec::new();
    for (metric, rule) in &price.overage {
        let used = usage.get(metric).copied().unwrap_or(0).max(0) as u64;
        let included = price.included.get(metric).copied().unwrap_or(0);
        let billable_units = used.saturating_sub(included);
        if billable_units == 0 {
            continue; // within quota ⇒ no overage line
        }
        let cents = rule_cost(rule, billable_units);
        if cents == 0 {
            continue; // a free/zero-rate rule produces no line
        }
        lines.push(LineItem { metric: metric.clone(), billable_units, cents });
    }
    lines.sort_by(|a, b| a.metric.cmp(&b.metric));
    let lines_total: u64 = lines.iter().map(|l| l.cents).sum();
    let total_cents = price.base_fee_cents.saturating_add(lines_total);
    ChargeBreakdown {
        base_cents: price.base_fee_cents,
        lines,
        total_cents,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(rate_cents: u64, per_units: u64) -> PricingRule {
        PricingRule::Flat { rate_cents, per_units }
    }

    // -- ported flat/tiered/multi-dimensional tests (millicents → cents) ----

    #[test]
    fn flat_pricing_overage() {
        // Ported `flat_pricing`: $0.30 / million requests, expressed in CENTS
        // (30 cents / 1M). 3.5M billable units → 3.5M * 30 / 1M = 105 cents.
        let mut price = PlanPrice::default();
        price.overage.insert("requests".to_string(), flat(30, 1_000_000));
        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), 3_500_000);
        let b = charge_cents(&price, &usage);
        assert_eq!(b.lines.len(), 1);
        assert_eq!(b.lines[0].cents, 105);
        assert_eq!(b.total_cents, 105);
    }

    #[test]
    fn multi_dimensional_overage() {
        // Ported `multi_dimensional` in cents. requests: 30c/1M; cpu_us:
        // 1250c/1B; egress_bytes: 9c/1B. No included quota ⇒ all usage billable.
        let mut price = PlanPrice::default();
        price.overage.insert("requests".to_string(), flat(30, 1_000_000));
        price.overage.insert("cpu_us".to_string(), flat(1250, 1_000_000_000));
        price.overage.insert("egress_bytes".to_string(), flat(9, 1_000_000_000));
        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), 3_500_000); // 105
        usage.insert("cpu_us".to_string(), 17_500_000_000); // 21875
        usage.insert("egress_bytes".to_string(), 7_000_000_000); // 63
        let b = charge_cents(&price, &usage);
        assert_eq!(b.total_cents, 105 + 21875 + 63);
        // breakdown sums to total
        let sum: u64 = b.lines.iter().map(|l| l.cents).sum();
        assert_eq!(sum + b.base_cents, b.total_cents);
    }

    #[test]
    fn tiered_pricing_overage() {
        // Ported `tiered_pricing` in cents: first 1M free, next 9M at 30c/1M,
        // remainder at 20c/1M. 15M billable units.
        let mut price = PlanPrice::default();
        price.overage.insert(
            "requests".to_string(),
            PricingRule::Tiered {
                tiers: vec![
                    PricingTier { up_to: Some(1_000_000), rate_cents: 0, per_units: 1_000_000 },
                    PricingTier { up_to: Some(10_000_000), rate_cents: 30, per_units: 1_000_000 },
                    PricingTier { up_to: None, rate_cents: 20, per_units: 1_000_000 },
                ],
            },
        );
        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), 15_000_000);
        let b = charge_cents(&price, &usage);
        // Tier1: 1M free=0; Tier2: 9M*30/1M=270; Tier3: 5M*20/1M=100 → 370.
        assert_eq!(b.lines[0].cents, 370);
        assert_eq!(b.total_cents, 370);
    }

    #[test]
    fn unknown_metric_has_no_overage_rule_is_free() {
        // Ported `unknown_resource_is_free`: a metric with no overage rule is
        // free regardless of usage.
        let price = PlanPrice::default();
        let mut usage = HashMap::new();
        usage.insert("unknown".to_string(), 999_999);
        let b = charge_cents(&price, &usage);
        assert!(b.lines.is_empty());
        assert_eq!(b.total_cents, 0);
    }

    #[test]
    fn zero_usage_yields_only_base_fee() {
        // Ported `zero_usage_zero_cost`, now with a base fee: empty usage ⇒
        // base only.
        let mut price = PlanPrice::default();
        price.base_fee_cents = 500;
        price.overage.insert("requests".to_string(), flat(30, 1_000_000));
        let b = charge_cents(&price, &HashMap::new());
        assert!(b.lines.is_empty());
        assert_eq!(b.base_cents, 500);
        assert_eq!(b.total_cents, 500);
    }

    #[test]
    fn rule_cost_rounds_half_up() {
        // 1 unit at 1 cent per 2 units = 0.5 cents → rounds UP to 1 (half-up).
        // The platform port truncated DOWN to 0; the cents conversion uses
        // half-up at the line boundary (D3).
        assert_eq!(rule_cost(&flat(1, 2), 1), 1);
        // 1 unit at 1 cent per 3 units = 0.333 → rounds to 0.
        assert_eq!(rule_cost(&flat(1, 3), 1), 0);
        // 2 units at 1 cent per 3 units = 0.667 → rounds to 1.
        assert_eq!(rule_cost(&flat(1, 3), 2), 1);
    }

    // -- NEW regression tests (blueprint PR4 (d)) ----------------------------

    #[test]
    fn overage_only_charges_above_included() {
        // REGRESSION (catches a port bug that prices from ZERO instead of from
        // the included quota). Plan includes 1M requests free; usage is 1.5M.
        // Only the 0.5M OVER the quota is billable: 500_000 * 30 / 1M = 15c.
        // A from-zero bug would bill 1.5M * 30 / 1M = 45c.
        let mut price = PlanPrice::default();
        price.included.insert("requests".to_string(), 1_000_000);
        price.overage.insert("requests".to_string(), flat(30, 1_000_000));
        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), 1_500_000);
        let b = charge_cents(&price, &usage);
        assert_eq!(b.lines.len(), 1, "exactly one overage line");
        assert_eq!(b.lines[0].billable_units, 500_000, "only the over-quota units");
        assert_eq!(b.lines[0].cents, 15, "15c, NOT 45c (must not price from zero)");
        assert_eq!(b.total_cents, 15);
    }

    #[test]
    fn included_quota_fully_covers_usage_yields_base_only() {
        // Usage at OR below the included quota produces no overage line — only
        // the base fee is charged.
        let mut price = PlanPrice::default();
        price.base_fee_cents = 900;
        price.included.insert("requests".to_string(), 1_000_000);
        price.included.insert("cpu_us".to_string(), 5_000_000_000);
        price.overage.insert("requests".to_string(), flat(30, 1_000_000));
        price.overage.insert("cpu_us".to_string(), flat(1250, 1_000_000_000));
        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), 1_000_000); // exactly the quota
        usage.insert("cpu_us".to_string(), 4_000_000_000); // under the quota
        let b = charge_cents(&price, &usage);
        assert!(b.lines.is_empty(), "no overage when usage ≤ included");
        assert_eq!(b.base_cents, 900);
        assert_eq!(b.total_cents, 900);
    }

    #[test]
    fn breakdown_total_equals_base_plus_lines() {
        // Invariant: total_cents == base_cents + Σ lines.cents.
        let mut price = PlanPrice::default();
        price.base_fee_cents = 1200;
        price.included.insert("requests".to_string(), 100);
        price.overage.insert("requests".to_string(), flat(5, 1));
        price.overage.insert("egress_bytes".to_string(), flat(9, 1_000_000_000));
        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), 300); // 200 billable * 5 = 1000
        usage.insert("egress_bytes".to_string(), 2_000_000_000); // 18
        let b = charge_cents(&price, &usage);
        let sum: u64 = b.lines.iter().map(|l| l.cents).sum();
        assert_eq!(b.total_cents, b.base_cents + sum);
        assert_eq!(b.total_cents, 1200 + 1000 + 18);
        // Lines are deterministically sorted by metric name.
        assert_eq!(b.lines[0].metric, "egress_bytes");
        assert_eq!(b.lines[1].metric, "requests");
    }

    #[test]
    fn negative_total_contributes_no_units() {
        // A negative aggregate total (shouldn't happen, but be defensive) maps
        // to 0 billable units, not a wrapping huge value.
        let mut price = PlanPrice::default();
        price.overage.insert("requests".to_string(), flat(30, 1_000_000));
        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), -5);
        let b = charge_cents(&price, &usage);
        assert!(b.lines.is_empty());
        assert_eq!(b.total_cents, 0);
    }
}
