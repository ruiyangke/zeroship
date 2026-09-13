//! Lago billing-provider adapter.
//!
//! Lago billing-provider adapter. Adding a provider requires this adapter file
//! plus the `register_builtin` entry in `adapters/mod.rs`, with no provider
//! trait, registry, stack-builder, forwarder, or pipeline edits.
//!
//! # This adapter does not provision its subject, and Stripe's does
//!
//! Measured 2026-08-11 by enumerating every Lago path the control plane calls:
//! `POST /api/v1/events` (the only write), `GET /api/v1/meters/{slug}/query`,
//! and `GET /api/v1/customers/{subject}/current_usage`. There is no POST to
//! `/api/v1/customers` or `/api/v1/subscriptions` anywhere in
//! `crates/control/src/` - the only things in the repo that create them are the
//! e2e harnesses.
//!
//! The consequence is not lost data, and I checked which it was rather than
//! assuming: an event for an unprovisioned subject is accepted (HTTP 200) and
//! stays retrievable through `GET /api/v1/events?external_subscription_id=...`
//! with null customer/subscription ids. What fails is the INVOICING read below
//! (`current_usage`), which answers `404 resource_not_found`. Usage is recorded
//! and unbillable.
//!
//! The asymmetry is the part worth knowing: `stripe_handlers.rs` DOES provision
//! its provider-side customer - "Ensure a Customer exists (create lazily,
//! once)" on the billing-setup path. So within one subsystem one provider has a
//! provisioning trigger and this one has none, and
//! `docs/reference/billing-metering.md` does not say who is meant to.
//!
//! Whether the control plane should ensure the Lago customer, or whether that
//! is deliberately an operator step that wants documenting, is an open decision
//! (task #309). `tests/e2e_multi_app_attribution.sh` asserts invoiceability and
//! reproduces the unprovisioned state under `ATTR_SKIP_PROVISION=1`.

use std::time::Duration;

use serde_json::json;

use crate::metering::provider::{
    AggregateQuery, Backfiller, BillingPeriod, Capabilities, ClosedPeriodPolicy,
    CorrectionCapability, DedupContract, DedupKey, DedupTtl, HttpClientFactory, IngestAck,
    InvoiceRef, Meter, MeteringProvider, ProviderCtx, ProviderError, SecretInput, SubjectRef,
    UsageEvent,
};

const LAGO_HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, serde::Deserialize)]
struct LagoCfg {
    api_url: String,
    api_key: SecretInput,
}

#[derive(Debug)]
pub struct LagoProvider {
    api_url: String,
    api_key: crate::SecretString,
    http: HttpClientFactory,
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
    Ok(std::sync::Arc::new(LagoProvider {
        api_url: cfg.api_url.trim_end_matches('/').to_string(),
        api_key,
        http: ctx.http,
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
        } else if status == 422 && is_transaction_already_exists(&bytes) {
            // Lago's idempotency dedup. The usage-event POST is keyed by
            // `transaction_id` = the stable `UsageEvent.event_id`; Lago answers a
            // re-POST of an already-recorded id with 422
            // `transaction_id: value_already_exist`. The stream is at-least-once,
            // so a re-delivery WILL hit this — and it means the event is already
            // billed exactly once, i.e. SUCCESS. Treating it as a permanent
            // reject would dead-letter every re-delivered event and bury real
            // rejects in the noise.
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
        metric: &str,
        timestamp: i64,
        value: u64,
        correction: bool,
    ) -> Result<(), ProviderError> {
        if metric.trim().is_empty() {
            return Err(ProviderError::Config(
                "lago: usage event meter must not be empty".to_string(),
            ));
        }
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
                // Direct per-metric mapping: zeroship metric name == Lago
                // billable_metric code.
                "code": metric,
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
                crate::metering::provider::event_subject("lago", event)?,
                &event.meter,
                event.event_time,
                event.value,
                false,
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
                "lago: aggregate query meter must not be empty".to_string(),
            ));
        }
        let subject = encode_query_component(q.subject.as_str());
        let path =
            format!("/api/v1/customers/{subject}/current_usage?external_subscription_id={subject}");
        let json = self.get_json(&path).await?;
        Ok(parse_current_usage_total(&json, &q.meter))
    }
}

#[async_trait::async_trait(?Send)]
impl Backfiller for LagoProvider {
    async fn backfill(
        &self,
        subject: &SubjectRef,
        meter: &str,
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
            meter,
            period.end.saturating_sub(1),
            correct_total,
            true,
        )
        .await
    }
}

#[async_trait::async_trait(?Send)]
impl crate::metering::provider::Invoicer for LagoProvider {
    async fn close_period(
        &self,
        _subject: &SubjectRef,
        _period: BillingPeriod,
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

impl MeteringProvider for LagoProvider {
    fn id(&self) -> &str {
        "lago"
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

fn parse_current_usage_total(json: &serde_json::Value, meter: &str) -> u64 {
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
            charge_code == Some(meter)
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

/// True when a Lago 422 body is the idempotency dedup for a usage event — the
/// `transaction_id` (our stable `UsageEvent.event_id`) is already recorded. Lago
/// returns `{"code":"validation_errors","error_details":{"transaction_id":
/// ["value_already_exist"]}}`. This is the ONLY 422 we treat as success; every
/// other validation error stays a permanent reject.
fn is_transaction_already_exists(bytes: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .as_ref()
        .and_then(|v| v.get("error_details"))
        .and_then(|d| d.get("transaction_id"))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|codes| {
            codes
                .iter()
                .any(|c| c.as_str() == Some("value_already_exist"))
        })
}

#[cfg(test)]
mod tests {
    use super::is_transaction_already_exists;

    #[test]
    fn idempotency_reject_is_recognized_but_other_422s_are_not() {
        // The exact Lago dedup reject → treated as an already-recorded success.
        let dup = br#"{"status":422,"error":"Unprocessable Entity","code":"validation_errors","error_details":{"transaction_id":["value_already_exist"]}}"#;
        assert!(is_transaction_already_exists(dup));

        // A DIFFERENT validation error on transaction_id must NOT be swallowed.
        let other_field = br#"{"code":"validation_errors","error_details":{"external_subscription_id":["value_is_invalid"]}}"#;
        assert!(!is_transaction_already_exists(other_field));

        // A real transaction_id validation error (not the dedup code) stays a reject.
        let other_code = br#"{"code":"validation_errors","error_details":{"transaction_id":["value_is_invalid"]}}"#;
        assert!(!is_transaction_already_exists(other_code));

        // Non-JSON / unrelated bodies are never mistaken for the dedup reject.
        assert!(!is_transaction_already_exists(b"Internal Server Error"));
        assert!(!is_transaction_already_exists(b"{}"));
    }
}
