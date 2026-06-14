//! Thin `cyper`-based Stripe REST client (billing PR6, ISS-31, Stream-1).
//!
//! Stream-1 (infra-cost billing) talks to the PLATFORM's own Stripe account:
//! it creates a Customer (`cus_…`) per creator, a Checkout setup-mode session to
//! save a PaymentMethod, and — at month close — invoice items + a finalized
//! invoice on that Customer. NO Connect, NO `application_fee` (that is Stream-2).
//!
//! Zero tokio: HTTP is `cyper::Client` + `compio::time::timeout`, the SAME idiom
//! the control plane already uses for the Hydra admin POSTs
//! (`bootstrap_builder.rs`, `oauth_handlers.rs`) and the worker-log GET
//! (`api.rs::fetch_worker_logs`). Bodies are `application/x-www-form-urlencoded`
//! (Stripe's wire); we hand-encode so nested params (`period[start]`,
//! `metadata[creator_id]`) come out in Stripe's bracket form. Every MUTATING
//! call carries an `Idempotency-Key` header (defense in depth on top of the
//! `billing_runs` per-period claim) so an at-least-once retry replays the same
//! Stripe object instead of creating a duplicate.
//!
//! [`StripeApi`] is a trait so unit tests inject a recording fake; the
//! integration tests drive the REAL [`StripeClient`] against a localhost
//! mock-Stripe HTTP server (the base URL is overridable via
//! [`StripeClient::with_base_url`]).

use std::time::Duration;

use crate::stripe_store::StripeError;
use crate::SecretString;

/// Stripe's live API base. Overridable (tests point it at a localhost mock).
pub const DEFAULT_STRIPE_BASE_URL: &str = "https://api.stripe.com";

/// Per-request timeout. Stripe's p99 is well under this; a hung socket must not
/// wedge the reconcile cron tick.
const STRIPE_HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// A billing period `[start, end)` in unix seconds — stamped on an invoice
/// item so the Stripe-side line shows the service window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Period {
    pub start: i64,
    pub end: i64,
}

/// The Stripe billing surface PR6 needs. A trait so unit tests can inject a
/// recording fake; [`StripeClient`] is the production `cyper` impl, and the
/// integration tests use that real impl against a localhost mock server.
#[allow(async_fn_in_trait)]
pub trait StripeApi {
    /// Create a Customer in the platform account for a creator. `creator_id` is
    /// stamped into `metadata.creator_id` so webhooks can resolve it back.
    /// Returns the `cus_…` id.
    async fn create_customer(&self, email: &str, creator_id: &str) -> Result<String, StripeError>;

    /// Create a Checkout session in `mode=setup` to collect + save a
    /// PaymentMethod for `customer`. Returns the hosted session `url`.
    async fn create_checkout_setup_session(
        &self,
        customer: &str,
        success_url: &str,
        cancel_url: &str,
    ) -> Result<String, StripeError>;

    /// Create a pending invoice item on `customer` for one billing line.
    /// `idempotency_key` makes the create replay-safe. `lookup_key` is stamped
    /// into `metadata.zs_item_key` so a >24h re-drive (after the
    /// Idempotency-Key window has expired) can FIND an already-posted item via
    /// [`StripeApi::find_invoice_item_by_key`] instead of blindly re-posting it
    /// (C1). Returns the `ii_…` id.
    #[allow(clippy::too_many_arguments)]
    async fn create_invoice_item(
        &self,
        customer: &str,
        amount_cents: u64,
        currency: &str,
        description: &str,
        period: Period,
        idempotency_key: &str,
        lookup_key: &str,
    ) -> Result<String, StripeError>;

    /// Find a previously-posted, still-pending invoice item on `customer` whose
    /// `metadata.zs_item_key` equals `lookup_key`. Returns the `ii_…` id if one
    /// exists, else `None`.
    ///
    /// C1: when the per-app ledger has an intent row with a NULL `stripe_item_id`
    /// (the prior drive crashed between the Stripe POST and the ledger commit)
    /// AND Stripe's 24h Idempotency-Key window has expired, the deterministic key
    /// no longer dedupes — so we must look the item up by its deterministic
    /// metadata key and adopt it if present, rather than POST a duplicate.
    async fn find_invoice_item_by_key(
        &self,
        customer: &str,
        lookup_key: &str,
    ) -> Result<Option<String>, StripeError>;

    /// Create a DRAFT invoice sweeping `customer`'s pending invoice items.
    /// Returns the draft `in_…` id. `creator_id` is stamped into
    /// `metadata.creator_id` so the `invoice.payment_failed` webhook can resolve
    /// the creator directly. The caller PERSISTS this id (C2) BEFORE calling
    /// [`StripeApi::finalize_invoice`], so a crash before finalize re-drives by
    /// finalizing THIS draft (which carries the real items) rather than creating
    /// a fresh empty draft.
    async fn create_invoice(
        &self,
        customer: &str,
        creator_id: &str,
        idempotency_key: &str,
    ) -> Result<String, StripeError>;

    /// Finalize an existing DRAFT invoice by id (draft → open/issued). Idempotent:
    /// finalizing an already-finalized invoice returns the same `in_…`.
    async fn finalize_invoice(&self, invoice_id: &str) -> Result<String, StripeError>;

    /// Push ONE Stripe **Billing Meter** event (M-Stripe): the compute-unit
    /// `value` for a `(customer, period)` window onto a Stripe Meter that an
    /// operator has provisioned (the metered Price + Subscription self-invoice
    /// from these events on Stripe's own billing cycle — we never invoice on
    /// this rail). `POST /v1/billing/meter_events`, form-encoded:
    ///   * `event_name`                  — the Meter's configured event name
    ///     (e.g. `compute_units`).
    ///   * `payload[stripe_customer_id]` — the `cus_…` the meter aggregates by.
    ///   * `payload[value]`              — the CU delta this push carries.
    ///   * `identifier`                  — the dedup key; Stripe drops a repeat
    ///     event with the same `identifier` within its window (defense in depth
    ///     on top of the export-ledger high-water).
    ///   * `timestamp`                   — the event time (unix secs).
    ///
    /// Stripe meter events are SUMMED, so the caller pushes the CU CONSUMED
    /// SINCE THE LAST EXPORT (a delta), never the cumulative total. The mutating
    /// POST carries `identifier` as its `Idempotency-Key` so a transport-level
    /// retry replays rather than double-counts.
    async fn create_meter_event(
        &self,
        event_name: &str,
        stripe_customer_id: &str,
        value: u64,
        identifier: &str,
        timestamp: i64,
    ) -> Result<(), StripeError>;

    /// Read the Stripe Meter's *aggregated* value for one `(customer, period)`
    /// window — the SUM of every meter event Stripe has accepted for it
    /// (`GET /v1/billing/meters/{meter_id}/event_summaries?customer=…&
    /// start_time=…&end_time=…`, `value_grouping_window=day`, summed).
    ///
    /// This is the C2 re-drive guard: the export cron pushes
    /// `current_local − stripe_aggregate`, so a re-drive PAST Stripe's ~24h
    /// `identifier` dedup window (where a blind re-push would be SUMMED twice)
    /// instead pushes only the still-missing remainder. The guarantee no longer
    /// depends on the local high-water being fresh, nor on the 24h window.
    ///
    /// `meter_id` is the `mtr_…` id; `start_time`/`end_time` are unix seconds
    /// (the billing period `[start, end)`). Returns the aggregated CU total.
    async fn meter_event_summary(
        &self,
        meter_id: &str,
        stripe_customer_id: &str,
        start_time: i64,
        end_time: i64,
    ) -> Result<u64, StripeError>;
}

/// Production `cyper`-based Stripe client. Holds the secret key (never logged —
/// [`SecretString`]) and the base URL (overridable for tests).
#[allow(missing_debug_implementations)]
pub struct StripeClient {
    secret_key: SecretString,
    base_url: String,
}

impl StripeClient {
    /// New client against live Stripe.
    #[must_use]
    pub fn new(secret_key: SecretString) -> Self {
        Self {
            secret_key,
            base_url: DEFAULT_STRIPE_BASE_URL.to_string(),
        }
    }

    /// Override the base URL (no trailing slash) — used by the integration
    /// tests to point the REAL client at a localhost mock-Stripe server.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self
    }

    /// POST a form-encoded body to `path`, with the Bearer auth header and an
    /// optional `Idempotency-Key`. Parses the JSON response, returning the
    /// `id` field on 2xx and mapping a non-2xx to [`StripeError::Api`].
    async fn post_form(
        &self,
        path: &str,
        form: &[(String, String)],
        idempotency_key: Option<&str>,
    ) -> Result<serde_json::Value, StripeError> {
        let url = format!("{}{}", self.base_url, path);
        let body = encode_form(form);
        let client = cyper::Client::new();
        let mut builder = client
            .post(&url)
            .map_err(|e| StripeError::Db(format!("stripe: build request: {e}")))?
            .header("content-type", "application/x-www-form-urlencoded")
            .map_err(|e| StripeError::Db(format!("stripe: set content-type: {e}")))?
            .header(
                "authorization",
                &format!("Bearer {}", self.secret_key.expose_secret()),
            )
            .map_err(|e| StripeError::Db(format!("stripe: set auth header: {e}")))?;
        if let Some(key) = idempotency_key {
            builder = builder
                .header("idempotency-key", key)
                .map_err(|e| StripeError::Db(format!("stripe: set idempotency-key: {e}")))?;
        }
        let response = compio::time::timeout(STRIPE_HTTP_TIMEOUT, builder.body(body).send())
            .await
            .map_err(|_| StripeError::Db("stripe: request timeout".to_string()))?
            .map_err(|e| StripeError::Db(format!("stripe: transport: {e}")))?;

        let status = response.status().as_u16();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| StripeError::Db(format!("stripe: read body: {e}")))?;
        let json: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| {
            StripeError::Db(format!("stripe: response not JSON (status {status}): {e}"))
        })?;

        if (200..300).contains(&status) {
            Ok(json)
        } else {
            // Stripe error bodies are `{ "error": { "code": "...", "message": ... } }`.
            let code = json
                .get("error")
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_str())
                .map(str::to_string);
            Err(StripeError::Api { status, code })
        }
    }

    /// GET `path` (already including any query string) with the Bearer auth
    /// header. Parses the JSON response, mapping a non-2xx to [`StripeError::Api`].
    /// Used by [`StripeApi::find_invoice_item_by_key`] to list invoice items.
    async fn get_json(&self, path: &str) -> Result<serde_json::Value, StripeError> {
        let url = format!("{}{}", self.base_url, path);
        let client = cyper::Client::new();
        let builder = client
            .get(&url)
            .map_err(|e| StripeError::Db(format!("stripe: build request: {e}")))?
            .header(
                "authorization",
                &format!("Bearer {}", self.secret_key.expose_secret()),
            )
            .map_err(|e| StripeError::Db(format!("stripe: set auth header: {e}")))?;
        let response = compio::time::timeout(STRIPE_HTTP_TIMEOUT, builder.send())
            .await
            .map_err(|_| StripeError::Db("stripe: request timeout".to_string()))?
            .map_err(|e| StripeError::Db(format!("stripe: transport: {e}")))?;

        let status = response.status().as_u16();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| StripeError::Db(format!("stripe: read body: {e}")))?;
        let json: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| {
            StripeError::Db(format!("stripe: response not JSON (status {status}): {e}"))
        })?;
        if (200..300).contains(&status) {
            Ok(json)
        } else {
            let code = json
                .get("error")
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_str())
                .map(str::to_string);
            Err(StripeError::Api { status, code })
        }
    }
}

/// Pull the `id` field out of a Stripe object response, or surface a clear
/// error if it is absent (a 2xx with no `id` is a protocol violation).
fn extract_id(json: &serde_json::Value, what: &str) -> Result<String, StripeError> {
    json.get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| StripeError::Db(format!("stripe: {what} response missing id")))
}

impl StripeApi for StripeClient {
    async fn create_customer(&self, email: &str, creator_id: &str) -> Result<String, StripeError> {
        // Customer creation is not retried with a deterministic key (the caller
        // ensures at-most-once via the `creator_billing` row check), so no
        // Idempotency-Key here.
        let form = vec![
            ("email".to_string(), email.to_string()),
            ("metadata[creator_id]".to_string(), creator_id.to_string()),
        ];
        let json = self.post_form("/v1/customers", &form, None).await?;
        extract_id(&json, "customer")
    }

    async fn create_checkout_setup_session(
        &self,
        customer: &str,
        success_url: &str,
        cancel_url: &str,
    ) -> Result<String, StripeError> {
        let form = vec![
            ("mode".to_string(), "setup".to_string()),
            ("customer".to_string(), customer.to_string()),
            ("success_url".to_string(), success_url.to_string()),
            ("cancel_url".to_string(), cancel_url.to_string()),
        ];
        let json = self
            .post_form("/v1/checkout/sessions", &form, None)
            .await?;
        json.get("url")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| StripeError::Db("stripe: checkout session response missing url".into()))
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_invoice_item(
        &self,
        customer: &str,
        amount_cents: u64,
        currency: &str,
        description: &str,
        period: Period,
        idempotency_key: &str,
        lookup_key: &str,
    ) -> Result<String, StripeError> {
        // Money MUST NOT silently clamp on overflow — a clamp would mis-bill.
        // Surface it as a hard validation error so the caller skips this line.
        let amount = i64::try_from(amount_cents).map_err(|_| {
            StripeError::Validation(format!(
                "invoice item amount_cents {amount_cents} exceeds i64::MAX — refusing to clamp"
            ))
        })?;
        let form = vec![
            ("customer".to_string(), customer.to_string()),
            ("amount".to_string(), amount.to_string()),
            ("currency".to_string(), currency.to_string()),
            ("description".to_string(), description.to_string()),
            ("period[start]".to_string(), period.start.to_string()),
            ("period[end]".to_string(), period.end.to_string()),
            // Deterministic lookup key (C1): lets a >24h re-drive FIND this item
            // by metadata (the Idempotency-Key dedupe window having expired)
            // instead of POSTing a duplicate.
            ("metadata[zs_item_key]".to_string(), lookup_key.to_string()),
        ];
        let json = self
            .post_form("/v1/invoiceitems", &form, Some(idempotency_key))
            .await?;
        extract_id(&json, "invoice item")
    }

    async fn find_invoice_item_by_key(
        &self,
        customer: &str,
        lookup_key: &str,
    ) -> Result<Option<String>, StripeError> {
        // List the customer's PENDING (not-yet-invoiced) items and match on the
        // deterministic metadata key. `pending=true` keeps the page small and
        // bounded to items not yet swept onto an invoice. Stripe caps `limit` at
        // 100; a single creator's monthly per-app item count is far below that.
        let enc_customer = encode_query_component(customer);
        let path = format!("/v1/invoiceitems?customer={enc_customer}&pending=true&limit=100");
        let json = self.get_json(&path).await?;
        let Some(items) = json.get("data").and_then(|d| d.as_array()) else {
            return Ok(None);
        };
        for item in items {
            let matches = item
                .get("metadata")
                .and_then(|m| m.get("zs_item_key"))
                .and_then(|k| k.as_str())
                .is_some_and(|k| k == lookup_key);
            if matches {
                if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                    return Ok(Some(id.to_string()));
                }
            }
        }
        Ok(None)
    }

    async fn create_invoice(
        &self,
        customer: &str,
        creator_id: &str,
        idempotency_key: &str,
    ) -> Result<String, StripeError> {
        // Create a draft invoice sweeping the customer's pending items.
        // auto_advance=false so WE control finalization (no surprise charge
        // timing); the deterministic key makes the create replay-safe within 24h.
        // metadata[creator_id] lets invoice.payment_failed resolve the creator.
        // `metadata[invoice_kind]=infra` is the POSITIVE infra signal (critic #6):
        // the `invoice.paid` recovery path only un-suspends when THIS marker is
        // present, so a Connect end-user `invoice.paid` whose customer happens to
        // collide with a platform `creator_billing.stripe_customer_id` can never
        // falsely recover a suspension. Stripe copies invoice metadata onto the
        // `invoice.paid`/`invoice.payment_failed` events, so the webhook sees it.
        let create_form = vec![
            ("customer".to_string(), customer.to_string()),
            ("auto_advance".to_string(), "false".to_string()),
            ("collection_method".to_string(), "charge_automatically".to_string()),
            ("metadata[creator_id]".to_string(), creator_id.to_string()),
            ("metadata[invoice_kind]".to_string(), "infra".to_string()),
        ];
        let invoice = self
            .post_form("/v1/invoices", &create_form, Some(idempotency_key))
            .await?;
        extract_id(&invoice, "invoice")
    }

    async fn finalize_invoice(&self, invoice_id: &str) -> Result<String, StripeError> {
        // Finalize (draft → open/issued). A derived idempotency key keyed on the
        // invoice id makes the finalize replay-safe; finalizing an already-final
        // invoice is itself idempotent on Stripe's side.
        let finalize_key = format!("finalize:{invoice_id}");
        let finalized = self
            .post_form(
                &format!("/v1/invoices/{invoice_id}/finalize"),
                &[],
                Some(&finalize_key),
            )
            .await?;
        extract_id(&finalized, "finalized invoice")
    }

    async fn create_meter_event(
        &self,
        event_name: &str,
        stripe_customer_id: &str,
        value: u64,
        identifier: &str,
        timestamp: i64,
    ) -> Result<(), StripeError> {
        let form = vec![
            ("event_name".to_string(), event_name.to_string()),
            (
                "payload[stripe_customer_id]".to_string(),
                stripe_customer_id.to_string(),
            ),
            ("payload[value]".to_string(), value.to_string()),
            ("identifier".to_string(), identifier.to_string()),
            ("timestamp".to_string(), timestamp.to_string()),
        ];
        // The `identifier` doubles as the Idempotency-Key so a transport retry
        // replays the same event instead of summing it twice. A meter_event
        // response is `{ "object": "billing.meter_event", ... }` (no top-level
        // billable `id` we need) — a 2xx is success; `post_form` already maps a
        // non-2xx to StripeError::Api.
        self.post_form("/v1/billing/meter_events", &form, Some(identifier))
            .await?;
        Ok(())
    }

    async fn meter_event_summary(
        &self,
        meter_id: &str,
        stripe_customer_id: &str,
        start_time: i64,
        end_time: i64,
    ) -> Result<u64, StripeError> {
        // Read the meter's aggregated value for the window. `value_grouping_
        // window=day` keeps the page bounded (≤31 summary rows/month); we SUM the
        // per-window `aggregated_value`s to the period total. A `limit=100` covers
        // a calendar month comfortably. (The reconcile decision is "current −
        // aggregate"; an under-read here would only RE-PUSH a delta that the
        // identifier still dedups within 24h — never a double-count.)
        let enc_meter = encode_query_component(meter_id);
        let enc_customer = encode_query_component(stripe_customer_id);
        let path = format!(
            "/v1/billing/meters/{enc_meter}/event_summaries\
             ?customer={enc_customer}&start_time={start_time}&end_time={end_time}\
             &value_grouping_window=day&limit=100"
        );
        let json = self.get_json(&path).await?;
        let Some(rows) = json.get("data").and_then(|d| d.as_array()) else {
            return Ok(0);
        };
        let mut total: u64 = 0;
        for row in rows {
            // `aggregated_value` is a JSON number; Stripe sums integer CU, so it
            // is an exact non-negative integer. Be defensive about float repr.
            let v = row
                .get("aggregated_value")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0);
            if v.is_finite() && v > 0.0 {
                total = total.saturating_add(v as u64);
            }
        }
        Ok(total)
    }
}

/// Percent-encode a value for use as a URL QUERY-STRING component (used to build
/// the `find_invoice_item_by_key` GET path). Same unreserved set as the form
/// encoder, but a space becomes `%20` (not `+`) per RFC 3986 query rules.
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

/// `application/x-www-form-urlencoded` encode `(key, value)` pairs with Stripe's
/// expected percent-escaping. Keys are already in Stripe bracket form
/// (`period[start]`, `metadata[creator_id]`); both key and value are escaped.
fn encode_form(pairs: &[(String, String)]) -> Vec<u8> {
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        percent_encode_into(&mut out, k);
        out.push('=');
        percent_encode_into(&mut out, v);
    }
    out.into_bytes()
}

/// Percent-encode a single form component per `application/x-www-form-urlencoded`
/// rules: unreserved chars pass through, space → `+`, everything else → `%XX`.
fn percent_encode_into(out: &mut String, s: &str) {
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(hex_upper(b >> 4));
                out.push(hex_upper(b & 0x0f));
            }
        }
    }
}

fn hex_upper(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + (nibble - 10)) as char,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_form_escapes_brackets_and_spaces() {
        let pairs = vec![
            ("period[start]".to_string(), "1700000000".to_string()),
            ("description".to_string(), "infra usage Jun 2026".to_string()),
            ("metadata[creator_id]".to_string(), "abc/def".to_string()),
        ];
        let encoded = String::from_utf8(encode_form(&pairs)).unwrap();
        // Brackets are percent-escaped; spaces become '+'; '/' becomes %2F.
        assert_eq!(
            encoded,
            "period%5Bstart%5D=1700000000&description=infra+usage+Jun+2026&metadata%5Bcreator_id%5D=abc%2Fdef",
        );
    }

    #[test]
    fn encode_form_empty_is_empty() {
        assert!(encode_form(&[]).is_empty());
    }

    #[test]
    fn extract_id_present_and_absent() {
        let with = serde_json::json!({"id": "cus_123", "object": "customer"});
        assert_eq!(extract_id(&with, "customer").unwrap(), "cus_123");
        let without = serde_json::json!({"object": "customer"});
        assert!(extract_id(&without, "customer").is_err());
    }
}
