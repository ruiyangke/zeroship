//! Compute-unit (CU) pricing — the cost-model / price decoupling (billing-v2
//! Refactor B).
//!
//! The cost model (how much compute a metric op "costs" in CU) is a **global**
//! fleet-wide table ([`MetricWeights`], persisted in `zeroship.metric_weights`);
//! the price lever (how many cents one CU sells for — the **FX**) is per-plan
//! (`PlanPrice::fx`, defaulting from the global `zeroship.pricing_config`). This
//! separates "engineering cost" from "business price" and — critically — lets us
//! accumulate integer `compute_units` across every metric and convert to cents
//! **exactly once**, killing the per-metric-line rounding-error class the old
//! per-metric overage model carried.
//!
//! Charge model (billing-v2, locked 2026-06-13):
//!
//! ```text
//! total_units    = Σ_m  floor( max(0, usage[m]) × units_per_op[m] / per_units[m] )   (integer CU)
//! billable_units = max(0, total_units − included_units)
//! total_cents    = base_fee_cents + round_half_up_ONCE( billable_units × fx )
//! ```
//!
//! - The unit is **`compute_units` / CU** — an integer, deliberately NOT named
//!   "token" (that collides with PAT/JWT/`token_id`/`token_handlers.rs`).
//! - A metric absent from the weight table contributes **0 units** (free) —
//!   preserving the live "unknown metric is free, not an error" semantics.
//! - FX is stored as an integer **pico-cents per CU** (`fx_pico_cents_per_unit`,
//!   10⁻¹² cent) so a sub-cent unit price is representable without floats; the
//!   single `× fx ÷ 10¹²` conversion rounds half-up once at the boundary.
//!
//! ## Overflow envelope (documented per the blueprint)
//!
//! Per-metric CU accumulation: `usage[m] (i64, ≤ ~9.2e18) × units_per_op (u64)`
//! is done in `u128` (max ~3.4e38) then divided by `per_units` (≥ 1) — a single
//! metric cannot overflow `u128`, and the summed `total_units` is clamped into
//! `u64` (saturating) before pricing. The cents conversion `billable_units (u64,
//! ≤ ~1.8e19) × fx_pico (u64, ≤ ~1.8e19)` is a `u128` product (≤ ~3.4e38, within
//! `u128::MAX ≈ 3.4e38`) divided by `10¹²`, rounded half-up, then saturated into
//! `u64` cents. Realistic magnitudes (billable ≤ ~1e12 CU, fx ≤ ~1e9 pico-cents)
//! sit ~17 orders of magnitude below the `u128` ceiling; the saturating clamps
//! make even adversarial inputs total-correct (no wrap), logged when they fire.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// The fixed integer scale for the FX (cents-per-CU) lever: FX is stored as
/// **pico-cents per CU** (10⁻¹² cent). `cents = round_half_up(billable_units ×
/// fx_pico_cents_per_unit / FX_SCALE)`, computed once.
pub const FX_SCALE: u128 = 1_000_000_000_000; // 10^12

/// One metric's global cost weight: `units_per_op` CU accrue per `per_units`
/// operations of this metric, so a sub-unit weight is exact (e.g. 1 CU per 1000
/// `egress_bytes` ⇒ `units_per_op = 1, per_units = 1000`). `per_units` must be
/// `> 0`; a `per_units == 0` weight is treated as free (contributes 0 CU),
/// matching the "unweighted metric is free" rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricWeight {
    pub units_per_op: u64,
    pub per_units: u64,
}

/// The GLOBAL cost model: `metric → MetricWeight`. Loaded once per spend/reconcile
/// sweep from `zeroship.metric_weights`. A metric absent here contributes 0 CU.
pub type MetricWeights = HashMap<String, MetricWeight>;

/// The fully-resolved price model for one plan tier under CU pricing.
///
/// `base_fee_cents` is charged unconditionally each period. `included_units` CU
/// are free; CU beyond that are billed at `fx` (pico-cents per CU). `fx == None`
/// ⇒ fall back to the global `pricing_config` default FX (resolved by the
/// catalog before pricing). `spend_limit_default_cents` is the cap a new app
/// inherits from the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlanPrice {
    pub base_fee_cents: u64,
    /// CU included before overage.
    pub included_units: u64,
    /// FX as pico-cents per CU; `None` ⇒ use the global default
    /// (`pricing_config.fx_pico_cents_per_unit`). Always resolved to a concrete
    /// value before [`charge_cents`] is called (the catalog substitutes the
    /// default), but kept optional in the type so a plan can simply inherit.
    #[serde(default)]
    pub fx_pico_cents_per_unit: Option<u64>,
    pub spend_limit_default_cents: u64,
}

impl PlanPrice {
    /// Semantic validation of a catalog price model, run at the write boundary
    /// (the `PUT /api/plans/:id` handler) so a malformed price is a 400, not a
    /// silently-wrong charge at billing time.
    ///
    /// Under CU pricing the price model is scalar, so the only structural
    /// constraint is the FX: if set, it must be `> 0` (a `Some(0)` FX would price
    /// all usage to base-only, which is almost certainly an operator mistake —
    /// to make a tier free, leave `included_units` high or `fx = 0` is rejected
    /// so the intent is explicit via the global default / weights, not a silent
    /// zero). `None` (inherit the global default) is always valid.
    ///
    /// # Errors
    /// Returns a human-readable message when the FX is explicitly zero.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(0) = self.fx_pico_cents_per_unit {
            return Err(
                "fx_pico_cents_per_unit must be > 0 when set (omit it to inherit the global \
                 default; a zero FX prices all usage to base-only)"
                    .to_string(),
            );
        }
        Ok(())
    }

    /// Resolve the effective FX (pico-cents per CU): the plan's own `fx` if set,
    /// else the global default. Call before [`charge_cents`] so an inheriting
    /// plan (`fx == None`) prices at the global rate, not base-only.
    #[must_use]
    pub fn with_effective_fx(&self, default_fx_pico_cents_per_unit: Option<u64>) -> Self {
        let mut p = self.clone();
        if p.fx_pico_cents_per_unit.is_none() {
            p.fx_pico_cents_per_unit = default_fx_pico_cents_per_unit;
        }
        p
    }
}

/// The full per-period charge under CU pricing. `total_cents` is what both call
/// sites read; `total_units`/`billable_units` are exposed for transparency
/// (audit, dashboards, invoice description).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChargeBreakdown {
    pub base_cents: u64,
    /// Σ over metrics of `floor(usage × units_per_op / per_units)` (audit).
    pub total_units: u64,
    /// `max(0, total_units − included_units)`.
    pub billable_units: u64,
    pub total_cents: u64,
}

/// Accumulate total compute units for a usage map under the global weight table.
///
/// `total_units = Σ_m floor( max(0, usage[m]) × units_per_op[m] / per_units[m] )`,
/// integer throughout (`u128` intermediate, floored per metric, saturating-summed
/// into `u64`). A metric with no weight — or a `per_units == 0` weight —
/// contributes 0.
#[must_use]
pub fn total_units(weights: &MetricWeights, usage: &HashMap<String, i64>) -> u64 {
    let mut acc: u64 = 0;
    for (metric, &raw) in usage {
        let Some(w) = weights.get(metric) else {
            continue; // unweighted ⇒ free
        };
        if w.per_units == 0 {
            continue; // degenerate weight ⇒ free (no divide-by-zero)
        }
        let used = u128::from(raw.max(0) as u64);
        // floor(used × units_per_op / per_units), exact integer.
        let metric_units = used * u128::from(w.units_per_op) / u128::from(w.per_units);
        let metric_units = u64::try_from(metric_units).unwrap_or(u64::MAX);
        acc = acc.saturating_add(metric_units);
    }
    acc
}

/// Compute the full period charge for a plan price against a usage map and the
/// global weight table.
///
/// Accumulates integer CU across every metric ([`total_units`]), subtracts the
/// plan's `included_units`, and converts the billable CU to cents **exactly
/// once** via the FX lever (`× fx_pico ÷ FX_SCALE`, half-up). `fx == None` is
/// treated as 0 here — the catalog is responsible for substituting the global
/// default before calling (an unresolved FX prices to base-only, never panics).
#[must_use]
pub fn charge_cents(
    price: &PlanPrice,
    usage: &HashMap<String, i64>,
    weights: &MetricWeights,
) -> ChargeBreakdown {
    let total = total_units(weights, usage);
    let billable = total.saturating_sub(price.included_units);
    let fx_pico = price.fx_pico_cents_per_unit.unwrap_or(0);
    let overage_cents = div_round_half_up(u128::from(billable) * u128::from(fx_pico), FX_SCALE);
    let total_cents = price.base_fee_cents.saturating_add(overage_cents);
    ChargeBreakdown {
        base_cents: price.base_fee_cents,
        total_units: total,
        billable_units: billable,
        total_cents,
    }
}

/// `round(numer / denom)` half-up, in `u128`, saturating into `u64`. A zero
/// denominator yields 0.
fn div_round_half_up(numer: u128, denom: u128) -> u64 {
    if denom == 0 {
        return 0;
    }
    let rounded = (numer + denom / 2) / denom;
    u64::try_from(rounded).unwrap_or_else(|_| {
        // A charge that overflows u64 cents is absurd (≈$1.8e17); clamp but log
        // it so a runaway weight/FX/usage total is visible, not silent.
        tracing::warn!(
            rounded_cents = %rounded,
            "pricing: charge saturated u64::MAX cents — clamping (check weights / fx / usage)"
        );
        u64::MAX
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(units_per_op: u64, per_units: u64) -> MetricWeight {
        MetricWeight { units_per_op, per_units }
    }

    /// A representative global weight table: 1 CU/request, 1 CU/1000 cpu_us,
    /// 1 CU/1000 egress_bytes.
    fn weights() -> MetricWeights {
        let mut t = MetricWeights::new();
        t.insert("requests".to_string(), w(1, 1));
        t.insert("cpu_us".to_string(), w(1, 1_000));
        t.insert("egress_bytes".to_string(), w(1, 1_000));
        t
    }

    #[test]
    fn total_units_sums_weighted_metrics() {
        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), 100); // 100 CU
        usage.insert("cpu_us".to_string(), 5_500); // floor(5500/1000) = 5 CU
        usage.insert("egress_bytes".to_string(), 2_999); // floor(2999/1000) = 2 CU
        assert_eq!(total_units(&weights(), &usage), 107);
    }

    #[test]
    fn unknown_metric_zero_weight_is_free() {
        // REGRESSION: a metric absent from the weight table contributes 0 CU
        // (free), matching the live "unknown metric is free, not an error".
        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), 10);
        usage.insert("not_a_metric".to_string(), 999_999_999);
        assert_eq!(total_units(&weights(), &usage), 10);
    }

    #[test]
    fn included_units_quota_then_overage() {
        // included_units fully covers usage ⇒ base only; over the quota ⇒ the
        // overage is billed at FX.
        // FX = 1 cent per CU = 10^12 pico-cents/CU.
        let one_cent_per_cu = FX_SCALE as u64;
        let price = PlanPrice {
            base_fee_cents: 500,
            included_units: 100,
            fx_pico_cents_per_unit: Some(one_cent_per_cu),
            spend_limit_default_cents: 5_000,
        };
        // Usage = 100 CU exactly (= included) ⇒ base only.
        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), 100);
        let b = charge_cents(&price, &usage, &weights());
        assert_eq!(b.total_units, 100);
        assert_eq!(b.billable_units, 0, "at/under quota ⇒ nothing billable");
        assert_eq!(b.total_cents, 500, "base fee only");

        // Usage = 150 CU ⇒ 50 billable × 1 cent = 50 cents over the base.
        let mut usage2 = HashMap::new();
        usage2.insert("requests".to_string(), 150);
        let b2 = charge_cents(&price, &usage2, &weights());
        assert_eq!(b2.total_units, 150);
        assert_eq!(b2.billable_units, 50);
        assert_eq!(b2.total_cents, 500 + 50);
    }

    #[test]
    fn included_units_cover_usage_yields_base_only() {
        let price = PlanPrice {
            base_fee_cents: 900,
            included_units: 1_000_000,
            fx_pico_cents_per_unit: Some(FX_SCALE as u64),
            spend_limit_default_cents: 0,
        };
        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), 999_999); // under the included CU
        let b = charge_cents(&price, &usage, &weights());
        assert_eq!(b.billable_units, 0);
        assert_eq!(b.total_cents, 900);
    }

    #[test]
    fn charge_rounds_units_to_cents_exactly_once() {
        // REGRESSION (the whole point of Refactor B). Under the OLD per-metric
        // overage model each metric's cents were rounded independently and
        // summed, biasing the charge upward. Here we accumulate CU across many
        // metrics into ONE total and round to cents ONCE.
        //
        // FX = 0.5 cent per CU = FX_SCALE/2 pico-cents/CU. Three metrics each
        // contributing an ODD number of CU:
        //   requests 1 CU, cpu_us 1 CU (1000 us), egress_bytes 1 CU (1000 bytes)
        //   total_units = 3 CU.
        //   Per-metric rounding (the OLD bug): each 1 CU × 0.5c = 0.5c → round-up
        //     to 1c each ⇒ 3c total.
        //   Round-ONCE (correct): 3 CU × 0.5c = 1.5c → round half-up once = 2c.
        let half_cent_per_cu = (FX_SCALE / 2) as u64;
        let price = PlanPrice {
            base_fee_cents: 0,
            included_units: 0,
            fx_pico_cents_per_unit: Some(half_cent_per_cu),
            spend_limit_default_cents: 0,
        };
        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), 1); // 1 CU
        usage.insert("cpu_us".to_string(), 1_000); // 1 CU
        usage.insert("egress_bytes".to_string(), 1_000); // 1 CU
        let b = charge_cents(&price, &usage, &weights());
        assert_eq!(b.total_units, 3, "CU accumulate across metrics");
        assert_eq!(
            b.total_cents, 2,
            "round ONCE over 3 CU (1.5c → 2c), NOT per-metric (0.5c×3 → 3c)"
        );
    }

    #[test]
    fn fx_change_reprices_without_touching_weights() {
        // REGRESSION proving cost-model / price decoupling: the SAME raw usage +
        // SAME global weight table, but a different per-plan FX, reprices. The
        // weights (engineering cost) are untouched; only the business price (FX)
        // moves.
        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), 1_000); // 1000 CU under weights()
        let ws = weights();

        let cheap = PlanPrice {
            base_fee_cents: 0,
            included_units: 0,
            fx_pico_cents_per_unit: Some(FX_SCALE as u64), // 1 cent/CU
            spend_limit_default_cents: 0,
        };
        let pricey = PlanPrice {
            fx_pico_cents_per_unit: Some((FX_SCALE as u64) * 2), // 2 cents/CU
            ..cheap.clone()
        };

        let bc = charge_cents(&cheap, &usage, &ws);
        let bp = charge_cents(&pricey, &usage, &ws);
        assert_eq!(bc.total_units, 1_000, "CU is weight-derived, unchanged");
        assert_eq!(bp.total_units, 1_000, "same CU under the same weights");
        assert_eq!(bc.total_cents, 1_000, "1000 CU × 1c");
        assert_eq!(bp.total_cents, 2_000, "1000 CU × 2c — FX lever moved, weights did not");
    }

    #[test]
    fn sub_unit_weight_and_sub_cent_fx_compose() {
        // The "$0.30 per 1M requests" tier expressed in CU terms when 1 req = 1
        // CU. fx = 0.00003 cent/CU = 3e-5 cent = 3e-5 × 10^12 = 30_000_000
        // pico-cents/CU (the seeded global default). 1M requests ⇒ 1M CU ×
        // 0.00003c = 30c, rounded once.
        let mut t = MetricWeights::new();
        t.insert("requests".to_string(), w(1, 1));
        let price = PlanPrice {
            base_fee_cents: 0,
            included_units: 0,
            fx_pico_cents_per_unit: Some(30_000_000),
            spend_limit_default_cents: 0,
        };
        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), 1_000_000);
        let b = charge_cents(&price, &usage, &t);
        assert_eq!(b.total_units, 1_000_000);
        assert_eq!(b.total_cents, 30, "1M CU × 0.00003c = 30c, rounded once");
    }

    #[test]
    fn unresolved_fx_prices_base_only() {
        // fx == None (catalog failed to substitute) must price to base-only, never
        // panic. Defensive: the catalog always resolves the default in practice.
        let price = PlanPrice {
            base_fee_cents: 700,
            included_units: 0,
            fx_pico_cents_per_unit: None,
            spend_limit_default_cents: 0,
        };
        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), 5_000);
        let b = charge_cents(&price, &usage, &weights());
        assert_eq!(b.billable_units, 5_000);
        assert_eq!(b.total_cents, 700, "no FX ⇒ base only, no panic");
    }

    #[test]
    fn negative_total_contributes_no_units() {
        // A negative aggregate total (shouldn't happen, but defensive) maps to 0
        // CU, not a wrapping huge value.
        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), -5);
        assert_eq!(total_units(&weights(), &usage), 0);
    }

    #[test]
    fn validate_rejects_explicit_zero_fx_accepts_none_and_positive() {
        let mut p = PlanPrice {
            base_fee_cents: 0,
            included_units: 0,
            fx_pico_cents_per_unit: Some(0),
            spend_limit_default_cents: 0,
        };
        assert!(p.validate().is_err(), "explicit zero FX is rejected");
        p.fx_pico_cents_per_unit = None;
        assert!(p.validate().is_ok(), "None (inherit default) is valid");
        p.fx_pico_cents_per_unit = Some(30_000);
        assert!(p.validate().is_ok(), "positive FX is valid");
    }

    #[test]
    fn zero_per_units_weight_is_free() {
        let mut t = MetricWeights::new();
        t.insert("x".to_string(), w(1, 0)); // degenerate ⇒ free
        let mut usage = HashMap::new();
        usage.insert("x".to_string(), 999);
        assert_eq!(total_units(&t, &usage), 0);
    }
}
