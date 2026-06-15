//! The OpenMeter metering provider (M-OpenMeter). OpenMeter is an *export-only*
//! aggregation sink: the platform PUSHES compute units as **CloudEvents** and
//! READS the per-subject aggregate back. OpenMeter NEVER invoices — billing
//! stays on the Native (or Stripe) rail; on this rail OpenMeter just aggregates.
//!
//! Structurally a sibling of [`super::stripe_meters::StripeProvider`]; the verbs
//! differ only in WHERE the CU goes:
//!
//! ## Subject mapping — the provider-generic seam
//!
//! OpenMeter has no customer object; it aggregates by the CloudEvent `subject`.
//! zeroship maps the subject to the **creator's customer handle** — the SAME
//! per-creator id the export cron already resolves (`CustomerRef`, a `cus_…` for
//! the standard "OpenMeter export + Native/Stripe invoicing" deployment). Using
//! the cron's existing per-creator handle keeps the cron 100% unchanged AND
//! guarantees `report_usage` (push) and `reported_total` (read) address the
//! EXACT same subject — which is what makes the inherited C2 reconcile correct.
//! The export pushes per CREATOR (the meter aggregates per subject), so one
//! CloudEvent carries the creator's whole billable-CU delta — there is no single
//! app to attribute.
//!
//! - `ensure_customer` → **map / no-op**. Returns the creator's existing handle
//!   (never talks to OpenMeter — there is no customer object to create).
//! - `report_usage`    → push ONE CloudEvent carrying the CU DELTA the export
//!   cron computed, under the deterministic `identifier` as the CloudEvent `id`
//!   (OpenMeter dedupes on `(source, id)`). The event `subject` is the creator
//!   handle; `time` is the cron's `now` (C1: the consumption instant, never the
//!   future `period.end`).
//! - `reported_total`  → query OpenMeter's AGGREGATE for the `(subject, period)`
//!   window — the SAME subject `report_usage` pushed under. The cron pushes
//!   `current_local − this`, so a crash-then-re-drive PAST OpenMeter's dedup
//!   window pushes only the still-missing remainder — the inherited C2 reconcile,
//!   riding on OpenMeter's own aggregate.
//! - `invoice`         → **NO-OP** (`InvoiceRef(None)`). OpenMeter aggregates,
//!   it does not invoice. (`spawn_all` spawns `metering_export`, NOT
//!   `billing_reconcile`, under openmeter — the M5 table.)
//! - `handle_webhook`  → no-op (no inbound billing events on this rail).
//!
//! The LOCAL spend cap still enforces on the plan `fx` (`spend.rs` reads
//! `usage_aggregates` directly, provider-independent), so the provider NEVER
//! touches enforcement — it is export only.

use super::types::{
    BillingPeriod, CreatorBilling, CustomerRef, InvoiceRef, MeteringProviderKind, ProviderError,
};
use super::{MeteringProvider, OpenMeterConfig};
use crate::openmeter_client::{CloudEvent, OpenMeterApi, OpenMeterClient};
use crate::AppState;

/// The OpenMeter provider. Holds the deployment's OpenMeter config (base URL +
/// token + meter slug + event type); every verb that talks to OpenMeter builds
/// the `cyper` client from that config — the SAME idiom as `StripeProvider`.
#[derive(Debug, Clone)]
pub struct OpenMeterProvider {
    config: OpenMeterConfig,
}

impl OpenMeterProvider {
    /// Construct from the operator-provisioned OpenMeter config.
    #[must_use]
    pub fn new(config: OpenMeterConfig) -> Self {
        Self { config }
    }

    /// Build the production `cyper` OpenMeter client from the config. The base
    /// URL is overridable so the integration tests point the REAL client at a
    /// localhost mock-OpenMeter server.
    fn client(&self) -> OpenMeterClient {
        OpenMeterClient::new(crate::SecretString::new(
            self.config.token.expose_secret().to_string(),
        ))
        .with_base_url(self.config.base_url.clone())
    }

    /// The OpenMeter meter slug the aggregate is queried under.
    #[must_use]
    pub fn meter_slug(&self) -> &str {
        &self.config.meter_slug
    }

    /// The CloudEvent `type` (the meter's configured `eventType`).
    #[must_use]
    pub fn event_type(&self) -> &str {
        &self.config.event_type
    }

}

#[async_trait::async_trait(?Send)]
impl MeteringProvider for OpenMeterProvider {
    fn kind(&self) -> MeteringProviderKind {
        MeteringProviderKind::OpenMeter
    }

    async fn ensure_customer(
        &self,
        _state: &AppState,
        creator: &CreatorBilling,
    ) -> Result<CustomerRef, ProviderError> {
        // No-op: OpenMeter has no customer object; it keys on the CloudEvent
        // `subject`, which zeroship maps to the creator's existing handle. Return
        // the saved handle if present, else the creator id as a stable subject.
        if let Some(existing) = &creator.customer {
            return Ok(existing.clone());
        }
        Ok(CustomerRef(creator.creator_id.to_string()))
    }

    async fn report_usage(
        &self,
        _state: &AppState,
        customer: &CustomerRef,
        period: BillingPeriod,
        compute_units: u64,
        idempotency_key: &str,
        now: i64,
    ) -> Result<(), ProviderError> {
        // Push ONE CloudEvent carrying the CU DELTA (the export cron has already
        // computed `compute_units` as the still-missing remainder — the creator's
        // current billable CU minus OpenMeter's per-subject aggregate / high-water).
        // OpenMeter SUMS events per subject, so we never push the cumulative total
        // here.
        //
        // The CloudEvent `subject` is the cron's per-creator handle (the SAME id
        // `reported_total` queries the aggregate under). The push is per CREATOR —
        // the meter aggregates per subject, so there is no single app to attribute.
        // C1: stamp `time` at `now` — the CONSUMPTION instant — NOT `period.end`.
        // The CloudEvent `id` is the deterministic dedup identifier so a transport
        // retry / same-window re-push replays.
        let event = CloudEvent {
            id: idempotency_key.to_string(),
            event_type: self.config.event_type.clone(),
            subject: customer.as_str().to_string(),
            time_unix: now,
            value: compute_units,
            period_start_unix: period.start,
        };
        self.client().ingest_event(&event).await
    }

    async fn reported_total(
        &self,
        _state: &AppState,
        customer: &CustomerRef,
        period: BillingPeriod,
    ) -> Result<u64, ProviderError> {
        // C2: read OpenMeter's AGGREGATED value for this (subject, period). The
        // cron pushes `current − this`, so a re-drive past OpenMeter's dedup
        // window pushes only the still-missing remainder instead of double-
        // counting. The guarantee rides on OpenMeter's aggregate, never on the
        // (by-construction-stale) local high-water.
        //
        // The subject is the cron's per-creator `customer` handle — the SAME id
        // `report_usage` pushed the CloudEvent under, so the read window matches
        // exactly what was pushed.
        self.client()
            .meter_query(
                &self.config.meter_slug,
                customer.as_str(),
                period.start,
                period.end,
            )
            .await
    }

    async fn invoice(
        &self,
        _state: &AppState,
        _creator: &CreatorBilling,
        _period: BillingPeriod,
    ) -> Result<InvoiceRef, ProviderError> {
        // NO-OP: OpenMeter aggregates, it does not invoice. Billing stays on the
        // Native/Stripe rail; an operator running OpenMeter consumes its
        // aggregates out-of-band (or pairs it with Native invoicing). `spawn_all`
        // does not spawn the billing_reconcile cron under openmeter.
        Ok(InvoiceRef(None))
    }

    async fn handle_webhook(
        &self,
        _state: &AppState,
        _payload: &[u8],
        _sig: &str,
    ) -> Result<(), ProviderError> {
        // No inbound billing events on the OpenMeter rail.
        Ok(())
    }
}
