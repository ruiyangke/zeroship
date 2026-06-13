//! PG-backed loaders for the GLOBAL compute-unit pricing config (billing-v2
//! Refactor B): the metric→weight cost model (`zeroship.metric_weights`) and the
//! default FX (`zeroship.pricing_config`).
//!
//! Both are small global (non-tenant) tables — the spend engine and the
//! reconciler each load the whole weight table ONCE per sweep (a handful of
//! rows) and pass it as `&MetricWeights` into [`crate::pricing::charge_cents`].
//! The default FX resolves a plan's `fx == None`.

use crate::pricing::{MetricWeight, MetricWeights};
use crate::registry::{Registry, RegistryError};

/// PG-backed reader for the global cost model + FX default.
#[derive(Clone, Debug)]
pub struct PricingStore {
    registry: Registry,
}

impl PricingStore {
    #[must_use]
    pub fn new(registry: Registry) -> Self {
        Self { registry }
    }

    /// Load the entire global weight table. A row with `per_units <= 0` (the
    /// CHECK forbids it, but be defensive) is skipped so it can never panic the
    /// divisor downstream. Cheap — a handful of rows, read once per sweep.
    pub async fn weights(&self) -> Result<MetricWeights, RegistryError> {
        let conn = self.registry.conn().await?;
        let rows = conn
            .query(
                "SELECT metric, units_per_op, per_units FROM zeroship.metric_weights",
                &[],
            )
            .await?;
        let mut table = MetricWeights::with_capacity(rows.len());
        for row in &rows {
            let metric: String = row.get("metric");
            let units_per_op: i64 = row.get("units_per_op");
            let per_units: i64 = row.get("per_units");
            if per_units <= 0 {
                tracing::warn!(metric = %metric, per_units, "metric_weights: non-positive per_units — skipping");
                continue;
            }
            table.insert(
                metric,
                MetricWeight {
                    units_per_op: units_per_op.max(0) as u64,
                    per_units: per_units as u64,
                },
            );
        }
        Ok(table)
    }

    /// The global default FX (pico-cents per CU) — `pricing_config.id='global'`.
    /// `None` if the singleton row is absent (treated as 0 ⇒ base-only pricing
    /// by [`crate::pricing::charge_cents`]; logged so a missing seed is visible).
    pub async fn default_fx_pico_cents_per_unit(&self) -> Result<Option<u64>, RegistryError> {
        let conn = self.registry.conn().await?;
        let rows = conn
            .query(
                "SELECT fx_pico_cents_per_unit FROM zeroship.pricing_config WHERE id = 'global'",
                &[],
            )
            .await?;
        Ok(rows.first().map(|r| {
            let fx: i64 = r.get("fx_pico_cents_per_unit");
            fx.max(0) as u64
        }))
    }
}
