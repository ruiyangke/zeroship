//! The Stripe (Billing Meters) metering provider (M-Stripe). Stripe owns
//! aggregation + invoicing; the platform only PUSHES compute units.
//!
//! - `ensure_customer` → the SAME platform Customer path the Native rail uses
//!   (`StripeApi::create_customer` + `StripeStore` upsert) — the `cus_…` the
//!   meter aggregates by.
//! - `report_usage`    → `StripeApi::create_meter_event` pushing the CU delta
//!   onto the operator-provisioned Stripe Meter (`event_name`). The CU `value`
//!   is the DELTA the export cron computed (CU consumed since the last export);
//!   the `identifier` is the deterministic dedup key. Stripe sums meter events,
//!   so we never push the cumulative total here.
//! - `invoice`         → **NO-OP** (`InvoiceRef(None)`). Stripe self-invoices
//!   from the operator-provisioned metered Price + Subscription on its OWN
//!   billing cycle. The provider assumes the Price/Subscription exist; it does
//!   NOT create them. (The `billing_reconcile` cron is NOT spawned under stripe.)
//! - `handle_webhook`  → reuses the existing verified Stripe webhook path that
//!   stays in `stripe_handlers::webhook` (the HTTP handler with the signature
//!   gate + rate limit). On the Stripe-Meters rail v1 adds no new mutation
//!   (`invoice.payment_failed` is still audited there); this verb is the seam.
//!
//! The FX on this rail is Stripe's metered Price (NOT the plan `fx`): we push
//! raw CU and Stripe prices it. The LOCAL spend cap still enforces on our `fx`
//! (`spend.rs` reads `usage_aggregates` directly, provider-independent), so the
//! provider NEVER touches enforcement — it is export/invoice only.

use uuid::Uuid;

use super::types::{
    BillingPeriod, CreatorBilling, CustomerRef, InvoiceRef, MeteringProviderKind, ProviderError,
};
use super::{MeteringProvider, StripeMeterConfig};
use crate::stripe_client::{StripeApi, StripeClient};
use crate::AppState;

/// The Stripe Billing Meters provider. Holds the meter config (event name +
/// Stripe creds); every verb that talks to Stripe builds the `cyper` client
/// from that config — the SAME idiom as `NativeProvider::stripe`.
#[derive(Debug, Clone)]
pub struct StripeProvider {
    meter: StripeMeterConfig,
}

impl StripeProvider {
    /// Construct from the operator-provisioned meter config.
    #[must_use]
    pub fn new(meter: StripeMeterConfig) -> Self {
        Self { meter }
    }

    /// Build the production `cyper` Stripe client from the meter config. The
    /// base URL is overridable so the integration tests point the REAL client
    /// at a localhost mock-meters server.
    fn stripe(&self) -> StripeClient {
        StripeClient::new(crate::SecretString::new(
            self.meter.secret_key.expose_secret().to_string(),
        ))
        .with_base_url(self.meter.base_url.clone())
    }

    /// The Stripe Meter's configured `event_name` (e.g. `compute_units`).
    #[must_use]
    pub fn event_name(&self) -> &str {
        &self.meter.event_name
    }
}

#[async_trait::async_trait(?Send)]
impl MeteringProvider for StripeProvider {
    fn kind(&self) -> MeteringProviderKind {
        MeteringProviderKind::Stripe
    }

    async fn ensure_customer(
        &self,
        state: &AppState,
        creator: &CreatorBilling,
    ) -> Result<CustomerRef, ProviderError> {
        // Same platform Customer path as the Native rail: return a saved cus_…
        // if present, else create + upsert one. The meter aggregates by this id.
        if let Some(existing) = &creator.customer {
            return Ok(existing.clone());
        }
        match state.stripe_store.get_customer(creator.creator_id).await {
            Ok(Some(c)) => Ok(CustomerRef(c)),
            Ok(None) => {
                let stripe = self.stripe();
                let cus = StripeApi::create_customer(
                    &stripe,
                    &creator.email,
                    &creator.creator_id.to_string(),
                )
                .await?;
                state
                    .stripe_store
                    .set_customer(creator.creator_id, &cus)
                    .await?;
                Ok(CustomerRef(cus))
            }
            Err(e) => Err(ProviderError::Stripe(e)),
        }
    }

    async fn report_usage(
        &self,
        _state: &AppState,
        customer: &CustomerRef,
        _app_id: Uuid,
        period: BillingPeriod,
        compute_units: u64,
        idempotency_key: &str,
    ) -> Result<(), ProviderError> {
        // Push the CU DELTA onto the Stripe Meter. The export cron has already
        // computed `compute_units` as (current cumulative − last exported), so
        // this is the consumed-since-last-export quantity Stripe will SUM.
        // `idempotency_key` is the deterministic per-window dedup `identifier`.
        // Stamp the event at the period end (the billed window's close).
        let stripe = self.stripe();
        stripe
            .create_meter_event(
                &self.meter.event_name,
                customer.as_str(),
                compute_units,
                idempotency_key,
                period.end,
            )
            .await?;
        Ok(())
    }

    async fn invoice(
        &self,
        _state: &AppState,
        _creator: &CreatorBilling,
        _period: BillingPeriod,
    ) -> Result<InvoiceRef, ProviderError> {
        // NO-OP: Stripe self-invoices from the operator-provisioned metered
        // Price + Subscription against the pushed meter events. We never create
        // an invoice on this rail (and `spawn_all` does not spawn the
        // billing_reconcile cron under stripe).
        Ok(InvoiceRef(None))
    }

    async fn handle_webhook(
        &self,
        _state: &AppState,
        _payload: &[u8],
        _sig: &str,
    ) -> Result<(), ProviderError> {
        // The verified Stripe webhook ingest stays in `stripe_handlers::webhook`
        // (signature gate + rate limit). Stripe-Meters adds no new mutation in
        // v1; this verb is the seam an export backend will use.
        Ok(())
    }
}
