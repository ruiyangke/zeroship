use std::sync::Arc;

use crate::metering::provider::{
    AggregateQuery, Capabilities, DedupContract, DedupKey, DedupTtl, IngestAck, Meter,
    MeteringProvider, ProviderCtx, ProviderError, SecretInput, UsageEvent,
};
use crate::openmeter_client::{CloudEvent, OpenMeterApi, OpenMeterClient};

#[derive(Debug, serde::Deserialize)]
struct OpenMeterCfg {
    base_url: String,
    token: SecretInput,
}

pub struct OpenMeterProvider {
    base_url: String,
    token: crate::SecretString,
}

impl std::fmt::Debug for OpenMeterProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenMeterProvider")
            .field("base_url", &self.base_url)
            .field("token", &self.token)
            .finish()
    }
}

pub fn factory(ctx: &ProviderCtx) -> Result<Arc<dyn MeteringProvider>, ProviderError> {
    let cfg: OpenMeterCfg = ctx.parse_adapter_config("openmeter")?;
    let token = ctx.secrets.resolve(&cfg.token)?;
    if cfg.base_url.trim().is_empty() || token.expose_secret().trim().is_empty() {
        return Err(ProviderError::Config(
            "openmeter: base_url + token required — refusing to boot".to_string(),
        ));
    }
    Ok(Arc::new(OpenMeterProvider {
        base_url: cfg.base_url,
        token,
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
            if event.meter.trim().is_empty() {
                return Err(ProviderError::Config(
                    "openmeter: usage event meter must not be empty".to_string(),
                ));
            }
            let cloud = CloudEvent {
                id: event.event_id.clone(),
                // Direct per-metric mapping: zeroship metric name == OpenMeter
                // event type and meter slug.
                event_type: event.meter.clone(),
                subject: crate::metering::provider::event_subject("openmeter", event)?.to_string(),
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
            deduped: None,
        })
    }

    async fn read_aggregate(&self, q: &AggregateQuery) -> Result<u64, ProviderError> {
        if q.meter.trim().is_empty() {
            return Err(ProviderError::Config(
                "openmeter: aggregate query meter must not be empty".to_string(),
            ));
        }
        self.client()
            .meter_query(&q.meter, q.subject.as_str(), q.period.start, q.period.end)
            .await
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
