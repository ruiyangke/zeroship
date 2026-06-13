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
//! metric cannot overflow `u128`. The cents conversion `billable_units (u64, ≤
//! ~1.8e19) × fx_pico (u64, ≤ ~1.8e19)` is a `u128` product (≤ ~3.4e38, within
//! `u128::MAX ≈ 3.4e38`) divided by `10¹²`, rounded half-up. Realistic magnitudes
//! (billable ≤ ~1e12 CU, fx ≤ ~1e9 pico-cents) sit ~17 orders of magnitude below
//! the `u128` ceiling.
//!
//! ## Money never silently clamps (MAJOR-1 / MAJOR-2)
//!
//! [`charge_cents`] is **fallible** on every path that could otherwise emit a
//! wrong-but-plausible bill with no operator signal:
//!
//! - A per-metric CU total that does not fit `u64`, or a cross-metric sum that
//!   overflows `u64`, is a hard [`PricingError::ComputeUnitOverflow`] (matching
//!   `billing_reconcile`'s cents→i64 posture: skip-the-creator-with-a-warning,
//!   never a clamped bill). [`total_units`] surfaces this via `Result` rather
//!   than the old `unwrap_or(u64::MAX)` + `saturating_add` silent cap.
//! - An **unresolved FX** (`fx == None` reaching the pricer) is a hard
//!   [`PricingError::UnresolvedFx`] — the platform cannot price, so the sweep
//!   aborts (bills no one) rather than silently charging base-only $0. The
//!   catalog resolves `None` to the global default before pricing; a *missing
//!   global default* is what makes this fire (see `pricing_store`), and it must
//!   stop the tick, not leak revenue per-app.
//! - A cents total that overflows `u64` is still saturated-with-`warn!` inside
//!   the half-up conversion — that ceiling (~$1.8e17) is unreachable for any
//!   real charge and the downstream `i64` clamp in `billing_reconcile` already
//!   hard-errors, so it is the one documented saturated boundary.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// The fixed integer scale for the FX (cents-per-CU) lever: FX is stored as
/// **pico-cents per CU** (10⁻¹² cent). `cents = round_half_up(billable_units ×
/// fx_pico_cents_per_unit / FX_SCALE)`, computed once.
pub const FX_SCALE: u128 = 1_000_000_000_000; // 10^12

/// A pricing failure that MUST abort the charge rather than emit a silently
/// wrong bill. Both variants are money-correctness guards (MAJOR-1 / MAJOR-2):
/// the sweep propagates them — skipping the affected creator with a warning, or
/// aborting the whole tick for the global-FX case — never clamping to a
/// plausible-but-wrong number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PricingError {
    /// The accumulated compute-unit total (per-metric, or the cross-metric sum)
    /// exceeds `u64::MAX`. Carries the offending metric (or `total`) for the
    /// operator. A clamp here would under- or over-bill silently.
    ComputeUnitOverflow {
        metric: String,
        raw: i64,
        units_per_op: u64,
        per_units: u64,
    },
    /// The plan's FX reached the pricer unresolved (`None`). The catalog must
    /// substitute the global default before pricing; an unresolved FX means the
    /// platform CANNOT price (the global default is missing) — pricing $0 here
    /// would be a silent revenue leak, so the sweep aborts instead.
    UnresolvedFx,
}

impl std::fmt::Display for PricingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ComputeUnitOverflow { metric, raw, units_per_op, per_units } => write!(
                f,
                "compute-unit total overflowed u64 (metric={metric}, raw={raw}, \
                 units_per_op={units_per_op}, per_units={per_units}) — refusing to clamp the bill"
            ),
            Self::UnresolvedFx => write!(
                f,
                "FX is unresolved (no per-plan fx and no global default) — platform cannot price; \
                 aborting rather than billing $0"
            ),
        }
    }
}

impl std::error::Error for PricingError {}

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

/// Lower sanity floor for an explicitly-set FX (pico-cents per CU). An FX below
/// this is so close to zero that all overage prices to ~$0 — almost certainly an
/// operator fat-finger rather than a real "nearly free" tier. `1_000`
/// pico-cents/CU = 10⁻⁹ cent/CU; at this floor even 1e15 CU bills < 1 cent, so
/// anything below it cannot represent a real per-unit price. (To make a tier
/// genuinely free, raise `included_units`, not the FX toward zero.) Operator
/// guardrail only — not a back-compat constraint.
pub const MIN_FX_PICO_CENTS_PER_UNIT: u64 = 1_000;

/// Upper sanity ceiling for `included_units`. The `plans.included_units` column
/// is `BIGINT` (i64), so a value above `i64::MAX` silently clamps at the catalog
/// write boundary (`plan_catalog.rs`). We reject anything above `i64::MAX` so the
/// stored value always round-trips exactly — an "effectively infinite" quota is
/// an operator mistake (use the `unlimited` tier's `spend_limit_default = 0`
/// uncapped posture instead). Operator guardrail only.
pub const MAX_INCLUDED_UNITS: u64 = i64::MAX as u64;

impl PlanPrice {
    /// Semantic validation of a catalog price model, run at the write boundary
    /// (the `PUT /api/plans/:id` handler) so a malformed price is a 400, not a
    /// silently-wrong charge at billing time.
    ///
    /// Under CU pricing the price model is scalar; the operator guardrails are:
    ///
    /// - **FX floor (MAJOR-3):** if set, the FX must be `>=`
    ///   [`MIN_FX_PICO_CENTS_PER_UNIT`]. `Some(0)` (prices all overage free) and
    ///   any absurdly-low non-zero value (e.g. `Some(1)` ≈ 10⁻¹² cent/CU, free
    ///   for all practical usage) are rejected so a "nearly free" tier is an
    ///   explicit choice (high `included_units`), not a silent near-zero price.
    ///   `None` (inherit the global default) is always valid.
    /// - **`included_units` ceiling (MAJOR-3):** must be `<=`
    ///   [`MAX_INCLUDED_UNITS`] (`i64::MAX`) so it round-trips the `BIGINT`
    ///   column exactly instead of silently clamping at the i64 boundary.
    ///
    /// # Errors
    /// Returns a human-readable message when the FX is below the floor or
    /// `included_units` exceeds the ceiling.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(fx) = self.fx_pico_cents_per_unit {
            if fx < MIN_FX_PICO_CENTS_PER_UNIT {
                return Err(format!(
                    "fx_pico_cents_per_unit must be >= {MIN_FX_PICO_CENTS_PER_UNIT} when set \
                     (got {fx}; omit it to inherit the global default — a near-zero FX prices \
                     all overage to ~$0; to make a tier free raise included_units instead)"
                ));
            }
        }
        if self.included_units > MAX_INCLUDED_UNITS {
            return Err(format!(
                "included_units must be <= {MAX_INCLUDED_UNITS} (i64::MAX); got {} — a value \
                 above the BIGINT column ceiling would silently clamp. For an uncapped tier use \
                 spend_limit_default_cents = 0, not an effectively-infinite quota",
                self.included_units
            ));
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
    ///
    /// The floor is applied **per metric, before summing** — so the sub-CU
    /// remainder of each metric is dropped independently. This is a deliberate,
    /// bounded (< 1 CU per weighted metric) systematic UNDER-count that favors
    /// the creator: it can never over-bill from rounding. Summing raw then
    /// flooring once would be marginally less generous; flooring per metric is
    /// by design.
    pub total_units: u64,
    /// `max(0, total_units − included_units)`.
    pub billable_units: u64,
    pub total_cents: u64,
}

/// Accumulate total compute units for a usage map under the global weight table.
///
/// `total_units = Σ_m floor( max(0, usage[m]) × units_per_op[m] / per_units[m] )`,
/// integer throughout (`u128` intermediate, floored per metric). A metric with
/// no weight — or a `per_units == 0` weight — contributes 0.
///
/// **MAJOR-1:** an overflow is NEVER a silent clamp. If a single metric's CU
/// total does not fit `u64`, or the cross-metric sum overflows `u64`, this
/// `warn!`s the offending `(metric, raw, units_per_op, per_units)` and returns
/// [`PricingError::ComputeUnitOverflow`] so the caller can skip-with-a-warning
/// instead of billing a wrong-but-plausible clamped number.
///
/// # Errors
/// [`PricingError::ComputeUnitOverflow`] when a per-metric or cross-metric CU
/// total exceeds `u64::MAX`.
pub fn total_units(
    weights: &MetricWeights,
    usage: &HashMap<String, i64>,
) -> Result<u64, PricingError> {
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
        let metric_units = u64::try_from(metric_units).map_err(|_| {
            tracing::warn!(
                metric = %metric,
                raw,
                units_per_op = w.units_per_op,
                per_units = w.per_units,
                "pricing: per-metric compute-unit total exceeds u64::MAX — refusing to clamp"
            );
            PricingError::ComputeUnitOverflow {
                metric: metric.clone(),
                raw,
                units_per_op: w.units_per_op,
                per_units: w.per_units,
            }
        })?;
        acc = acc.checked_add(metric_units).ok_or_else(|| {
            tracing::warn!(
                metric = %metric,
                raw,
                units_per_op = w.units_per_op,
                per_units = w.per_units,
                acc,
                "pricing: cross-metric compute-unit sum overflowed u64 — refusing to clamp"
            );
            PricingError::ComputeUnitOverflow {
                metric: metric.clone(),
                raw,
                units_per_op: w.units_per_op,
                per_units: w.per_units,
            }
        })?;
    }
    Ok(acc)
}

/// Compute the full period charge for a plan price against a usage map and the
/// global weight table.
///
/// Accumulates integer CU across every metric ([`total_units`]), subtracts the
/// plan's `included_units`, and converts the billable CU to cents **exactly
/// once** via the FX lever (`× fx_pico ÷ FX_SCALE`, half-up).
///
/// **MAJOR-2:** the FX must be resolved before pricing. `fx == None` reaching
/// this function is a hard [`PricingError::UnresolvedFx`] — the platform cannot
/// price, so the sweep aborts (bills no one) rather than silently charging
/// base-only $0 (a revenue leak). The catalog substitutes the global default
/// (`with_effective_fx`) before calling; a *missing global default* is what
/// leaves `None` here.
///
/// # Errors
/// - [`PricingError::ComputeUnitOverflow`] — see [`total_units`].
/// - [`PricingError::UnresolvedFx`] — the FX was not resolved (no per-plan fx
///   and no global default).
pub fn charge_cents(
    price: &PlanPrice,
    usage: &HashMap<String, i64>,
    weights: &MetricWeights,
) -> Result<ChargeBreakdown, PricingError> {
    let total = total_units(weights, usage)?;
    let billable = total.saturating_sub(price.included_units);
    let Some(fx_pico) = price.fx_pico_cents_per_unit else {
        tracing::error!(
            "pricing: charge_cents called with unresolved FX (None) — global default missing; \
             refusing to bill $0"
        );
        return Err(PricingError::UnresolvedFx);
    };
    let overage_cents = div_round_half_up(u128::from(billable) * u128::from(fx_pico), FX_SCALE);
    let total_cents = price.base_fee_cents.saturating_add(overage_cents);
    Ok(ChargeBreakdown {
        base_cents: price.base_fee_cents,
        total_units: total,
        billable_units: billable,
        total_cents,
    })
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
        assert_eq!(total_units(&weights(), &usage).unwrap(), 107);
    }

    #[test]
    fn unknown_metric_zero_weight_is_free() {
        // REGRESSION: a metric absent from the weight table contributes 0 CU
        // (free), matching the live "unknown metric is free, not an error".
        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), 10);
        usage.insert("not_a_metric".to_string(), 999_999_999);
        assert_eq!(total_units(&weights(), &usage).unwrap(), 10);
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
        let b = charge_cents(&price, &usage, &weights()).unwrap();
        assert_eq!(b.total_units, 100);
        assert_eq!(b.billable_units, 0, "at/under quota ⇒ nothing billable");
        assert_eq!(b.total_cents, 500, "base fee only");

        // Usage = 150 CU ⇒ 50 billable × 1 cent = 50 cents over the base.
        let mut usage2 = HashMap::new();
        usage2.insert("requests".to_string(), 150);
        let b2 = charge_cents(&price, &usage2, &weights()).unwrap();
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
        let b = charge_cents(&price, &usage, &weights()).unwrap();
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
        let b = charge_cents(&price, &usage, &weights()).unwrap();
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

        let bc = charge_cents(&cheap, &usage, &ws).unwrap();
        let bp = charge_cents(&pricey, &usage, &ws).unwrap();
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
        let b = charge_cents(&price, &usage, &t).unwrap();
        assert_eq!(b.total_units, 1_000_000);
        assert_eq!(b.total_cents, 30, "1M CU × 0.00003c = 30c, rounded once");
    }

    #[test]
    fn unresolved_fx_fails_closed_not_base_only() {
        // MAJOR-2 REGRESSION: fx == None reaching the pricer is NOT base-only $0
        // (a silent revenue leak); it is a hard UnresolvedFx error so the sweep
        // aborts (bills no one) rather than charging the wrong amount. The catalog
        // resolves None to the global default in practice; this fires only when
        // the global default itself is missing.
        let price = PlanPrice {
            base_fee_cents: 700,
            included_units: 0,
            fx_pico_cents_per_unit: None,
            spend_limit_default_cents: 0,
        };
        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), 5_000);
        let err = charge_cents(&price, &usage, &weights()).unwrap_err();
        assert_eq!(
            err,
            PricingError::UnresolvedFx,
            "unresolved FX must error (fail closed), never silently bill base-only $0"
        );
    }

    #[test]
    fn negative_total_contributes_no_units() {
        // A negative aggregate total (shouldn't happen, but defensive) maps to 0
        // CU, not a wrapping huge value.
        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), -5);
        assert_eq!(total_units(&weights(), &usage).unwrap(), 0);
    }

    #[test]
    fn total_units_overflow_is_loud_error_not_silent_clamp() {
        // MAJOR-1 REGRESSION: a weight/usage pair that forces the per-metric CU
        // total past u64::MAX must surface PricingError::ComputeUnitOverflow
        // (loud, with a warn!), NOT a silent u64::MAX clamp + saturating_add that
        // would emit a wrong-but-plausible bill.
        //
        // raw = i64::MAX (~9.2e18), units_per_op = 4, per_units = 1 ⇒
        //   ~3.7e19 CU > u64::MAX (~1.8e19). Old code clamped to u64::MAX silently.
        let mut t = MetricWeights::new();
        t.insert("requests".to_string(), w(4, 1));
        let mut usage = HashMap::new();
        usage.insert("requests".to_string(), i64::MAX);
        match total_units(&t, &usage) {
            Err(PricingError::ComputeUnitOverflow { metric, units_per_op, per_units, .. }) => {
                assert_eq!(metric, "requests");
                assert_eq!(units_per_op, 4);
                assert_eq!(per_units, 1);
            }
            other => panic!("expected ComputeUnitOverflow, got {other:?}"),
        }
        // And it propagates through charge_cents (the money path) rather than
        // being swallowed into a clamped total.
        let price = PlanPrice {
            base_fee_cents: 0,
            included_units: 0,
            fx_pico_cents_per_unit: Some(FX_SCALE as u64),
            spend_limit_default_cents: 0,
        };
        assert_eq!(
            charge_cents(&price, &usage, &t).unwrap_err(),
            PricingError::ComputeUnitOverflow {
                metric: "requests".to_string(),
                raw: i64::MAX,
                units_per_op: 4,
                per_units: 1,
            },
        );
    }

    #[test]
    fn total_units_cross_metric_sum_overflow_is_loud_error() {
        // MAJOR-1 REGRESSION (cross-metric arm): two metrics that each fit u64 but
        // whose SUM overflows must error via checked_add, not silently saturate.
        let mut t = MetricWeights::new();
        // raw=i64::MAX, units_per_op=2 ⇒ ~1.84e19 CU each (just fits u64::MAX),
        // so each metric passes the per-metric try_from but their SUM overflows.
        t.insert("a".to_string(), w(2, 1));
        t.insert("b".to_string(), w(2, 1));
        let mut usage = HashMap::new();
        usage.insert("a".to_string(), i64::MAX);
        usage.insert("b".to_string(), i64::MAX);
        assert!(
            matches!(total_units(&t, &usage), Err(PricingError::ComputeUnitOverflow { .. })),
            "cross-metric sum overflow must be a loud error, not a saturating clamp"
        );
    }

    #[test]
    fn validate_rejects_zero_and_near_zero_fx_accepts_none_and_above_floor() {
        // MAJOR-3: zero AND absurdly-low non-zero FX are both rejected; None and
        // any FX >= the floor are accepted.
        let mut p = PlanPrice {
            base_fee_cents: 0,
            included_units: 0,
            fx_pico_cents_per_unit: Some(0),
            spend_limit_default_cents: 0,
        };
        assert!(p.validate().is_err(), "explicit zero FX is rejected");
        p.fx_pico_cents_per_unit = Some(1); // ~10^-12 cent/CU — effectively free
        assert!(p.validate().is_err(), "absurdly-low non-zero FX is rejected (MAJOR-3)");
        p.fx_pico_cents_per_unit = Some(MIN_FX_PICO_CENTS_PER_UNIT - 1);
        assert!(p.validate().is_err(), "just below the floor is rejected");
        p.fx_pico_cents_per_unit = Some(MIN_FX_PICO_CENTS_PER_UNIT);
        assert!(p.validate().is_ok(), "at the floor is valid");
        p.fx_pico_cents_per_unit = None;
        assert!(p.validate().is_ok(), "None (inherit default) is valid");
        p.fx_pico_cents_per_unit = Some(30_000_000);
        assert!(p.validate().is_ok(), "the seeded default FX is valid");
    }

    #[test]
    fn validate_rejects_included_units_above_i64_ceiling() {
        // MAJOR-3: an included_units that would clamp at the BIGINT (i64) column
        // boundary is rejected so the stored value always round-trips.
        let p = PlanPrice {
            base_fee_cents: 0,
            included_units: u64::MAX, // would clamp to i64::MAX at the write boundary
            fx_pico_cents_per_unit: Some(30_000_000),
            spend_limit_default_cents: 0,
        };
        assert!(p.validate().is_err(), "included_units = u64::MAX is rejected");
        let ok = PlanPrice { included_units: MAX_INCLUDED_UNITS, ..p.clone() };
        assert!(ok.validate().is_ok(), "included_units = i64::MAX is the accepted ceiling");
    }

    #[test]
    fn zero_per_units_weight_is_free() {
        let mut t = MetricWeights::new();
        t.insert("x".to_string(), w(1, 0)); // degenerate ⇒ free
        let mut usage = HashMap::new();
        usage.insert("x".to_string(), 999);
        assert_eq!(total_units(&t, &usage).unwrap(), 0);
    }
}
