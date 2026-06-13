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
//! RLS; control is `BYPASSRLS`. Reads back the JSONB price/quota/limits columns
//! into the pure [`crate::pricing`] + [`AppRuntimeLimits`] types.

use compio_postgres::Row;
use zeroship_core::types::AppRuntimeLimits;

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
    pub async fn get(&self, id: &str) -> Result<Option<Plan>, RegistryError> {
        let conn = self.registry.conn().await?;
        let rows = conn
            .query(
                "SELECT id, name, base_fee_cents, price_model_json, included_quota_json, \
                        runtime_limits_json, spend_limit_default_cents, archived \
                 FROM zeroship.plans WHERE id = $1",
                &[&id],
            )
            .await?;
        rows.first().map(row_to_plan).transpose()
    }

    /// List every plan (including archived ones) ordered by id.
    pub async fn list(&self) -> Result<Vec<Plan>, RegistryError> {
        let conn = self.registry.conn().await?;
        let rows = conn
            .query(
                "SELECT id, name, base_fee_cents, price_model_json, included_quota_json, \
                        runtime_limits_json, spend_limit_default_cents, archived \
                 FROM zeroship.plans ORDER BY id",
                &[],
            )
            .await?;
        rows.iter().map(row_to_plan).collect()
    }

    /// Insert-or-update a plan (operator / master-key gated at the HTTP layer).
    /// The id is the primary key; an existing id is updated in place (so the
    /// built-in tiers are idempotently re-seeded on every boot). Returns the
    /// written [`Plan`].
    pub async fn upsert(&self, plan: &Plan) -> Result<Plan, RegistryError> {
        let price_model_json = serde_json::to_value(&plan.price.overage)
            .map_err(|e| RegistryError::InvalidInput(format!("price_model_json: {e}")))?;
        let included_quota_json = serde_json::to_value(&plan.price.included)
            .map_err(|e| RegistryError::InvalidInput(format!("included_quota_json: {e}")))?;
        let runtime_limits_json = serde_json::to_value(&plan.runtime)
            .map_err(|e| RegistryError::InvalidInput(format!("runtime_limits_json: {e}")))?;
        let base_fee = i64::try_from(plan.price.base_fee_cents).unwrap_or(i64::MAX);
        let spend_default = i64::try_from(plan.price.spend_limit_default_cents).unwrap_or(i64::MAX);

        let conn = self.registry.conn().await?;
        let rows = conn
            .query(
                "INSERT INTO zeroship.plans \
                   (id, name, base_fee_cents, price_model_json, included_quota_json, \
                    runtime_limits_json, spend_limit_default_cents, archived, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW()) \
                 ON CONFLICT (id) DO UPDATE SET \
                    name = EXCLUDED.name, \
                    base_fee_cents = EXCLUDED.base_fee_cents, \
                    price_model_json = EXCLUDED.price_model_json, \
                    included_quota_json = EXCLUDED.included_quota_json, \
                    runtime_limits_json = EXCLUDED.runtime_limits_json, \
                    spend_limit_default_cents = EXCLUDED.spend_limit_default_cents, \
                    archived = EXCLUDED.archived, \
                    updated_at = NOW() \
                 RETURNING id, name, base_fee_cents, price_model_json, included_quota_json, \
                           runtime_limits_json, spend_limit_default_cents, archived",
                &[
                    &plan.id,
                    &plan.name,
                    &base_fee,
                    &price_model_json,
                    &included_quota_json,
                    &runtime_limits_json,
                    &spend_default,
                    &plan.archived,
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

/// Decode a `plans` row into a [`Plan`]. The JSONB columns come back as
/// `serde_json::Value` (the `with-serde_json-1` driver feature) and deserialize
/// into the pure types.
fn row_to_plan(row: &Row) -> Result<Plan, RegistryError> {
    let base_fee: i64 = row.get("base_fee_cents");
    let spend_default: i64 = row.get("spend_limit_default_cents");
    let price_model_json: serde_json::Value = row.get("price_model_json");
    let included_quota_json: serde_json::Value = row.get("included_quota_json");
    let runtime_limits_json: serde_json::Value = row.get("runtime_limits_json");

    let overage = serde_json::from_value(price_model_json)
        .map_err(|e| RegistryError::Database(format!("plan price_model_json parse: {e}")))?;
    let included = serde_json::from_value(included_quota_json)
        .map_err(|e| RegistryError::Database(format!("plan included_quota_json parse: {e}")))?;
    let runtime: AppRuntimeLimits = serde_json::from_value(runtime_limits_json)
        .map_err(|e| RegistryError::Database(format!("plan runtime_limits_json parse: {e}")))?;

    Ok(Plan {
        id: row.get("id"),
        name: row.get("name"),
        price: PlanPrice {
            base_fee_cents: base_fee.max(0) as u64,
            included,
            overage,
            spend_limit_default_cents: spend_default.max(0) as u64,
        },
        runtime,
        archived: row.get("archived"),
    })
}
