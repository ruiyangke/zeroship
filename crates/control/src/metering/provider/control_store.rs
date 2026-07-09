use std::sync::Arc;

use uuid::Uuid;

use super::{
    AdjustmentNote, BillingPeriod, IngestAck, InvoiceRef, LiteStore, ProviderError, UsageEvent,
};
use crate::cron::billing_reconcile;
use crate::metering::Metering;
use crate::plan_catalog::PlanCatalog;
use crate::pricing::{billable_units, total_units};
use crate::pricing_store::PricingStore;
use crate::registry::Registry;
use crate::stripe_client::StripeClient;
use crate::stripe_store::StripeStore;

pub struct ControlLiteStore {
    registry: Registry,
    stripe_store: StripeStore,
    stripe_secret_key: crate::SecretString,
    stripe_base_url: String,
    tax_provider: Arc<dyn crate::tax::TaxProvider>,
}

impl std::fmt::Debug for ControlLiteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlLiteStore")
            .field("registry", &self.registry)
            .field("stripe_store", &"<stripe store>")
            .field("stripe_secret_key", &self.stripe_secret_key)
            .field("stripe_base_url", &self.stripe_base_url)
            .field("tax_provider", &"<tax provider>")
            .finish()
    }
}

impl ControlLiteStore {
    #[must_use]
    pub fn new(
        registry: Registry,
        stripe_store: StripeStore,
        stripe_secret_key: crate::SecretString,
        stripe_base_url: String,
        tax_provider: Arc<dyn crate::tax::TaxProvider>,
    ) -> Self {
        Self {
            registry,
            stripe_store,
            stripe_secret_key,
            stripe_base_url,
            tax_provider,
        }
    }

    fn stripe(&self) -> StripeClient {
        StripeClient::new(crate::SecretString::new(
            self.stripe_secret_key.expose_secret().to_string(),
        ))
        .with_base_url(self.stripe_base_url.clone())
    }
}

#[async_trait::async_trait(?Send)]
impl LiteStore for ControlLiteStore {
    async fn ingest_usage_events(&self, _batch: &[UsageEvent]) -> Result<IngestAck, ProviderError> {
        Err(ProviderError::Store(
            "lite: stream ingest requires a LiteStore event sink; ControlLiteStore uses the recompute snapshot path"
                .to_string(),
        ))
    }

    async fn owned_app_ids(&self, creator: &Uuid) -> Result<Vec<Uuid>, ProviderError> {
        billing_reconcile::owned_app_ids_for_registry(&self.registry, creator)
            .await
            .map_err(ProviderError::from)
    }

    async fn period_billable_units(
        &self,
        creator: &Uuid,
        period_start: i64,
    ) -> Result<u64, ProviderError> {
        let conn = self.registry.conn().await?;
        let app_ids = self.owned_app_ids(creator).await?;
        let pricing = PricingStore::new(self.registry.clone());
        let weights = pricing.weights().await?;
        let catalog = PlanCatalog::new(self.registry.clone());

        let mut units = 0u64;
        for app_id in app_ids {
            let usage = Metering::period_totals_on(&conn, &app_id, period_start).await?;
            let app_units = match billing_reconcile::lookup_plan_id_on(&conn, &app_id).await? {
                Some(plan_id) => match catalog.get(&plan_id).await? {
                    Some(plan) => billable_units(&plan.price, &usage, &weights).map_err(|e| {
                        ProviderError::Store(format!("lite: compute-unit pricing failed: {e}"))
                    })?,
                    None => continue,
                },
                None => total_units(&weights, &usage).map_err(|e| {
                    ProviderError::Store(format!("lite: compute-unit pricing failed: {e}"))
                })?,
            };
            units = units.saturating_add(app_units);
        }
        Ok(units)
    }

    async fn close_period_invoice(
        &self,
        creator: &Uuid,
        period: BillingPeriod,
    ) -> Result<InvoiceRef, ProviderError> {
        let stripe = self.stripe();
        let catalog = PlanCatalog::new(self.registry.clone());
        let pricing = PricingStore::new(self.registry.clone());
        let weights = pricing.weights().await?;
        let default_fx = pricing.default_fx_pico_cents_per_unit().await?;
        let app_ids = self.owned_app_ids(creator).await?;

        let billed = billing_reconcile::bill_creator_with_parts(
            &self.registry,
            &self.stripe_store,
            self.tax_provider.as_ref(),
            &stripe,
            &catalog,
            &weights,
            default_fx,
            creator,
            &app_ids,
            period.start,
        )
        .await?;
        if billed {
            let invoice_id =
                billing_reconcile::lookup_invoice_id_for_registry(&self.registry, creator, period.start)
                    .await?;
            Ok(InvoiceRef(invoice_id))
        } else {
            Ok(InvoiceRef(None))
        }
    }

    async fn adjustment_note_invoice(
        &self,
        creator: &Uuid,
        note: &AdjustmentNote,
    ) -> Result<InvoiceRef, ProviderError> {
        let app_id = note.app_id.ok_or_else(|| {
            ProviderError::Config(
                "adjustment_note requires an app_id for local invoice_lines bookkeeping"
                    .to_string(),
            )
        })?;
        let conn = self.registry.conn().await?;
        let adjustment_period = crate::metering::period_date(note.period.end);
        let invoice_id = match conn
            .query(
                "SELECT id FROM zeroship.invoices \
                 WHERE creator_id = $1 AND period = $2::date AND status <> 'void'",
                &[creator, &adjustment_period],
            )
            .await?
            .first()
            .map(|r| r.get::<_, String>("id"))
        {
            Some(id) => id,
            None => {
                let new_id = zeroship_core::typed_id::new_invoice_id();
                conn.execute(
                    "INSERT INTO zeroship.invoices (id, creator_id, period, status) \
                     VALUES ($1, $2, $3::date, 'draft') \
                     ON CONFLICT (creator_id, period) WHERE status <> 'void' DO NOTHING",
                    &[&new_id, creator, &adjustment_period],
                )
                .await?;
                conn.query(
                    "SELECT id FROM zeroship.invoices \
                     WHERE creator_id = $1 AND period = $2::date AND status <> 'void'",
                    &[creator, &adjustment_period],
                )
                .await?
                .first()
                .map(|r| r.get::<_, String>("id"))
                .ok_or_else(|| {
                    ProviderError::Store(
                        "adjustment_note invoice claim vanished after insert".to_string(),
                    )
                })?
            }
        };

        let status = conn
            .query(
                "SELECT status FROM zeroship.invoices WHERE id = $1",
                &[&invoice_id],
            )
            .await?
            .first()
            .map(|r| r.get::<_, String>("status"))
            .unwrap_or_default();
        if status == "finalized" {
            return Err(ProviderError::Store(format!(
                "adjustment invoice {invoice_id} is already finalized"
            )));
        }

        let plan_id = conn
            .query("SELECT plan_id FROM zeroship.apps WHERE id = $1", &[&app_id])
            .await?
            .first()
            .map(|r| r.get::<_, String>("plan_id"))
            .ok_or_else(|| {
                ProviderError::Store(format!(
                    "adjustment_note app {app_id} has no current plan"
                ))
            })?;
        let segment_no_u32 = (i16::MAX as u32)
            .checked_sub(note.correction_seq)
            .ok_or_else(|| {
                ProviderError::Config(format!(
                    "correction_seq {} exceeds invoice line segment range",
                    note.correction_seq
                ))
            })?;
        let segment_no = i16::try_from(segment_no_u32).map_err(|_| {
            ProviderError::Config(format!(
                "correction_seq {} exceeds invoice line segment range",
                note.correction_seq
            ))
        })?;
        let line_kind = if note.amount_cents > 0 {
            "debit_note"
        } else if note.amount_cents < 0 {
            "credit_note"
        } else {
            return Err(ProviderError::Config(
                "adjustment_note amount_cents must be non-zero".to_string(),
            ));
        };
        let usage = serde_json::json!({
            "kind": "billing_correction",
            "period_start": note.period.start,
            "period_end": note.period.end,
            "quantity_delta": note.quantity_delta,
            "correction_seq": note.correction_seq,
            "reason": note.reason,
        });
        let weights = serde_json::json!({});
        conn.execute(
            "INSERT INTO zeroship.invoice_lines \
               (invoice_id, app_id, segment_no, plan_id, included_units, \
                fx_pico_cents_per_unit, base_fee_cents, amount_cents, \
                usage_snapshot, weights_snapshot, line_kind, correction_dedup_key) \
             VALUES ($1, $2, $3, $4, 0, 1000, 0, $5, $6, $7, $8, $9) \
             ON CONFLICT (correction_dedup_key) WHERE correction_dedup_key IS NOT NULL \
             DO NOTHING",
            &[
                &invoice_id,
                &app_id,
                &segment_no,
                &plan_id,
                &note.amount_cents,
                &usage,
                &weights,
                &line_kind,
                &note.idempotency_key,
            ],
        )
        .await?;
        Ok(InvoiceRef(Some(invoice_id)))
    }
}
