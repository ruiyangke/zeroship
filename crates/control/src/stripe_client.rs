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
    /// `idempotency_key` makes the create replay-safe. Returns the `ii_…` id.
    #[allow(clippy::too_many_arguments)]
    async fn create_invoice_item(
        &self,
        customer: &str,
        amount_cents: u64,
        currency: &str,
        description: &str,
        period: Period,
        idempotency_key: &str,
    ) -> Result<String, StripeError>;

    /// Create an invoice sweeping `customer`'s pending invoice items, then
    /// finalize it (so it is issued, not left in draft). Returns the `in_…` id.
    async fn create_and_finalize_invoice(
        &self,
        customer: &str,
        idempotency_key: &str,
    ) -> Result<String, StripeError>;
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

    async fn create_invoice_item(
        &self,
        customer: &str,
        amount_cents: u64,
        currency: &str,
        description: &str,
        period: Period,
        idempotency_key: &str,
    ) -> Result<String, StripeError> {
        let amount = i64::try_from(amount_cents).unwrap_or(i64::MAX);
        let form = vec![
            ("customer".to_string(), customer.to_string()),
            ("amount".to_string(), amount.to_string()),
            ("currency".to_string(), currency.to_string()),
            ("description".to_string(), description.to_string()),
            ("period[start]".to_string(), period.start.to_string()),
            ("period[end]".to_string(), period.end.to_string()),
        ];
        let json = self
            .post_form("/v1/invoiceitems", &form, Some(idempotency_key))
            .await?;
        extract_id(&json, "invoice item")
    }

    async fn create_and_finalize_invoice(
        &self,
        customer: &str,
        idempotency_key: &str,
    ) -> Result<String, StripeError> {
        // 1. Create a draft invoice sweeping the customer's pending items.
        //    auto_advance=false so WE control finalization (no surprise charge
        //    timing); the deterministic key makes the create replay-safe.
        let create_form = vec![
            ("customer".to_string(), customer.to_string()),
            ("auto_advance".to_string(), "false".to_string()),
            ("collection_method".to_string(), "charge_automatically".to_string()),
        ];
        let invoice = self
            .post_form("/v1/invoices", &create_form, Some(idempotency_key))
            .await?;
        let invoice_id = extract_id(&invoice, "invoice")?;

        // 2. Finalize it (draft → open/issued). Reuse a derived idempotency key
        //    so the finalize is also replay-safe.
        let finalize_key = format!("{idempotency_key}:finalize");
        let finalized = self
            .post_form(
                &format!("/v1/invoices/{invoice_id}/finalize"),
                &[],
                Some(&finalize_key),
            )
            .await?;
        extract_id(&finalized, "finalized invoice")
    }
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
