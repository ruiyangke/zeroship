//! PG-backed loaders for the GLOBAL compute-unit pricing config (billing-v2
//! Refactor B): the metric→weight cost model (`zeroship.metric_weights`) and the
//! default FX (`zeroship.pricing_config`).
//!
//! Both are small global (non-tenant) tables — the spend engine and the
//! reconciler each load the whole weight table ONCE per sweep (a handful of
//! rows) and pass it as `&MetricWeights` into [`crate::pricing::charge_cents`].
//! The default FX resolves a plan's `fx == None`.

use crate::pricing::{MetricWeight, MetricWeights, MIN_FX_PICO_CENTS_PER_UNIT};
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
            // MINOR-2: a negative units_per_op is forbidden by the DB CHECK
            // (added in 0041) but we coerce defensively — warn! if it ever fires
            // so a bad row is visible rather than silently treated as free.
            if units_per_op < 0 {
                tracing::warn!(
                    metric = %metric,
                    units_per_op,
                    "metric_weights: negative units_per_op coerced to 0 (CHECK should forbid this)"
                );
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
    ///
    /// `None` if the singleton row is absent OR its value is BELOW the near-zero
    /// floor. Both are PLATFORM MISCONFIGURATIONS, not benign defaults
    /// (MAJOR-2 / MAJOR-3): a plan that inherits (`fx == None`) cannot then be
    /// priced, and the sweeps fail closed (abort) rather than billing base-only
    /// $0. We `tracing::error!` so the bad/missing seed is loud.
    ///
    /// MAJOR-3: a below-floor global FX (`< MIN_FX_PICO_CENTS_PER_UNIT`) is
    /// treated as UNRESOLVED — returning `None` so the sweep fails closed —
    /// rather than coercing a near-zero/zero value to `Some(0)` and silently
    /// pricing ALL overage to $0 platform-wide. The DB CHECK in 0041 makes a
    /// below-floor value unrepresentable at the source; this is defense in depth
    /// (and covers a row written before the CHECK / by a privileged path).
    pub async fn default_fx_pico_cents_per_unit(&self) -> Result<Option<u64>, RegistryError> {
        let conn = self.registry.conn().await?;
        let rows = conn
            .query(
                "SELECT fx_pico_cents_per_unit FROM zeroship.pricing_config WHERE id = 'global'",
                &[],
            )
            .await?;
        let Some(row) = rows.first() else {
            tracing::error!(
                "pricing_config: global default FX row ('global') is MISSING — inheriting plans \
                 cannot be priced; billing sweeps will abort (fail closed). Seed pricing_config."
            );
            return Ok(None);
        };
        let fx: i64 = row.get("fx_pico_cents_per_unit");
        if fx < 0 || (fx as u64) < MIN_FX_PICO_CENTS_PER_UNIT {
            tracing::error!(
                fx,
                floor = MIN_FX_PICO_CENTS_PER_UNIT,
                "pricing_config: global default FX is BELOW the near-zero floor — refusing to \
                 price all overage to ~$0. Treating as UNRESOLVED so the sweep fails closed; \
                 fix pricing_config.fx_pico_cents_per_unit."
            );
            return Ok(None);
        }
        Ok(Some(fx as u64))
    }

    /// Set the GLOBAL default FX (pico-cents per CU) — the singleton
    /// `pricing_config.id='global'` row. Operator-only at the HTTP layer
    /// (`PUT /api/pricing-config`, gated `BillingWrite`/`Resource::Any`).
    ///
    /// Returns the PREVIOUS value (so the handler can audit the old→new
    /// transition — this is the highest-leverage price lever, it reprices every
    /// inheriting plan). `None` is returned for the previous value when the
    /// singleton row was absent before this write (an unseeded DB).
    ///
    /// The FX floor (`>= MIN_FX_PICO_CENTS_PER_UNIT`) is enforced HERE as defense
    /// in depth — the HTTP handler rejects a below-floor value with a 400 BEFORE
    /// calling this, and the DB CHECK in changeset 0041 makes a below-floor value
    /// unrepresentable at the source. A caller passing a below-floor value is a
    /// programming error; we reject it cleanly with [`RegistryError::InvalidInput`]
    /// (fail closed) rather than letting the DB CHECK surface as an opaque
    /// `Database` error.
    ///
    /// Reproducibility note: changing the global default FX affects FUTURE pricing
    /// only. Finalized invoices snapshot their own effective FX onto each line at
    /// finalize time, so a re-priced global default never rewrites a settled
    /// invoice (no invoices are finalized in this config-write flow).
    pub async fn set_default_fx(&self, fx_pico_cents_per_unit: u64) -> Result<Option<u64>, RegistryError> {
        if fx_pico_cents_per_unit < MIN_FX_PICO_CENTS_PER_UNIT {
            return Err(RegistryError::InvalidInput(format!(
                "fx_pico_cents_per_unit must be >= {MIN_FX_PICO_CENTS_PER_UNIT} (got \
                 {fx_pico_cents_per_unit}); a near-zero global FX prices all overage to ~$0"
            )));
        }
        // The column is BIGINT (i64); reject (don't clamp) a value above i64::MAX
        // so the stored value always round-trips exactly.
        let fx_i64 = i64::try_from(fx_pico_cents_per_unit).map_err(|_| {
            RegistryError::InvalidInput(format!(
                "fx_pico_cents_per_unit {fx_pico_cents_per_unit} exceeds i64::MAX — refusing to clamp"
            ))
        })?;

        let conn = self.registry.conn().await?;
        // UPSERT the singleton: an unseeded DB mints the 'global' row; a seeded DB
        // updates it. The RETURNING-via-old-value pattern needs the prior value,
        // so read-then-write would race; instead we read the old value in the same
        // statement using a CTE that captures the pre-update row.
        let rows = conn
            .query(
                "WITH prev AS ( \
                    SELECT fx_pico_cents_per_unit AS old_fx \
                    FROM zeroship.pricing_config WHERE id = 'global' \
                 ) \
                 INSERT INTO zeroship.pricing_config (id, fx_pico_cents_per_unit, updated_at) \
                 VALUES ('global', $1, NOW()) \
                 ON CONFLICT (id) DO UPDATE SET \
                    fx_pico_cents_per_unit = EXCLUDED.fx_pico_cents_per_unit, \
                    updated_at = NOW() \
                 RETURNING (SELECT old_fx FROM prev) AS old_fx",
                &[&fx_i64],
            )
            .await?;
        let old: Option<i64> = rows.first().and_then(|r| r.get("old_fx"));
        Ok(old.map(|v| v.max(0) as u64))
    }
}
