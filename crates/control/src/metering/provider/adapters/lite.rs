use std::sync::Arc;

use uuid::Uuid;

use crate::metering::provider::{
    AggregateQuery, BillingPeriod, Capabilities, CorrectionCapability, DedupContract, DedupKey,
    DedupTtl, IngestAck, InvoiceRef, LineItem, LiteStore, Meter, MeteringProvider, ProviderCtx,
    ProviderError, RatedInput, Rater, Subject, SubjectRef, UsageEvent,
};

#[derive(Clone)]
pub struct LiteProvider {
    store: Arc<dyn LiteStore>,
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

    async fn read_aggregate(&self, q: &AggregateQuery) -> Result<u64, ProviderError> {
        let creator = Uuid::parse_str(q.subject.as_str()).map_err(|e| {
            ProviderError::Config(format!("lite: subject is not a creator UUID: {e}"))
        })?;
        self.store
            .period_billable_units(&creator, q.period.start)
            .await
    }

    async fn ensure_subject(&self, subject: &Subject) -> Result<SubjectRef, ProviderError> {
        if let Some(existing) = &subject.customer {
            return Ok(existing.clone());
        }
        Ok(SubjectRef(subject.creator_id.to_string()))
    }
}

#[async_trait::async_trait(?Send)]
impl Rater for LiteProvider {
    async fn rate(
        &self,
        _subject: &SubjectRef,
        _period: BillingPeriod,
        input: &RatedInput,
    ) -> Result<Vec<LineItem>, ProviderError> {
        // The existing reconciler does per-app plan and proration pricing inside
        // close_period. The S1 rater surface is present for role composition and
        // will carry first-class line generation when the stream recompute lands.
        Ok(vec![LineItem {
            app_id: None,
            description: "Infra usage".to_string(),
            amount_cents: 0,
            quantity: input.units,
            metadata: std::collections::BTreeMap::new(),
        }])
    }
}

#[async_trait::async_trait(?Send)]
impl crate::metering::provider::Invoicer for LiteProvider {
    async fn close_period(
        &self,
        subject: &SubjectRef,
        period: BillingPeriod,
        _lines: &[LineItem],
    ) -> Result<InvoiceRef, ProviderError> {
        let creator = Uuid::parse_str(subject.as_str()).map_err(|e| {
            ProviderError::Config(format!("lite: subject is not a creator UUID: {e}"))
        })?;
        self.store.close_period_invoice(&creator, period).await
    }

    async fn adjustment_note(
        &self,
        subject: &SubjectRef,
        note: &crate::metering::provider::AdjustmentNote,
    ) -> Result<InvoiceRef, ProviderError> {
        let creator = Uuid::parse_str(subject.as_str()).map_err(|e| {
            ProviderError::Config(format!("lite: subject is not a creator UUID: {e}"))
        })?;
        self.store.adjustment_note_invoice(&creator, note).await
    }
}

impl MeteringProvider for LiteProvider {
    fn id(&self) -> &str {
        "lite"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::METER | Capabilities::RATE | Capabilities::INVOICE
    }

    fn as_meter(&self) -> Option<&dyn Meter> {
        Some(self)
    }

    fn as_rater(&self) -> Option<&dyn Rater> {
        Some(self)
    }

    fn as_invoicer(&self) -> Option<&dyn crate::metering::provider::Invoicer> {
        Some(self)
    }

    fn production_ready(&self) -> bool {
        false
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
