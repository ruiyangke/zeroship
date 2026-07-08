use std::collections::HashMap;
use std::sync::Arc;

use uuid::Uuid;

use super::{BillingPeriod, InvoiceRef, LiteStore, ProviderError, SubjectRef};
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
    async fn owned_app_ids(&self, creator: &Uuid) -> Result<Vec<Uuid>, ProviderError> {
        billing_reconcile::owned_app_ids_for_registry(&self.registry, creator)
            .await
            .map_err(ProviderError::from)
    }

    async fn period_totals(
        &self,
        app: &Uuid,
        period_start: i64,
    ) -> Result<HashMap<String, i64>, ProviderError> {
        let conn = self.registry.conn().await?;
        Metering::period_totals_on(&conn, app, period_start)
            .await
            .map_err(ProviderError::from)
    }

    async fn ensure_customer(
        &self,
        creator: &Uuid,
        _email: &str,
    ) -> Result<Option<SubjectRef>, ProviderError> {
        self.stripe_store
            .get_customer(*creator)
            .await
            .map(|c| c.map(SubjectRef))
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
}
