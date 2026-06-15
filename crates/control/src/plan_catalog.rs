//! Plan catalog — the operator-editable, server-side pricing catalog
//! (billing PR4, closes CT-A1).
//!
//! Replaces the free-text, self-escalatable `plan_id`: every plan is a row in
//! `zeroship.plans` keyed by a `pln_<base62>` typed id, and `apps.plan_id` is
//! an FK into it (added by changeset 0038). An app can no longer pick an
//! unpriced or oversized plan — the FK + the [`Registry`]'s existence check
//! reject any id that is not a real, unarchived plan.
//!
//! The catalog is a GLOBAL operator config (not tenant-scoped) — `plans` has no
//! RLS; control is `BYPASSRLS`. Under billing-v2 compute-unit pricing the price
//! model is SCALAR (`included_units` + a nullable per-plan FX) — read back into
//! the pure [`crate::pricing::PlanPrice`]; only `runtime_limits_json` stays
//! JSONB ([`AppRuntimeLimits`]).

use compio_postgres::Row;
use zeroship_core::types::{AppRuntimeLimits, FREE_TIER_RUNTIME_LIMITS};

use crate::pricing::PlanPrice;
use crate::registry::{Registry, RegistryError};

/// One catalog entry: the typed id, display name, the resolved price model, the
/// runtime limits this tier grants, and whether it has been archived (archived
/// plans stay resolvable for historical FKs but can't be assigned to new apps).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// `pln_<base62>` typed id.
    pub id: String,
    pub name: String,
    pub price: PlanPrice,
    pub runtime: AppRuntimeLimits,
    pub archived: bool,
    /// MAJOR-4: whether an app_owner (creator) principal may self-assign this
    /// plan via `PUT /api/apps/:id/plan`. The public tiers (`free`, `pro`) are
    /// `true`; operator tiers (console/enterprise/unlimited) are `false`. An
    /// operator (`BillingWrite` on `Resource::Any`) may assign ANY plan
    /// regardless of this flag.
    pub assignable_by_creator: bool,
}

/// PG-backed plan catalog. Shares the control plane's per-query connection
/// model via [`Registry`].
#[derive(Clone, Debug)]
pub struct PlanCatalog {
    registry: Registry,
}

impl PlanCatalog {
    #[must_use]
    pub fn new(registry: Registry) -> Self {
        Self { registry }
    }

    /// Fetch one plan by id. `None` if no such row (archived plans ARE
    /// returned — the caller decides whether to reject an archived plan).
    ///
    /// MAJOR-2: a corrupt `runtime_limits_json` no longer hard-errors here —
    /// `row_to_plan` falls back to free-tier runtime limits so the plan still
    /// PRICES (billing reconcile must still bill an app whose plan row's JSONB
    /// is poison). See [`row_to_plan`].
    pub async fn get(&self, id: &str) -> Result<Option<Plan>, RegistryError> {
        let conn = self.registry.conn().await?;
        let rows = conn
            .query(
                "SELECT id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                        runtime_limits_json, spend_limit_default_cents, archived, \
                        assignable_by_creator \
                 FROM zeroship.plans WHERE id = $1",
                &[&id],
            )
            .await?;
        rows.first().map(row_to_plan).transpose()
    }

    /// List every plan (including archived ones) ordered by id.
    ///
    /// MAJOR-2: poison tolerance is now in `row_to_plan` itself — a corrupt
    /// `runtime_limits_json` falls back to free-tier runtime limits and the plan
    /// is still RETURNED (not skipped), so spend enforcement still prices (and
    /// can Block) the app. `get(:id)` is poison-tolerant the SAME way, so the
    /// two pricing paths agree: neither silently drops an app on a poison row.
    /// A truly undecodable row (e.g. a non-nullable scalar column returns an
    /// error) is logged + skipped here, but such a row cannot exist under the
    /// NOT-NULL/CHECK schema.
    pub async fn list(&self) -> Result<Vec<Plan>, RegistryError> {
        let conn = self.registry.conn().await?;
        let rows = conn
            .query(
                "SELECT id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                        runtime_limits_json, spend_limit_default_cents, archived, \
                        assignable_by_creator \
                 FROM zeroship.plans ORDER BY id",
                &[],
            )
            .await?;
        let mut plans = Vec::with_capacity(rows.len());
        for row in &rows {
            match row_to_plan(row) {
                Ok(plan) => plans.push(plan),
                Err(e) => {
                    let id: String = row.get("id");
                    tracing::warn!(
                        plan_id = %id,
                        error = %e,
                        "plan_catalog: list: skipping unparseable plan row"
                    );
                }
            }
        }
        Ok(plans)
    }

    /// Insert-or-update a plan (operator / master-key gated at the HTTP layer).
    /// The id is the primary key; an existing id is updated in place (so the
    /// built-in tiers are idempotently re-seeded on every boot). Returns the
    /// written [`Plan`].
    ///
    /// `archived` controls the archived flag on the UPSERT:
    ///   - `Some(b)` — set `archived = b` explicitly (the only way to UN-archive
    ///     is `Some(false)`; un-archiving must be deliberate).
    ///   - `None` — PRESERVE the existing row's `archived` on conflict
    ///     (`COALESCE($8, plans.archived)`); a brand-new row defaults to
    ///     `false`. This is what a PUT without an `archived` field maps to, so a
    ///     name/price edit can't silently resurrect an archived plan.
    ///
    /// `plan.archived` is ignored for the flag — pass the intent via `archived`.
    pub async fn upsert(&self, plan: &Plan, archived: Option<bool>) -> Result<Plan, RegistryError> {
        // MINOR (write-path i64 clamps): validate the price model at the write
        // boundary as DEFENSE IN DEPTH. The HTTP handler already validates, but
        // a direct `upsert` (bootstrap seed, future internal callers) must not
        // silently clamp an out-of-range `base_fee`/`included_units`/`fx`/
        // `spend_default` to i64::MAX below — `validate()` rejects an
        // included_units above the i64 ceiling (and a below-floor FX) so an
        // out-of-range plan write is a HARD ERROR, not a silent clamp.
        plan.price.validate().map_err(RegistryError::InvalidInput)?;

        let runtime_limits_json = serde_json::to_value(&plan.runtime)
            .map_err(|e| RegistryError::InvalidInput(format!("runtime_limits_json: {e}")))?;
        // `base_fee` and `spend_limit_default` have no explicit ceiling in
        // `validate()` but cannot exceed i64::MAX after that check on realistic
        // inputs; the try_from below is retained as a final guard and now warns
        // on the (validate-unreachable) overflow rather than silently producing
        // a wrong value. `included_units`/`fx` are already bounded by validate().
        let base_fee = i64::try_from(plan.price.base_fee_cents).map_err(|_| {
            RegistryError::InvalidInput(format!(
                "base_fee_cents {} exceeds i64::MAX — refusing to clamp",
                plan.price.base_fee_cents
            ))
        })?;
        let included_units = i64::try_from(plan.price.included_units).map_err(|_| {
            RegistryError::InvalidInput(format!(
                "included_units {} exceeds i64::MAX — refusing to clamp",
                plan.price.included_units
            ))
        })?;
        // fx is per-plan and NULLABLE (NULL ⇒ global pricing_config default).
        let fx_pico: Option<i64> = match plan.price.fx_pico_cents_per_unit {
            Some(fx) => Some(i64::try_from(fx).map_err(|_| {
                RegistryError::InvalidInput(format!(
                    "fx_pico_cents_per_unit {fx} exceeds i64::MAX — refusing to clamp"
                ))
            })?),
            None => None,
        };
        let spend_default = i64::try_from(plan.price.spend_limit_default_cents).map_err(|_| {
            RegistryError::InvalidInput(format!(
                "spend_limit_default_cents {} exceeds i64::MAX — refusing to clamp",
                plan.price.spend_limit_default_cents
            ))
        })?;

        let conn = self.registry.conn().await?;
        let rows = conn
            .query(
                // INSERT defaults a new row's archived to COALESCE($8, false);
                // ON CONFLICT preserves the existing value when $8 is NULL
                // (COALESCE($8, plans.archived)) so a PUT without `archived`
                // never un-archives.
                "INSERT INTO zeroship.plans \
                   (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                    runtime_limits_json, spend_limit_default_cents, archived, \
                    assignable_by_creator, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, COALESCE($8, false), $9, NOW()) \
                 ON CONFLICT (id) DO UPDATE SET \
                    name = EXCLUDED.name, \
                    base_fee_cents = EXCLUDED.base_fee_cents, \
                    included_units = EXCLUDED.included_units, \
                    fx_pico_cents_per_unit = EXCLUDED.fx_pico_cents_per_unit, \
                    runtime_limits_json = EXCLUDED.runtime_limits_json, \
                    spend_limit_default_cents = EXCLUDED.spend_limit_default_cents, \
                    archived = COALESCE($8, zeroship.plans.archived), \
                    assignable_by_creator = EXCLUDED.assignable_by_creator, \
                    updated_at = NOW() \
                 RETURNING id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                           runtime_limits_json, spend_limit_default_cents, archived, \
                           assignable_by_creator",
                &[
                    &plan.id,
                    &plan.name,
                    &base_fee,
                    &included_units,
                    &fx_pico,
                    &runtime_limits_json,
                    &spend_default,
                    &archived,
                    &plan.assignable_by_creator,
                ],
            )
            .await?;
        rows.first()
            .map(row_to_plan)
            .ok_or_else(|| RegistryError::Database("upsert ok but read-back failed".into()))?
    }

    /// Archive a plan (soft delete — `archived = true`). The row stays so
    /// existing `apps.plan_id` FKs and historical billing runs keep resolving.
    /// Returns `true` if a row was updated. There is NO hard DELETE.
    pub async fn archive(&self, id: &str) -> Result<bool, RegistryError> {
        let conn = self.registry.conn().await?;
        let n = conn
            .execute(
                "UPDATE zeroship.plans SET archived = true, updated_at = NOW() WHERE id = $1",
                &[&id],
            )
            .await?;
        Ok(n > 0)
    }
}

/// Decode a `plans` row into a [`Plan`]. The price model is scalar (CU pricing);
/// `runtime_limits_json` is the only JSONB column. `fx_pico_cents_per_unit` is
/// nullable (NULL ⇒ the plan inherits the global `pricing_config` default — the
/// engine/reconciler resolve `None` before pricing).
///
/// MAJOR-2 (poison tolerance FOR PRICING): a corrupt `runtime_limits_json` does
/// NOT fail the decode. Pricing only needs the scalar price columns
/// (`base_fee_cents`, `included_units`, `fx`, `spend_limit_default_cents`) —
/// NOT the runtime limits. A row whose JSONB can't parse falls back to the
/// conservative [`FREE_TIER_RUNTIME_LIMITS`] (with a `warn!`) so BOTH `list()`
/// (spend enforcement) and `get()` (billing reconcile) still PRICE the app —
/// previously `list` SKIPPED the poison row (app ran uncapped) while `get`
/// HARD-ERRORED (creator's whole bill failed), so a poison plan made an app
/// both uncapped AND unbilled. The runtime-limits consumer in
/// `registry.rs::get_versions` has its OWN conservative fallback and does not go
/// through this decoder, so a real runtime-limits read is unaffected.
fn row_to_plan(row: &Row) -> Result<Plan, RegistryError> {
    let base_fee: i64 = row.get("base_fee_cents");
    let included_units: i64 = row.get("included_units");
    let fx_pico: Option<i64> = row.get("fx_pico_cents_per_unit");
    let spend_default: i64 = row.get("spend_limit_default_cents");
    let runtime_limits_json: serde_json::Value = row.get("runtime_limits_json");
    let id: String = row.get("id");

    let runtime: AppRuntimeLimits = serde_json::from_value(runtime_limits_json).unwrap_or_else(|e| {
        tracing::warn!(
            plan_id = %id,
            error = %e,
            "plan_catalog: runtime_limits_json parse failure — using free-tier fallback so the \
             plan still PRICES (MAJOR-2 poison tolerance); enforcement + billing both see the app"
        );
        FREE_TIER_RUNTIME_LIMITS
    });

    Ok(Plan {
        id,
        name: row.get("name"),
        price: PlanPrice {
            base_fee_cents: base_fee.max(0) as u64,
            included_units: included_units.max(0) as u64,
            fx_pico_cents_per_unit: fx_pico.map(|fx| fx.max(0) as u64),
            spend_limit_default_cents: spend_default.max(0) as u64,
        },
        runtime,
        archived: row.get("archived"),
        assignable_by_creator: row.get("assignable_by_creator"),
    })
}
