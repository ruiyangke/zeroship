use std::sync::Arc;

use zeroship_core::organization_id::OrganizationId;

use crate::metering::provider::{
    AggregateQuery, BillingPeriod, Capabilities, CorrectionCapability, DedupContract, DedupKey,
    DedupTtl, IngestAck, InvoiceRef, LiteStore, Meter, MeteringProvider, ProviderCtx,
    ProviderError, SubjectRef, UsageEvent,
};

#[derive(Clone)]
pub struct LiteProvider {
    store: Arc<dyn LiteStore>,
}

impl std::fmt::Debug for LiteProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiteProvider")
            .field("store", &"<lite store>")
            .finish()
    }
}

pub fn factory(ctx: &ProviderCtx) -> Result<Arc<dyn MeteringProvider>, ProviderError> {
    let store = ctx
        .store
        .clone()
        .ok_or_else(|| ProviderError::Config("lite: LiteStore is required".to_string()))?;
    Ok(Arc::new(LiteProvider {
        store,
    }))
}

#[async_trait::async_trait(?Send)]
impl Meter for LiteProvider {
    async fn ingest(&self, batch: &[UsageEvent]) -> Result<IngestAck, ProviderError> {
        self.store.ingest_usage_events(batch).await
    }

    /// `lite` derives usage from the platform's local recompute snapshot
    /// (`usage_aggregates`) and bills from it in `close_period`; it does not
    /// accept forwarded stream events (`ingest` errors by design). Signal that
    /// so the control plane does not spawn a forwarder that would perpetually
    /// error on this provider.
    fn accepts_forwarded_events(&self) -> bool {
        false
    }

    async fn read_aggregate(&self, q: &AggregateQuery) -> Result<u64, ProviderError> {
        let organization = OrganizationId::parse(q.subject.as_str()).map_err(|e| {
            ProviderError::Config(format!("lite: subject is not an organization id: {e}"))
        })?;
        self.store
            .period_meter_units(organization.as_str(), q.period.start, &q.meter)
            .await
    }
}

#[async_trait::async_trait(?Send)]
impl crate::metering::provider::Invoicer for LiteProvider {
    async fn close_period(
        &self,
        subject: &SubjectRef,
        period: BillingPeriod,
    ) -> Result<InvoiceRef, ProviderError> {
        let organization = OrganizationId::parse(subject.as_str()).map_err(|e| {
            ProviderError::Config(format!("lite: subject is not an organization id: {e}"))
        })?;
        self.store.close_period_invoice(organization.as_str(), period).await
    }

    async fn adjustment_note(
        &self,
        subject: &SubjectRef,
        note: &crate::metering::provider::AdjustmentNote,
    ) -> Result<InvoiceRef, ProviderError> {
        let organization = OrganizationId::parse(subject.as_str()).map_err(|e| {
            ProviderError::Config(format!("lite: subject is not an organization id: {e}"))
        })?;
        self.store.adjustment_note_invoice(organization.as_str(), note).await
    }
}

impl MeteringProvider for LiteProvider {
    fn id(&self) -> &str {
        "lite"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::METER | Capabilities::INVOICE
    }

    fn as_meter(&self) -> Option<&dyn Meter> {
        Some(self)
    }

    fn as_invoicer(&self) -> Option<&dyn crate::metering::provider::Invoicer> {
        Some(self)
    }

    fn production_ready(&self) -> bool {
        false
    }

    fn owns_local_invoice(&self) -> bool {
        true
    }

    fn dedup(&self) -> DedupContract {
        DedupContract {
            key: DedupKey::SourceAndId,
            ttl: DedupTtl::Unbounded,
        }
    }

    fn correction(&self) -> CorrectionCapability {
        CorrectionCapability::InvoiceCredit
    }
}
