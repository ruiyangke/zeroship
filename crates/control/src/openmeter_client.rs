//! Thin `cyper`-based OpenMeter REST client (M-OpenMeter, blueprint §M4/§M7).
//!
//! OpenMeter is an *export-only* aggregation sink: the platform PUSHES compute
//! units as **CloudEvents** (`POST /api/v1/events`) and READS the per-subject
//! aggregate back (`GET /api/v1/meters/{slug}/query`). OpenMeter never invoices —
//! billing stays on the Native (or Stripe) rail; on this rail OpenMeter just
//! aggregates the CU stream.
//!
//! The wire shape mirrors [`crate::stripe_client::StripeClient`] exactly — the
//! SAME zero-tokio idiom (`cyper::Client` + `compio::time::timeout`, a Bearer
//! token, an overridable base URL for the localhost mock). The DIFFERENCES from
//! the Stripe client are the body encoding (CloudEvents/JSON, not form) and the
//! query endpoint (an aggregate value, not a list of summary rows).
//!
//! [`OpenMeterApi`] is a trait so unit tests can inject a recording fake; the
//! integration tests drive the REAL [`OpenMeterClient`] against a localhost
//! **mock-OpenMeter** HTTP server (the base URL is overridable via
//! [`OpenMeterClient::with_base_url`]).

use std::time::Duration;

use chrono::{TimeZone, Utc};

use crate::metering::provider::ProviderError;
use crate::SecretString;

/// OpenMeter's default cloud base. Overridable (tests point it at a localhost
/// mock; self-hosters point it at their own deployment).
pub const DEFAULT_OPENMETER_BASE_URL: &str = "https://openmeter.cloud";

/// Per-request timeout. A hung socket must not wedge the export cron tick (same
/// posture as the Stripe client).
const OPENMETER_HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// The CloudEvent `source` every zeroship export carries (the producing system).
pub const CLOUDEVENT_SOURCE: &str = "zeroship-control";

/// One CloudEvent (CloudEvents 1.0) carrying a CU measurement for OpenMeter
/// ingest. The fields map onto OpenMeter's meter config:
///   * `id`      — the deterministic idempotency identifier. OpenMeter DEDUPES on
///     `(source, id)`, so a transport retry / a same-window re-push replays
///     instead of double-counting (defense in depth on top of the export-ledger
///     high-water + the cron's aggregate reconcile).
///   * `type`    — the meter's configured `eventType` (e.g. `compute_units`).
///   * `subject` — the meter's aggregation subject (zeroship maps this to the
///     creator's customer handle — see [`crate::metering::provider::openmeter`]
///     for the mapping; the per-app id rides in `data.app_id`).
///   * `time`    — the CONSUMPTION instant (the cron's `now`), RFC3339/UTC.
///   * `data`    — `{ value: <CU>, app_id, period_start }`; OpenMeter's meter
///     `valueProperty` reads `$.value` and SUMS it per subject per window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudEvent {
    pub id: String,
    pub event_type: String,
    pub subject: String,
    /// The consumption instant in unix seconds (serialized to RFC3339 UTC `time`).
    pub time_unix: i64,
    /// The compute-unit value this event carries (the DELTA the export pushes).
    pub value: u64,
    /// The app the CU is attributed to (echoed into `data.app_id` for audit).
    pub app_id: String,
    /// The billing period start in unix seconds (echoed into `data.period_start`).
    pub period_start_unix: i64,
}

impl CloudEvent {
    /// Render the CloudEvent as the `application/cloudevents+json` body
    /// OpenMeter's `/api/v1/events` ingest expects. `time` is RFC3339/UTC; `data`
    /// carries the integer CU `value` plus the audit fields.
    ///
    /// # Errors
    /// Returns [`ProviderError::Transport`] if `time_unix` is not a representable
    /// UTC instant (never expected for a real wall-clock `now`).
    pub fn to_json(&self) -> Result<serde_json::Value, ProviderError> {
        let time = Utc
            .timestamp_opt(self.time_unix, 0)
            .single()
            .ok_or_else(|| {
                ProviderError::Transport(format!(
                    "openmeter: invalid CloudEvent time_unix {}",
                    self.time_unix
                ))
            })?
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        Ok(serde_json::json!({
            "specversion": "1.0",
            "id": self.id,
            "source": CLOUDEVENT_SOURCE,
            "type": self.event_type,
            "time": time,
            "subject": self.subject,
            "data": {
                "value": self.value,
                "app_id": self.app_id,
                "period_start": self.period_start_unix,
            },
        }))
    }
}

/// The OpenMeter surface the export rail needs. A trait so unit tests can inject
/// a recording fake; [`OpenMeterClient`] is the production `cyper` impl, and the
/// integration tests use that real impl against a localhost mock server.
#[allow(async_fn_in_trait)]
pub trait OpenMeterApi {
    /// Ingest ONE CloudEvent (`POST /api/v1/events`,
    /// `content-type: application/cloudevents+json`). OpenMeter dedupes on the
    /// event `id`, so the caller pushes the CU CONSUMED SINCE THE LAST EXPORT
    /// (a delta) under a deterministic id — never the cumulative total.
    async fn ingest_event(&self, event: &CloudEvent) -> Result<(), ProviderError>;

    /// Read the meter's AGGREGATED value for one `(subject, period)` window — the
    /// SUM OpenMeter has accepted (`GET /api/v1/meters/{slug}/query?subject=…
    /// &from=…&to=…`). This is the C2 re-drive guard: the export cron pushes
    /// `current_local − aggregate`, so a re-drive PAST OpenMeter's dedup window
    /// (where a blind re-push would be SUMMED twice) instead pushes only the
    /// still-missing remainder. The guarantee rides on OpenMeter's own aggregate,
    /// not on the local high-water being fresh.
    ///
    /// `from`/`to` are unix seconds (the billing period `[start, end)`). Returns
    /// the aggregated CU total (0 when the subject has no accepted events yet).
    async fn meter_query(
        &self,
        meter_slug: &str,
        subject: &str,
        from: i64,
        to: i64,
    ) -> Result<u64, ProviderError>;
}

/// Production `cyper`-based OpenMeter client. Holds the API token (never logged —
/// [`SecretString`]) and the base URL (overridable for tests).
#[allow(missing_debug_implementations)]
pub struct OpenMeterClient {
    token: SecretString,
    base_url: String,
}

impl OpenMeterClient {
    /// New client against the configured OpenMeter base.
    #[must_use]
    pub fn new(token: SecretString) -> Self {
        Self {
            token,
            base_url: DEFAULT_OPENMETER_BASE_URL.to_string(),
        }
    }

    /// Override the base URL (no trailing slash) — used by the integration tests
    /// to point the REAL client at a localhost mock-OpenMeter server.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self
    }

    /// POST a JSON body to `path` with the given `content-type` and the Bearer
    /// auth header. Returns the parsed JSON response on 2xx; maps a non-2xx to a
    /// [`ProviderError::Transport`] carrying the status + body (never the token).
    async fn post_json(
        &self,
        path: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<(), ProviderError> {
        let url = format!("{}{}", self.base_url, path);
        let client = cyper::Client::new();
        let builder = client
            .post(&url)
            .map_err(|e| ProviderError::Transport(format!("openmeter: build request: {e}")))?
            .header("content-type", content_type)
            .map_err(|e| ProviderError::Transport(format!("openmeter: set content-type: {e}")))?
            .header(
                "authorization",
                &format!("Bearer {}", self.token.expose_secret()),
            )
            .map_err(|e| ProviderError::Transport(format!("openmeter: set auth header: {e}")))?;
        let response = compio::time::timeout(OPENMETER_HTTP_TIMEOUT, builder.body(body).send())
            .await
            .map_err(|_| ProviderError::Transport("openmeter: request timeout".to_string()))?
            .map_err(|e| ProviderError::Transport(format!("openmeter: transport: {e}")))?;

        let status = response.status().as_u16();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| ProviderError::Transport(format!("openmeter: read body: {e}")))?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            // Surface the status + a bounded slice of the body for diagnosis. The
            // token is in the request header only — never echoed back here.
            let body_snippet: String = String::from_utf8_lossy(&bytes).chars().take(200).collect();
            Err(ProviderError::Transport(format!(
                "openmeter: ingest returned HTTP {status}: {body_snippet}"
            )))
        }
    }

    /// GET `path` (already including any query string) with the Bearer auth
    /// header. Parses the JSON response, mapping a non-2xx to
    /// [`ProviderError::Transport`].
    async fn get_json(&self, path: &str) -> Result<serde_json::Value, ProviderError> {
        let url = format!("{}{}", self.base_url, path);
        let client = cyper::Client::new();
        let builder = client
            .get(&url)
            .map_err(|e| ProviderError::Transport(format!("openmeter: build request: {e}")))?
            .header(
                "authorization",
                &format!("Bearer {}", self.token.expose_secret()),
            )
            .map_err(|e| ProviderError::Transport(format!("openmeter: set auth header: {e}")))?;
        let response = compio::time::timeout(OPENMETER_HTTP_TIMEOUT, builder.send())
            .await
            .map_err(|_| ProviderError::Transport("openmeter: request timeout".to_string()))?
            .map_err(|e| ProviderError::Transport(format!("openmeter: transport: {e}")))?;

        let status = response.status().as_u16();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| ProviderError::Transport(format!("openmeter: read body: {e}")))?;
        let json: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| {
            ProviderError::Transport(format!("openmeter: query not JSON (status {status}): {e}"))
        })?;
        if (200..300).contains(&status) {
            Ok(json)
        } else {
            Err(ProviderError::Transport(format!(
                "openmeter: query returned HTTP {status}"
            )))
        }
    }
}

/// Percent-encode a value for use as a URL QUERY-STRING component (RFC 3986:
/// unreserved pass through, space → `%20`). Used to build the `meter_query`
/// GET path's `subject` / time params.
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

/// Render a unix-seconds instant as RFC3339/UTC for the `from`/`to` query params
/// OpenMeter's `/query` endpoint expects.
fn rfc3339_utc(unix: i64) -> Result<String, ProviderError> {
    Ok(Utc
        .timestamp_opt(unix, 0)
        .single()
        .ok_or_else(|| ProviderError::Transport(format!("openmeter: invalid query bound {unix}")))?
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

impl OpenMeterApi for OpenMeterClient {
    async fn ingest_event(&self, event: &CloudEvent) -> Result<(), ProviderError> {
        let json = event.to_json()?;
        let body = serde_json::to_vec(&json)
            .map_err(|e| ProviderError::Transport(format!("openmeter: encode CloudEvent: {e}")))?;
        self.post_json("/api/v1/events", "application/cloudevents+json", body)
            .await
    }

    async fn meter_query(
        &self,
        meter_slug: &str,
        subject: &str,
        from: i64,
        to: i64,
    ) -> Result<u64, ProviderError> {
        // Query the meter's aggregate for the (subject, window). `subject=…`
        // scopes to one app; `from`/`to` bound the billing period. OpenMeter
        // returns `{ "data": [ { "value": <number>, ... } ] }`; with no
        // windowSize the whole window collapses to ONE row carrying the SUM. We
        // SUM the rows defensively in case a windowed deployment returns several.
        let enc_slug = encode_query_component(meter_slug);
        let enc_subject = encode_query_component(subject);
        let enc_from = encode_query_component(&rfc3339_utc(from)?);
        let enc_to = encode_query_component(&rfc3339_utc(to)?);
        let path = format!(
            "/api/v1/meters/{enc_slug}/query?subject={enc_subject}&from={enc_from}&to={enc_to}"
        );
        let json = self.get_json(&path).await?;
        let Some(rows) = json.get("data").and_then(|d| d.as_array()) else {
            return Ok(0);
        };
        let mut total: u64 = 0;
        for row in rows {
            // `value` is a JSON number; OpenMeter SUMS integer CU, so it is an
            // exact non-negative integer. Be defensive about float repr.
            let v = row
                .get("value")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0);
            if v.is_finite() && v > 0.0 {
                total = total.saturating_add(v as u64);
            }
        }
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloudevent_to_json_has_required_fields() {
        let ev = CloudEvent {
            id: "export:abc:100:0:750".to_string(),
            event_type: "compute_units".to_string(),
            subject: "app-123".to_string(),
            time_unix: 1_746_057_600, // 2025-05-01T00:00:00Z
            value: 750,
            app_id: "app-123".to_string(),
            period_start_unix: 1_746_057_600,
        };
        let json = ev.to_json().expect("renders");
        assert_eq!(json.get("specversion").and_then(|v| v.as_str()), Some("1.0"));
        assert_eq!(json.get("id").and_then(|v| v.as_str()), Some("export:abc:100:0:750"));
        assert_eq!(json.get("source").and_then(|v| v.as_str()), Some(CLOUDEVENT_SOURCE));
        assert_eq!(json.get("type").and_then(|v| v.as_str()), Some("compute_units"));
        assert_eq!(json.get("subject").and_then(|v| v.as_str()), Some("app-123"));
        // `time` is RFC3339/UTC at the consumption instant.
        assert_eq!(json.get("time").and_then(|v| v.as_str()), Some("2025-05-01T00:00:00Z"));
        // `data.value` carries the integer CU.
        assert_eq!(
            json.get("data").and_then(|d| d.get("value")).and_then(serde_json::Value::as_u64),
            Some(750)
        );
        assert_eq!(
            json.get("data").and_then(|d| d.get("app_id")).and_then(|v| v.as_str()),
            Some("app-123")
        );
    }

    #[test]
    fn cloudevent_id_is_carried_verbatim_for_dedup() {
        // OpenMeter dedupes on (source, id) — the id must be the exact
        // deterministic identifier the cron computed, unmodified.
        let ev = CloudEvent {
            id: "export:xyz:200:100:250".to_string(),
            event_type: "cu".to_string(),
            subject: "s".to_string(),
            time_unix: 1_700_000_000,
            value: 150,
            app_id: "a".to_string(),
            period_start_unix: 200,
        };
        let json = ev.to_json().unwrap();
        assert_eq!(json["id"], "export:xyz:200:100:250");
    }

    #[test]
    fn rfc3339_utc_renders_z_suffixed_seconds() {
        assert_eq!(rfc3339_utc(1_746_057_600).unwrap(), "2025-05-01T00:00:00Z");
    }

    #[test]
    fn encode_query_component_escapes_reserved() {
        // A subject/slug with reserved chars is percent-escaped (space → %20).
        assert_eq!(encode_query_component("a b/c"), "a%20b%2Fc");
        assert_eq!(encode_query_component("app-1_2.3~x"), "app-1_2.3~x");
    }
}
