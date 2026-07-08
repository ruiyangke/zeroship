//! Lago billing-provider adapter.
//!
//! Lago billing-provider adapter. Adding a provider requires this adapter file
//! plus the `register_builtin` entry in `adapters/mod.rs`, with no provider
//! trait, registry, stack-builder, forwarder, or pipeline edits.

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::metering::provider::{
    AggregateQuery, Backfiller, BillingPeriod, Capabilities, ClosedPeriodPolicy,
    CorrectionCapability, DedupContract, DedupKey, DedupTtl, HttpClientFactory, IngestAck,
    InvoiceRef, LineItem, Meter, MeteringProvider, ProviderCtx, ProviderError, Rater, RatedInput,
    SecretHandle, Subject, SubjectRef, UsageEvent, WebhookEvent, WebhookOutcome, WebhookSink,
};

type HmacSha256 = Hmac<Sha256>;

const LAGO_HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, serde::Deserialize)]
struct LagoCfg {
    api_url: String,
    api_key: SecretHandle,
    billable_metric_code: String,
}

#[derive(Debug)]
pub struct LagoProvider {
    api_url: String,
    api_key: crate::SecretString,
    billable_metric_code: String,
    http: HttpClientFactory,
    seen_webhooks: Mutex<HashSet<String>>,
}

pub fn factory(ctx: &ProviderCtx) -> Result<std::sync::Arc<dyn MeteringProvider>, ProviderError> {
    let cfg: LagoCfg = ctx.parse_adapter_config("lago")?;
    let api_key = ctx.secrets.resolve(&cfg.api_key)?;
    if cfg.api_url.trim().is_empty() {
        return Err(ProviderError::Config(
            "lago: api_url required — refusing to boot".to_string(),
        ));
    }
    if api_key.expose_secret().trim().is_empty() {
        return Err(ProviderError::Config(
            "lago: api_key resolved empty".to_string(),
        ));
    }
    if cfg.billable_metric_code.trim().is_empty() {
        return Err(ProviderError::Config(
            "lago: billable_metric_code required for event ingest/read-back".to_string(),
        ));
    }

    Ok(std::sync::Arc::new(LagoProvider {
        api_url: cfg.api_url.trim_end_matches('/').to_string(),
        api_key,
        billable_metric_code: cfg.billable_metric_code,
        http: ctx.http,
        seen_webhooks: Mutex::new(HashSet::new()),
    }))
}

impl LagoProvider {
    async fn post_json(&self, path: &str, body: Vec<u8>) -> Result<(), ProviderError> {
        let url = format!("{}{}", self.api_url, path);
        let client = self.http.client();
        let builder = client
            .post(&url)
            .map_err(|e| ProviderError::Transport(format!("lago: build request: {e}")))?
            .header("content-type", "application/json")
            .map_err(|e| ProviderError::Transport(format!("lago: set content-type: {e}")))?
            .header(
                "authorization",
                &format!("Bearer {}", self.api_key.expose_secret()),
            )
            .map_err(|e| ProviderError::Transport(format!("lago: set auth header: {e}")))?;
        let response = compio::time::timeout(LAGO_HTTP_TIMEOUT, builder.body(body).send())
            .await
            .map_err(|_| ProviderError::Transport("lago: request timeout".to_string()))?
            .map_err(|e| ProviderError::Transport(format!("lago: transport: {e}")))?;

        let status = response.status().as_u16();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| ProviderError::Transport(format!("lago: read body: {e}")))?;
        if (200..300).contains(&status) {
            Ok(())
        } else if (400..500).contains(&status) {
            Err(ProviderError::permanent_reject(
                status,
                format!("lago: request rejected: {}", body_snippet(&bytes)),
            ))
        } else {
            Err(ProviderError::Transport(format!(
                "lago: request returned HTTP {status}: {}",
                body_snippet(&bytes)
            )))
        }
    }

    async fn get_json(&self, path: &str) -> Result<serde_json::Value, ProviderError> {
        let url = format!("{}{}", self.api_url, path);
        let client = self.http.client();
        let builder = client
            .get(&url)
            .map_err(|e| ProviderError::Transport(format!("lago: build request: {e}")))?
            .header(
                "authorization",
                &format!("Bearer {}", self.api_key.expose_secret()),
            )
            .map_err(|e| ProviderError::Transport(format!("lago: set auth header: {e}")))?;
        let response = compio::time::timeout(LAGO_HTTP_TIMEOUT, builder.send())
            .await
            .map_err(|_| ProviderError::Transport("lago: request timeout".to_string()))?
            .map_err(|e| ProviderError::Transport(format!("lago: transport: {e}")))?;

        let status = response.status().as_u16();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| ProviderError::Transport(format!("lago: read body: {e}")))?;
        let json: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| {
            ProviderError::Transport(format!("lago: response not JSON (status {status}): {e}"))
        })?;
        if (200..300).contains(&status) {
            Ok(json)
        } else if (400..500).contains(&status) {
            Err(ProviderError::permanent_reject(
                status,
                format!("lago: query rejected: {}", body_snippet(&bytes)),
            ))
        } else {
            Err(ProviderError::Transport(format!(
                "lago: query returned HTTP {status}: {}",
                body_snippet(&bytes)
            )))
        }
    }

    async fn post_usage_event(
        &self,
        transaction_id: &str,
        subject: &str,
        timestamp: i64,
        value: u64,
        correction: bool,
    ) -> Result<(), ProviderError> {
        let mut properties = serde_json::Map::new();
        properties.insert("value".to_string(), json!(value));
        if correction {
            properties.insert("zeroship_correct_total".to_string(), json!(true));
        }
        let body = json!({
            "event": {
                "transaction_id": transaction_id,
                "external_customer_id": subject,
                "external_subscription_id": subject,
                "code": self.billable_metric_code,
                "timestamp": timestamp,
                "properties": properties,
            }
        });
        let bytes = serde_json::to_vec(&body)
            .map_err(|e| ProviderError::Transport(format!("lago: encode event: {e}")))?;
        self.post_json("/api/v1/events", bytes).await
    }
}

#[async_trait::async_trait(?Send)]
impl Meter for LagoProvider {
    async fn ingest(&self, batch: &[UsageEvent]) -> Result<IngestAck, ProviderError> {
        for event in batch {
            self.post_usage_event(
                &event.event_id,
                &event.creator_subject(),
                event.event_time,
                event.value,
                false,
            )
            .await?;
        }
        Ok(IngestAck {
            accepted: batch.len(),
            deduped: 0,
        })
    }

    async fn read_aggregate(&self, q: &AggregateQuery) -> Result<u64, ProviderError> {
        let subject = encode_query_component(q.subject.as_str());
        let path = format!(
            "/api/v1/customers/{subject}/current_usage?external_subscription_id={subject}"
        );
        let json = self.get_json(&path).await?;
        Ok(parse_current_usage_total(
            &json,
            &self.billable_metric_code,
            &q.meter,
        ))
    }

    async fn ensure_subject(&self, subject: &Subject) -> Result<SubjectRef, ProviderError> {
        if let Some(existing) = &subject.customer {
            return Ok(existing.clone());
        }
        Ok(SubjectRef(subject.creator_id.to_string()))
    }
}

#[async_trait::async_trait(?Send)]
impl Backfiller for LagoProvider {
    async fn backfill(
        &self,
        subject: &SubjectRef,
        period: BillingPeriod,
        correct_total: u64,
    ) -> Result<(), ProviderError> {
        let transaction_id = format!(
            "zeroship_backfill_{}_{}_{}_{}",
            subject.as_str(),
            period.start,
            period.end,
            correct_total
        );
        self.post_usage_event(
            &transaction_id,
            subject.as_str(),
            period.end.saturating_sub(1),
            correct_total,
            true,
        )
        .await
    }
}

#[async_trait::async_trait(?Send)]
impl Rater for LagoProvider {
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
impl crate::metering::provider::Invoicer for LagoProvider {
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
        Ok(InvoiceRef(None))
    }
}

#[async_trait::async_trait(?Send)]
impl WebhookSink for LagoProvider {
    fn verify(&self, payload: &[u8], sig: &str) -> Result<(), ProviderError> {
        let mut mac =
            HmacSha256::new_from_slice(self.api_key.expose_secret().as_bytes()).map_err(|e| {
                ProviderError::Config(format!("lago: invalid HMAC key material: {e}"))
            })?;
        mac.update(payload);
        let expected = BASE64.encode(mac.finalize().into_bytes());
        if expected
            .as_bytes()
            .ct_eq(sig.trim().as_bytes())
            .into()
        {
            Ok(())
        } else {
            Err(ProviderError::PermanentReject {
                status: 400,
                message: "lago webhook signature rejected".to_string(),
            })
        }
    }

    async fn handle(&self, event: WebhookEvent) -> Result<WebhookOutcome, ProviderError> {
        let event_id = event
            .payload
            .get("id")
            .or_else(|| event.payload.get("webhook_id"))
            .or_else(|| event.payload.get("lago_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| ProviderError::PermanentReject {
                status: 400,
                message: "lago webhook event missing id".to_string(),
            })?;
        let mut seen = self.seen_webhooks.lock().map_err(|_| {
            ProviderError::Store("lago webhook idempotency lock poisoned".to_string())
        })?;
        if seen.insert(event_id) {
            Ok(WebhookOutcome::Processed)
        } else {
            Ok(WebhookOutcome::Ignored)
        }
    }
}

impl MeteringProvider for LagoProvider {
    fn id(&self) -> &str {
        "lago"
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

    fn as_backfiller(&self) -> Option<&dyn Backfiller> {
        Some(self)
    }

    fn dedup(&self) -> DedupContract {
        DedupContract {
            key: DedupKey::TransactionId,
            ttl: DedupTtl::Unbounded,
        }
    }

    fn correction(&self) -> CorrectionCapability {
        CorrectionCapability::Backfill {
            window: Duration::from_secs(32 * 24 * 60 * 60),
            closed: ClosedPeriodPolicy::OpenPeriodOnly,
        }
    }
}

fn parse_current_usage_total(json: &serde_json::Value, code: &str, meter: &str) -> u64 {
    let Some(charges) = json
        .get("customer_usage")
        .and_then(|usage| usage.get("charges_usage"))
        .and_then(serde_json::Value::as_array)
    else {
        return 0;
    };

    charges
        .iter()
        .filter(|charge| {
            let charge_code = charge
                .get("billable_metric")
                .and_then(|m| m.get("code"))
                .and_then(serde_json::Value::as_str);
            charge_code == Some(code) || charge_code == Some(meter)
        })
        .filter_map(|charge| {
            charge
                .get("total_aggregated_units")
                .or_else(|| charge.get("units"))
                .and_then(parse_lago_units)
        })
        .sum()
}

fn parse_lago_units(value: &serde_json::Value) -> Option<u64> {
    if let Some(n) = value.as_u64() {
        return Some(n);
    }
    let n = value.as_f64().or_else(|| value.as_str()?.parse().ok())?;
    if n.is_finite() && n > 0.0 {
        Some(n as u64)
    } else {
        None
    }
}

fn encode_query_component(s: &str) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push(hex_upper(b >> 4));
                out.push(hex_upper(b & 0x0f));
            }
        }
    }
    out
}

fn hex_upper(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + (nibble - 10)) as char,
    }
}

fn body_snippet(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).chars().take(200).collect()
}
