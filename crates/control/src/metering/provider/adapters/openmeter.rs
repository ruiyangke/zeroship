use std::sync::Arc;

use crate::metering::provider::{
    AggregateQuery, Capabilities, DedupContract, DedupKey, DedupTtl, IngestAck, Meter,
    MeteringProvider, ProviderCtx, ProviderError, SecretHandle, Subject, SubjectRef, UsageEvent,
};
use crate::openmeter_client::{CloudEvent, OpenMeterApi, OpenMeterClient};

#[derive(Debug, serde::Deserialize)]
struct OpenMeterCfg {
    base_url: String,
    token: SecretHandle,
    #[serde(default = "default_event_type")]
    event_type: String,
    meter_slug: String,
}

fn default_event_type() -> String {
    "compute_units".to_string()
}

pub struct OpenMeterProvider {
    base_url: String,
    token: crate::SecretString,
    event_type: String,
    meter_slug: String,
}

pub fn factory(ctx: &ProviderCtx) -> Result<Arc<dyn MeteringProvider>, ProviderError> {
    let cfg: OpenMeterCfg = ctx.parse_adapter_config("openmeter")?;
    let token = ctx.secrets.resolve(&cfg.token)?;
    if cfg.base_url.trim().is_empty() || token.expose_secret().trim().is_empty() {
        return Err(ProviderError::Config(
            "openmeter: base_url + token required — refusing to boot".to_string(),
        ));
    }
    if cfg.meter_slug.trim().is_empty() {
        return Err(ProviderError::Config(
            "openmeter: meter_slug required for aggregate read-back".to_string(),
        ));
    }
    if cfg.event_type.trim().is_empty() {
        return Err(ProviderError::Config(
            "openmeter: event_type must not be empty".to_string(),
        ));
    }
    Ok(Arc::new(OpenMeterProvider {
        base_url: cfg.base_url,
        token,
        event_type: cfg.event_type,
        meter_slug: cfg.meter_slug,
    }))
}

impl OpenMeterProvider {
    fn client(&self) -> OpenMeterClient {
        OpenMeterClient::new(crate::SecretString::new(
            self.token.expose_secret().to_string(),
        ))
        .with_base_url(self.base_url.clone())
    }
}

#[async_trait::async_trait(?Send)]
impl Meter for OpenMeterProvider {
    async fn ingest(&self, batch: &[UsageEvent]) -> Result<IngestAck, ProviderError> {
        let client = self.client();
        for event in batch {
            let cloud = CloudEvent {
                id: event.event_id.clone(),
                event_type: self.event_type.clone(),
                subject: event.creator_subject(),
                time_unix: event.event_time,
                value: event.value,
                period_start_unix: event
                    .dims
                    .get("period_start")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(event.event_time),
            };
            client.ingest_event(&cloud).await?;
        }
        Ok(IngestAck {
            accepted: batch.len(),
            deduped: 0,
        })
    }

    async fn read_aggregate(&self, q: &AggregateQuery) -> Result<u64, ProviderError> {
        self.client()
            .meter_query(&self.meter_slug, q.subject.as_str(), q.period.start, q.period.end)
            .await
    }

    async fn ensure_subject(&self, subject: &Subject) -> Result<SubjectRef, ProviderError> {
        if let Some(existing) = &subject.customer {
            return Ok(existing.clone());
        }
        Ok(SubjectRef(subject.creator_id.to_string()))
    }
}

impl MeteringProvider for OpenMeterProvider {
    fn id(&self) -> &str {
        "openmeter"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::METER
    }

    fn as_meter(&self) -> Option<&dyn Meter> {
        Some(self)
    }

    fn dedup(&self) -> DedupContract {
        DedupContract {
            key: DedupKey::SourceAndId,
            ttl: DedupTtl::Unbounded,
        }
    }
}
