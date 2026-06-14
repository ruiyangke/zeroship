//! The Native metering provider (default): the CURRENT billing pipeline behind
//! the [`MeteringProvider`](super::MeteringProvider) trait. ZERO behaviour
//! change — every verb DELEGATES to the existing, hardened code:
//!
//! - `ensure_customer` → the existing `billing_setup` Customer path
//!   (`StripeApi::create_customer` + `StripeStore` upsert).
//! - `report_usage`    → NO-OP. Usage already lands in `usage_aggregates` via
//!   the worker→control ingest; Native enforcement + invoicing both read the
//!   LOCAL ledger, so there is nothing to forward (this is the concrete
//!   statement of "metering is never outsourced").
//! - `invoice`         → the existing `billing_reconcile::bill_creator` body,
//!   REUSED VERBATIM (claim-then-call C1, persist-draft-before-finalize C2, the
//!   `find_invoice_item_by_key` adoption path, the per-app `charge_cents`
//!   pricing). The hardened crash-window logic is NOT touched; it is relocated
//!   behind this verb by calling into it.
//! - `handle_webhook`  → the existing Stripe webhook path stays in
//!   `stripe_handlers::webhook` (the verified HTTP ingest); on the Native rail
//!   the provider verb is a no-op seam (M-Native).
//!
//! The fleet reconcile sweep itself (`billing_reconcile::tick_with` / `sweep`)
//! is UNCHANGED — it remains the seam the cron and the on-demand
//! `/internal/billing/reconcile` endpoint drive, and the regression-gate
//! integration suite injects its `StripeApi` there. Native's per-creator
//! `invoice` verb reuses the SAME `bill_creator` those tests exercise.

use uuid::Uuid;

use super::types::{
    BillingPeriod, CreatorBilling, CustomerRef, InvoiceRef, MeteringProviderKind, ProviderError,
};
use super::MeteringProvider;
use crate::cron::billing_reconcile;
use crate::metering::Metering;
use crate::plan_catalog::PlanCatalog;
use crate::pricing_store::PricingStore;
use crate::stripe_client::StripeClient;
use crate::AppState;

/// The default provider. A ZST: it owns no state — every verb is driven by the
/// `&AppState` handed in (so there is no `AppState`→provider→`AppState` cycle).
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeProvider;

impl NativeProvider {
    /// Construct the Native provider.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Build the production `cyper` Stripe client from the deployment's Stripe
    /// config — IDENTICAL to what `billing_reconcile::tick` and
    /// `stripe_handlers::billing_setup` build today.
    fn stripe(state: &AppState) -> StripeClient {
        StripeClient::new(crate::SecretString::new(
            state.stripe_secret_key.expose_secret().to_string(),
        ))
        .with_base_url(state.stripe_base_url.clone())
    }
}

#[async_trait::async_trait(?Send)]
impl MeteringProvider for NativeProvider {
    fn kind(&self) -> MeteringProviderKind {
        MeteringProviderKind::Native
    }

    async fn ensure_customer(
        &self,
        state: &AppState,
        creator: &CreatorBilling,
    ) -> Result<CustomerRef, ProviderError> {
        // Reuse the existing `billing_setup` Customer path: return a saved
        // `cus_…` if present, else create one and upsert it. Idempotent.
        if let Some(existing) = &creator.customer {
            return Ok(existing.clone());
        }
        match state.stripe_store.get_customer(creator.creator_id).await {
            Ok(Some(c)) => Ok(CustomerRef(c)),
            Ok(None) => {
                let stripe = Self::stripe(state);
                let cus = crate::stripe_client::StripeApi::create_customer(
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
        _customer: &CustomerRef,
        _app_id: Uuid,
        _period: BillingPeriod,
        _compute_units: u64,
        _idempotency_key: &str,
    ) -> Result<(), ProviderError> {
        // NO-OP on the Native rail: usage is already local (usage_aggregates).
        Ok(())
    }

    async fn invoice(
        &self,
        state: &AppState,
        creator: &CreatorBilling,
        period: BillingPeriod,
    ) -> Result<InvoiceRef, ProviderError> {
        // Reuse the EXISTING reconciler per-creator path verbatim. The per-tick
        // setup (catalog, global weights, default FX) mirrors `sweep` exactly;
        // `bill_creator` carries the hardened C1/C2 + MAJOR crash-window logic
        // unchanged.
        let stripe = Self::stripe(state);
        let metering = Metering::new(state.registry.clone());
        let catalog = PlanCatalog::new(state.registry.clone());
        let pricing = PricingStore::new(state.registry.clone());
        let weights = pricing.weights().await?;
        let default_fx = pricing.default_fx_pico_cents_per_unit().await?;

        let app_ids = billing_reconcile::owned_app_ids(state, &creator.creator_id).await?;

        let billed = billing_reconcile::bill_creator(
            state,
            &stripe,
            &metering,
            &catalog,
            &weights,
            default_fx,
            &creator.creator_id,
            &app_ids,
            period.start,
        )
        .await?;

        // `bill_creator` returns true when a fresh invoice was finalized this
        // call. The finalized invoice id lives on `billing_runs`; surface it so
        // a caller can audit the close. A no-op (already billed / nothing to
        // bill / no customer) yields `InvoiceRef(None)`.
        if billed {
            let invoice_id = lookup_invoice_id(state, &creator.creator_id, period.start)
                .await
                .map_err(ProviderError::Registry)?;
            Ok(InvoiceRef(invoice_id))
        } else {
            Ok(InvoiceRef(None))
        }
    }

    async fn handle_webhook(
        &self,
        _state: &AppState,
        _payload: &[u8],
        _sig: &str,
    ) -> Result<(), ProviderError> {
        // M-Native: the verified Stripe webhook ingest stays in
        // `stripe_handlers::webhook` (the HTTP handler with the signature gate +
        // rate limit). This verb is the seam an export backend will use; on the
        // Native rail it is a no-op.
        Ok(())
    }
}

/// Read back the finalized Stripe invoice id `bill_creator` persisted on the
/// `billing_runs` row for `(creator, period)`. `None` if no finalized invoice
/// (the run is still in-flight or absent).
async fn lookup_invoice_id(
    state: &AppState,
    creator_id: &Uuid,
    period_start: i64,
) -> Result<Option<String>, crate::registry::RegistryError> {
    use chrono::TimeZone;
    let period_ts = chrono::Utc
        .timestamp_opt(period_start, 0)
        .single()
        .ok_or_else(|| {
            crate::registry::RegistryError::Database(format!("invalid period_start {period_start}"))
        })?;
    let conn = state.registry.conn().await?;
    let rows = conn
        .query(
            "SELECT stripe_invoice_id FROM zeroship.billing_runs \
             WHERE creator_id = $1 AND period_start = $2",
            &[creator_id, &period_ts],
        )
        .await?;
    Ok(rows
        .first()
        .and_then(|r| r.get::<_, Option<String>>("stripe_invoice_id")))
}
