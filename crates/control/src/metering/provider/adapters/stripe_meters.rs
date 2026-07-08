use std::sync::Arc;
use std::time::Duration;

use crate::metering::provider::{
    AggregateQuery, BillingPeriod, Capabilities, CorrectionCapability, DedupContract, DedupKey,
    DedupTtl, IngestAck, InvoiceRef, LineItem, Meter, MeteringProvider, ProviderCtx,
    ProviderError, Rater, RatedInput, SecretHandle, Subject, SubjectRef, UsageEvent, WebhookEvent,
    WebhookOutcome, WebhookSink,
};
use crate::stripe_client::{StripeApi, StripeClient};

#[derive(Debug, serde::Deserialize)]
struct StripeMetersCfg {
    event_name: String,
    meter_id: String,
    secret_key: SecretHandle,
    #[serde(default = "default_stripe_base_url")]
    base_url: String,
}

fn default_stripe_base_url() -> String {
    "https://api.stripe.com".to_string()
}

pub struct StripeMetersProvider {
    event_name: String,
    meter_id: String,
    secret_key: crate::SecretString,
    base_url: String,
}

pub fn factory(ctx: &ProviderCtx) -> Result<Arc<dyn MeteringProvider>, ProviderError> {
    let cfg: StripeMetersCfg = ctx.parse_adapter_config("stripe_meters")?;
    let secret_key = ctx.secrets.resolve(&cfg.secret_key)?;
    if cfg.event_name.trim().is_empty() {
        return Err(ProviderError::Config(
            "stripe_meters: event_name required — refusing to boot".to_string(),
        ));
    }
    if cfg.meter_id.trim().is_empty() {
        return Err(ProviderError::Config(
            "stripe_meters: meter_id required for aggregate read-back".to_string(),
        ));
    }
    if secret_key.expose_secret().trim().is_empty() {
        return Err(ProviderError::Config(
            "stripe_meters: secret_key resolved empty".to_string(),
        ));
    }
    Ok(Arc::new(StripeMetersProvider {
        event_name: cfg.event_name,
        meter_id: cfg.meter_id,
        secret_key,
        base_url: cfg.base_url,
    }))
}

impl StripeMetersProvider {
    fn stripe(&self) -> StripeClient {
        StripeClient::new(crate::SecretString::new(
            self.secret_key.expose_secret().to_string(),
        ))
        .with_base_url(self.base_url.clone())
    }
}

#[async_trait::async_trait(?Send)]
impl Meter for StripeMetersProvider {
    async fn ingest(&self, batch: &[UsageEvent]) -> Result<IngestAck, ProviderError> {
        let stripe = self.stripe();
        for event in batch {
            stripe
                .create_meter_event(
                    &self.event_name,
                    event.subject.as_str(),
                    event.value,
                    &event.event_id,
                    event.time_unix,
                )
                .await?;
        }
        Ok(IngestAck {
            accepted: batch.len(),
            deduped: 0,
        })
    }

    async fn read_aggregate(&self, q: &AggregateQuery) -> Result<u64, ProviderError> {
        Ok(self
            .stripe()
            .meter_event_summary(
                &self.meter_id,
                q.subject.as_str(),
                q.period.start,
                q.period.end,
            )
            .await?)
    }

    async fn ensure_subject(&self, subject: &Subject) -> Result<SubjectRef, ProviderError> {
        if let Some(existing) = &subject.customer {
            return Ok(existing.clone());
        }
        Ok(SubjectRef(subject.creator_id.to_string()))
    }
}

#[async_trait::async_trait(?Send)]
impl Rater for StripeMetersProvider {
    async fn rate(
        &self,
        _subject: &SubjectRef,
        _period: BillingPeriod,
        _input: &RatedInput,
    ) -> Result<Vec<LineItem>, ProviderError> {
        Ok(Vec::new())
    }
}

#[async_trait::async_trait(?Send)]
impl crate::metering::provider::Invoicer for StripeMetersProvider {
    async fn close_period(
        &self,
        _subject: &SubjectRef,
        _period: BillingPeriod,
        _lines: &[LineItem],
    ) -> Result<InvoiceRef, ProviderError> {
        Ok(InvoiceRef(None))
    }

    async fn adjustment_note(
        &self,
        _subject: &SubjectRef,
        _note: &crate::metering::provider::AdjustmentNote,
    ) -> Result<InvoiceRef, ProviderError> {
        Err(ProviderError::Config(
            "stripe_meters: adjustment notes are not implemented in S1".to_string(),
        ))
    }
}

#[async_trait::async_trait(?Send)]
impl WebhookSink for StripeMetersProvider {
    fn verify(&self, _payload: &[u8], _sig: &str) -> Result<(), ProviderError> {
        Ok(())
    }

    async fn handle(&self, _event: WebhookEvent) -> Result<WebhookOutcome, ProviderError> {
        Ok(WebhookOutcome::Ignored)
    }
}

impl MeteringProvider for StripeMetersProvider {
    fn id(&self) -> &str {
        "stripe_meters"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::METER | Capabilities::RATE | Capabilities::INVOICE | Capabilities::WEBHOOK
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

    fn as_webhook(&self) -> Option<&dyn WebhookSink> {
        Some(self)
    }

    fn dedup(&self) -> DedupContract {
        DedupContract {
            key: DedupKey::Identifier,
            ttl: DedupTtl::Bounded(Duration::from_secs(24 * 60 * 60)),
        }
    }

    fn correction(&self) -> CorrectionCapability {
        CorrectionCapability::InvoiceCredit
    }
}
