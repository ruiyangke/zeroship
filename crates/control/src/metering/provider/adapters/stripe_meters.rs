use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use crate::metering::provider::{
    AggregateQuery, BillingPeriod, Capabilities, CorrectionCapability, DedupContract, DedupKey,
    DedupTtl, IngestAck, InvoiceRef, Meter, MeteringProvider, ProviderCtx, ProviderError,
    SecretHandle, SubjectRef, UsageEvent,
};
use crate::stripe_client::{StripeApi, StripeClient};

#[derive(Debug, serde::Deserialize)]
struct StripeMetersCfg {
    secret_key: SecretHandle,
    #[serde(default = "default_stripe_base_url")]
    base_url: String,
}

fn default_stripe_base_url() -> String {
    "https://api.stripe.com".to_string()
}

pub struct StripeMetersProvider {
    store: Arc<dyn crate::metering::provider::LiteStore>,
    secret_key: crate::SecretString,
    base_url: String,
}

impl std::fmt::Debug for StripeMetersProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StripeMetersProvider")
            .field("store", &"<lite store>")
            .field("secret_key", &self.secret_key)
            .field("base_url", &self.base_url)
            .finish()
    }
}

pub fn factory(ctx: &ProviderCtx) -> Result<Arc<dyn MeteringProvider>, ProviderError> {
    let cfg: StripeMetersCfg = ctx.parse_adapter_config("stripe_meters")?;
    let secret_key = ctx.secrets.resolve(&cfg.secret_key)?;
    if secret_key.expose_secret().trim().is_empty() {
        return Err(ProviderError::Config(
            "stripe_meters: secret_key resolved empty".to_string(),
        ));
    }
    if cfg.base_url.trim().is_empty() {
        return Err(ProviderError::Config(
            "stripe_meters: base_url must not be empty".to_string(),
        ));
    }
    let store = ctx.store.clone().ok_or_else(|| {
        ProviderError::Config("stripe_meters: LiteStore is required".to_string())
    })?;
    Ok(Arc::new(StripeMetersProvider {
        store,
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
            if event.meter.trim().is_empty() {
                return Err(ProviderError::Config(
                    "stripe_meters: usage event meter must not be empty".to_string(),
                ));
            }
            stripe
                .create_meter_event(
                    // Direct per-metric mapping: zeroship metric name == Stripe
                    // meter event name.
                    &event.meter,
                    &event.creator_subject(),
                    event.value,
                    &event.event_id,
                    event.event_time,
                )
                .await?;
        }
        Ok(IngestAck {
            accepted: batch.len(),
            deduped: None,
        })
    }

    async fn read_aggregate(&self, q: &AggregateQuery) -> Result<u64, ProviderError> {
        if q.meter.trim().is_empty() {
            return Err(ProviderError::Config(
                "stripe_meters: aggregate query meter must not be empty".to_string(),
            ));
        }
        Ok(self
            .stripe()
            .meter_event_summary(
                // Direct per-metric mapping: zeroship metric name == Stripe
                // meter id for read-back.
                &q.meter,
                q.subject.as_str(),
                q.period.start,
                q.period.end,
            )
            .await?)
    }
}

#[async_trait::async_trait(?Send)]
impl crate::metering::provider::Invoicer for StripeMetersProvider {
    async fn close_period(
        &self,
        _subject: &SubjectRef,
        _period: BillingPeriod,
    ) -> Result<InvoiceRef, ProviderError> {
        Ok(InvoiceRef(None))
    }

    async fn adjustment_note(
        &self,
        subject: &SubjectRef,
        note: &crate::metering::provider::AdjustmentNote,
    ) -> Result<InvoiceRef, ProviderError> {
        let creator = Uuid::parse_str(subject.as_str()).map_err(|e| {
            ProviderError::Config(format!("stripe_meters: subject is not a creator UUID: {e}"))
        })?;
        self.store.adjustment_note_invoice(&creator, note).await
    }
}

impl MeteringProvider for StripeMetersProvider {
    fn id(&self) -> &str {
        "stripe_meters"
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
