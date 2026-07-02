//! Thin `cyper`-based Stripe REST client (billing PR6, ISS-31, Stream-1).
//!
//! Stream-1 (infra-cost billing) talks to the PLATFORM's own Stripe account:
//! it creates a Customer (`cus_…`) per creator, a Checkout setup-mode session to
//! save a PaymentMethod, and — at month close — invoice items + a finalized
//! invoice on that Customer. NO Connect, NO `application_fee` (that is Stream-2).
//!
//! Zero tokio: HTTP is `cyper::Client` + `compio::time::timeout`, the SAME idiom
//! the control plane already uses for outbound provider calls
//! (`bootstrap_builder.rs`, `oauth_handlers.rs`) and the worker-log GET
//! (`api.rs::fetch_worker_logs`). Bodies are `application/x-www-form-urlencoded`
//! (Stripe's wire); we hand-encode so nested params (`period[start]`,
//! `metadata[creator_id]`) come out in Stripe's bracket form. Money-moving /
//! object-minting MUTATING calls that the caller may retry under a deterministic
//! key (invoice item / invoice / finalize / refund / meter event / connect
//! PaymentIntent) carry an `Idempotency-Key` header (defense in depth on top of
//! the `billing_runs` per-period claim) so an at-least-once retry replays the
//! same Stripe object instead of creating a duplicate. The lazily-created,
//! caller-deduped objects (customer, connect account, account_link, checkout
//! setup session) do NOT carry one — at-most-once is enforced by the caller's
//! own `creator_billing` / `creator_accounts` row check, and an account_link /
//! checkout session is a short-lived hosted URL where a duplicate is harmless
//! (m2).
//!
//! C1: EVERY request — GET, POST, DELETE — sends a pinned `Stripe-Version`
//! header ([`STRIPE_API_VERSION`]) so the response wire shape is the one these
//! parsers target, independent of the account's dashboard-default API version.
//! A forced/dashboard bump cannot silently re-shape the payload under us (the
//! exact failure mode of the D2/Basil `payment_intent`/`charge` removal).
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

/// The Stripe API version this code is WRITTEN AGAINST, sent as the
/// `Stripe-Version` header on EVERY outbound request (C1). Without it, a call
/// renders against the account's *default* version, so a dashboard / forced
/// version bump (exactly how the Basil `payment_intent`/`charge` removal —
/// D2 — silently re-broke parsing) would change the response shape under us.
/// Pinning the header here means the wire shape is the one our parsers expect,
/// independent of the account's dashboard setting.
///
/// `2025-09-30.clover` (Basil 2025-03-31+) is the version whose Invoice shape
/// the D2 settlement parsers target (`payments.data[].payment.payment_intent`,
/// top-level `payment_intent`/`charge` removed). Verified at
/// docs.stripe.com/api/versioning and the Basil changelog.
pub const STRIPE_API_VERSION: &str = "2025-09-30.clover";

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

/// A retrieved Connect account's onboarding signals (billing G1, Stream-2). The
/// `callback` handler reads these from a server-side `retrieve_account` to verify
/// ownership + completeness — never trusting the client's POSTed acct_…
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectAccount {
    /// The `acct_…` id Stripe returned (echoed so the caller can re-confirm it
    /// matches what it requested).
    pub id: String,
    pub charges_enabled: bool,
    pub payouts_enabled: bool,
    pub details_submitted: bool,
    /// The `metadata.creator_id` we stamped at account-create time, if present —
    /// the ownership signal the callback matches against the path principal.
    pub creator_id: Option<String>,
}

/// Result of creating a server-stamped Connect charge (billing G1). The platform
/// builds the PaymentIntent server-side with `application_fee_amount` resolved
/// from the creator's server-held [`crate::fee_policy::FeePolicy`] — the SDK
/// cannot set or override the fee.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectPaymentIntent {
    pub id: String,
    /// The `client_secret` the creator's front-end uses to confirm the payment.
    pub client_secret: Option<String>,
}

/// A retrieved Stripe **Invoice**'s reconciliation-relevant fields (`GET
/// /v1/invoices/{in_}`, #28). All amounts are cents. `status` is the raw Stripe
/// enum (`draft`/`open`/`paid`/`uncollectible`/`void`); the reconciler compares
/// it against OUR `invoices.status` + the cash we recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeInvoice {
    pub id: String,
    pub status: String,
    pub amount_due: i64,
    pub amount_paid: i64,
    pub total: i64,
}

/// A retrieved Stripe **Refund**'s reconciliation-relevant fields (`GET
/// /v1/refunds/{re_}`, #28). `status` is the raw Stripe enum
/// (`pending`/`requires_action`/`succeeded`/`failed`/`canceled`). The reconciler
/// flags a refund Stripe says `failed`/`canceled` that we still hold
/// `pending`/`issued` (a missed `charge.refund.updated`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeRefund {
    pub id: String,
    pub status: String,
    pub amount: i64,
}

/// A retrieved Stripe **Dispute**'s reconciliation-relevant fields (`GET
/// /v1/disputes/{du_}` or a row from `GET /v1/disputes?created>=…`, #28). A
/// Dispute object carries NO `invoice` field — only the settling `charge`
/// (`ch_…`) / `payment_intent` (`pi_…`) (verified at docs.stripe.com/api/disputes/
/// object), which the reconciler resolves back to our invoice via the
/// `billing_provider_refs` linkage (mirroring the webhook's
/// `resolve_invoice_for_dispute`). `status` is the raw Stripe lifecycle value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeDispute {
    pub id: String,
    pub status: String,
    pub amount: i64,
    pub currency: String,
    pub charge: Option<String>,
    pub payment_intent: Option<String>,
    pub reason: Option<String>,
    /// `evidence_details.due_by` (unix seconds), if Stripe supplied it.
    pub evidence_due_by: Option<i64>,
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
    ///
    /// `metadata` carries the FULL CU/usage derivation (`compute_units`,
    /// `billable_units`, `included_units`, `fx_pico_cents_per_unit`,
    /// `base_fee_cents`, `period`, `segment`, and the packed per-metric `usage`
    /// blob(s)) so the charge is fully visible + queryable in the Stripe
    /// dashboard/API. `metadata.zs_item_key` is ALWAYS appended by this method
    /// (the caller's `metadata` must NOT contain it) — the lookup key is the
    /// adopt-path contract, not caller-supplied. DESCRIPTIVE only: the
    /// authoritative `amount_cents` is unaffected by anything in `metadata`.
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
        metadata: &[(String, String)],
    ) -> Result<String, StripeError>;

    /// Delete a PENDING (not-yet-finalized-onto-an-invoice) invoice item by id
    /// (`DELETE /v1/invoiceitems/{id}`). Used by the reconcile's draft-orphan
    /// reconciliation (round 4, MAJOR-1): when a re-drive builds FEWER segments than
    /// a prior crashed drive posted, the stale higher-segment items must be removed
    /// BEFORE the draft sweeps them, or the finalized subtotal disagrees with the
    /// Stripe total (an over-charge). Deleting an item that is already gone (a prior
    /// partial cleanup) returns Stripe's `resource_missing`; the caller treats that
    /// as success (the desired end-state — no such item — already holds).
    async fn delete_invoice_item(&self, item_id: &str) -> Result<(), StripeError>;

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

    // ── Stream-2: Connect onboarding + server-stamped application fee (G1) ───

    /// Create an **Express** Connect account for a creator (`POST /v1/accounts`,
    /// `type=express`). `creator_id` is stamped into `metadata.creator_id` so the
    /// `callback` can VERIFY ownership server-side (it never trusts a client-POSTed
    /// acct_…). `email` pre-fills the onboarding form. Returns the `acct_…` id.
    async fn create_connect_account(
        &self,
        email: &str,
        creator_id: &str,
        country: &str,
    ) -> Result<String, StripeError>;

    /// Create a hosted onboarding **account link** for an existing Connect account
    /// (`POST /v1/account_links`, `type=account_onboarding`). Returns the URL the
    /// creator visits to complete Stripe-hosted onboarding. This REPLACES the
    /// placeholder `connect.stripe.com/express_login?...` URL (ISS-30).
    async fn create_account_link(
        &self,
        account_id: &str,
        refresh_url: &str,
        return_url: &str,
    ) -> Result<String, StripeError>;

    /// Retrieve a Connect account (`GET /v1/accounts/:id`) → its onboarding
    /// signals + the `metadata.creator_id` we stamped at create time. The
    /// `callback` uses this to VERIFY the acct_… belongs to the path creator
    /// (server-side truth), closing the "callback trusts the POSTed acct_…" hole.
    async fn retrieve_account(&self, account_id: &str) -> Result<ConnectAccount, StripeError>;

    /// Resolve a finalized/paid invoice's SETTLEMENT object ids — the
    /// PaymentIntent (`pi_…`) and Charge (`ch_…`) that actually moved the cash —
    /// by an EXPANDED `GET /v1/invoices/{id}` (D2).
    ///
    /// Why a dedicated fetch (not the webhook payload): on Stripe API
    /// `2025-09-30.clover` (Basil 2025-03-31+) the `payment_intent`/`charge`
    /// fields were REMOVED from the Invoice object, and the delivered
    /// `invoice.paid` event payload carries NEITHER them nor an inline
    /// `payments` list. The settlement ids now live under
    /// `invoice.payments.data[].payment.payment_intent`, and the only way to
    /// read them is to EXPAND that path on a fresh retrieve
    /// (`expand[]=payments.data.payment.payment_intent`, verified at
    /// docs.stripe.com/changelog/basil/2025-03-31). A webhook payload cannot be
    /// expanded, so the handler must call this.
    ///
    /// The expanded `payment.payment_intent` is the full PaymentIntent OBJECT:
    /// its `id` is the `pi_…`, and its `latest_charge` (string) is the `ch_…`
    /// (docs.stripe.com/api/payment_intents/object). Returns
    /// `(payment_intent, charge)`, each `None` when absent (a $0/credit-only
    /// invoice has no settlement object — a harmless no-op for the caller).
    async fn invoice_settlement_ids(
        &self,
        provider_invoice_id: &str,
    ) -> Result<(Option<String>, Option<String>), StripeError>;

    /// Create a **Refund** (`re_…`) returning the cash that was collected on a paid
    /// invoice back to the original card (billing-ops gap #26, PR-3).
    /// `provider_invoice_id` is the refund TARGET the caller recorded: the settling
    /// `pi_…`/`ch_…` (preferred — captured at `invoice.paid` from the expanded fetch) or
    /// the Stripe invoice `in_…`. A `pi_…`/`ch_…` is refunded DIRECTLY; an `in_…` is first
    /// resolved to its settling PaymentIntent via an EXPANDED fetch (D2 — the bare
    /// `invoice.payment_intent` field was removed on API 2025-09-30.clover), then refunded.
    /// Issues `POST /v1/refunds {payment_intent|charge, amount, reason}` (verified at
    /// docs.stripe.com/api/refunds/create: a `Refund` "Funds will be refunded to the
    /// credit or debit card that was originally charged" — a credit note alone does NOT
    /// move cash on a paid invoice, so a Refund is the authoritative money-movement object
    /// for `destination='cash'`). `amount_cents` is the positive amount to refund (cents,
    /// the smallest currency unit); a partial refund passes less than the charge.
    /// `currency` is used by the CALLER's ledger/over-refund accounting and is
    /// intentionally NOT sent to Stripe (the Refund API has no `currency` param — sending
    /// one is a 400 `parameter_unknown`; the refund is in the charge's currency).
    /// `idempotency_key` is a deterministic key derived from `refund.id` so a crash-retry
    /// returns the SAME `re_…` rather than double-refunding. Returns the `re_…` id.
    async fn create_refund(
        &self,
        provider_invoice_id: &str,
        amount_cents: u64,
        currency: &str,
        idempotency_key: &str,
    ) -> Result<String, StripeError>;

    /// Create a Connect **PaymentIntent** on the connected account, with the
    /// platform's `application_fee_amount` stamped SERVER-SIDE (`POST
    /// /v1/payment_intents`, `transfer_data[destination]=acct_…`,
    /// `application_fee_amount=<server-resolved fee>`). The fee is computed by the
    /// platform from the creator's server-held [`crate::fee_policy::FeePolicy`] —
    /// the SDK/creator code cannot set or override it (ISS-29). `idempotency_key`
    /// makes the create replay-safe. Returns the intent id + client_secret.
    #[allow(clippy::too_many_arguments)]
    async fn create_connect_payment_intent(
        &self,
        connected_account: &str,
        amount_cents: u64,
        currency: &str,
        application_fee_cents: u64,
        description: &str,
        idempotency_key: &str,
    ) -> Result<ConnectPaymentIntent, StripeError>;

    // ── #28: READ-ONLY state-reconciliation GETs (the missed-webhook backstop) ──

    /// Retrieve a Stripe Invoice's reconciliation fields (`GET /v1/invoices/{in_}`).
    /// READ-ONLY: the reconciler compares Stripe's `status`/`amount_due`/`amount_paid`
    /// against OUR invoice + the cash we recorded, flagging drift (e.g. Stripe paid but
    /// we have no charge row → a missed `invoice.paid`). NO expand needed — these are
    /// top-level Invoice fields on every API version.
    ///
    /// Default: `StripeError::NotFound`. Only the production [`StripeClient`] and the
    /// reconciliation tests' mock model the read path; the other (billing/refund/tax)
    /// recording fakes never reconcile, so they inherit this default rather than each
    /// stubbing four unused methods.
    async fn get_invoice(&self, invoice_id: &str) -> Result<StripeInvoice, StripeError> {
        let _ = invoice_id;
        Err(StripeError::NotFound)
    }

    /// Retrieve a Stripe Refund's reconciliation fields (`GET /v1/refunds/{re_}`).
    /// READ-ONLY: the reconciler catches a refund Stripe says `failed`/`canceled` that we
    /// still hold `pending`/`issued` (a missed `charge.refund.updated`). Default:
    /// `StripeError::NotFound` (see [`StripeApi::get_invoice`]).
    async fn get_refund(&self, refund_id: &str) -> Result<StripeRefund, StripeError> {
        let _ = refund_id;
        Err(StripeError::NotFound)
    }

    /// Retrieve a Stripe Dispute's reconciliation fields (`GET /v1/disputes/{du_}`).
    /// READ-ONLY: the reconciler compares Stripe's status/amount against OUR
    /// `billing_disputes` row. Default: `StripeError::NotFound` (see
    /// [`StripeApi::get_invoice`]).
    async fn get_dispute(&self, dispute_id: &str) -> Result<StripeDispute, StripeError> {
        let _ = dispute_id;
        Err(StripeError::NotFound)
    }

    /// List Stripe Disputes created at/after `created_gte` (unix seconds), most-recent
    /// first, bounded by `limit` (`GET /v1/disputes?created[gte]=…&limit=…`). READ-ONLY:
    /// the reconciler scans this window to find a Stripe dispute we have NEITHER a
    /// `billing_disputes` NOR a `pending_disputes` row for (a fully-missed
    /// `charge.dispute.created`). A single page bounded by `limit` is sufficient — the
    /// per-sweep entity cap keeps the Stripe call rate-aware (no pagination drain).
    /// Default: empty (see [`StripeApi::get_invoice`]).
    async fn list_disputes(
        &self,
        created_gte: i64,
        limit: u32,
    ) -> Result<Vec<StripeDispute>, StripeError> {
        let _ = (created_gte, limit);
        Ok(Vec::new())
    }
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
            .map_err(|e| StripeError::Db(format!("stripe: set auth header: {e}")))?
            // C1: PIN the API version on every request so a dashboard/forced
            // bump cannot silently re-shape the wire under our parsers.
            .header("stripe-version", STRIPE_API_VERSION)
            .map_err(|e| StripeError::Db(format!("stripe: set version header: {e}")))?;
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
            .map_err(|e| StripeError::Db(format!("stripe: set auth header: {e}")))?
            // C1: pin the API version on the GET too.
            .header("stripe-version", STRIPE_API_VERSION)
            .map_err(|e| StripeError::Db(format!("stripe: set version header: {e}")))?;
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

    /// DELETE `path` with the Bearer auth header. Parses the JSON response,
    /// mapping a non-2xx to [`StripeError::Api`]. Used by
    /// [`StripeApi::delete_invoice_item`].
    async fn delete_json(&self, path: &str) -> Result<serde_json::Value, StripeError> {
        let url = format!("{}{}", self.base_url, path);
        let client = cyper::Client::new();
        let builder = client
            .delete(&url)
            .map_err(|e| StripeError::Db(format!("stripe: build request: {e}")))?
            .header(
                "authorization",
                &format!("Bearer {}", self.secret_key.expose_secret()),
            )
            .map_err(|e| StripeError::Db(format!("stripe: set auth header: {e}")))?
            // C1: pin the API version on the DELETE too.
            .header("stripe-version", STRIPE_API_VERSION)
            .map_err(|e| StripeError::Db(format!("stripe: set version header: {e}")))?;
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

/// Parse the settling PaymentIntent (`pi_…`) and Charge (`ch_…`) out of an
/// EXPANDED `GET /v1/invoices/{id}?expand[]=payments.data.payment.payment_intent`
/// response (D2). Robust across API versions:
///   * modern (Basil 2025-03-31+): `payments.data[].payment.payment_intent` is the
///     expanded PaymentIntent OBJECT — its `id` is the `pi_…`, its `latest_charge`
///     (a string) is the `ch_…`. (The top-level `payment_intent`/`charge` fields
///     were removed on these versions, so the expand is the ONLY source.)
///   * legacy (pre-Basil): top-level `invoice.payment_intent` / `invoice.charge`
///     (also tolerated if `payment.payment_intent` is an un-expanded string).
///
/// Returns the FIRST non-empty id seen for each.
fn parse_invoice_settlement_ids(
    invoice: &serde_json::Value,
) -> (Option<String>, Option<String>) {
    let nonempty = |s: Option<&str>| s.map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    // Legacy top-level fields first (pre-Basil accounts/API versions).
    let mut pi = nonempty(invoice.get("payment_intent").and_then(|v| v.as_str()));
    let mut ch = nonempty(invoice.get("charge").and_then(|v| v.as_str()));

    if let Some(entries) = invoice
        .get("payments")
        .and_then(|p| p.get("data"))
        .and_then(|d| d.as_array())
    {
        for entry in entries {
            let Some(payment) = entry.get("payment") else { continue };
            let pi_field = payment.get("payment_intent");
            // Expanded: payment.payment_intent is the full PaymentIntent object.
            if let Some(pi_obj) = pi_field.filter(|v| v.is_object()) {
                if pi.is_none() {
                    pi = nonempty(pi_obj.get("id").and_then(|v| v.as_str()));
                }
                if ch.is_none() {
                    // PaymentIntent.latest_charge is the ch_… (string id, or an
                    // expanded Charge object whose own `id` is the ch_…).
                    let lc = pi_obj.get("latest_charge");
                    ch = nonempty(lc.and_then(|v| v.as_str()))
                        .or_else(|| nonempty(lc.and_then(|v| v.get("id")).and_then(|v| v.as_str())));
                }
            } else if pi.is_none() {
                // Un-expanded fallback: payment.payment_intent is a bare pi_… string.
                pi = nonempty(pi_field.and_then(|v| v.as_str()));
            }
            // Some shapes also carry a bare `charge` string on the payment object.
            if ch.is_none() {
                ch = nonempty(payment.get("charge").and_then(|v| v.as_str()));
            }
        }
    }
    (pi, ch)
}

/// Parse a Stripe Dispute object (from a retrieve OR a `data[]` list row) into the
/// reconciliation-relevant [`StripeDispute`]. `charge`/`payment_intent` may be string
/// ids OR (if a caller ever expands them) objects whose `id` we read; a missing/empty
/// value is `None`. `evidence_details.due_by` is the evidence deadline (unix secs).
fn parse_dispute(d: &serde_json::Value) -> Result<StripeDispute, StripeError> {
    let id = extract_id(d, "dispute")?;
    let status = d
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let amount = d.get("amount").and_then(serde_json::Value::as_i64).unwrap_or(0);
    let currency = d
        .get("currency")
        .and_then(|v| v.as_str())
        .unwrap_or("usd")
        .to_string();
    // A settling id may be a bare string or (rarely) an expanded object → its `id`.
    let id_field = |k: &str| -> Option<String> {
        let f = d.get(k)?;
        let s = f
            .as_str()
            .map(str::to_string)
            .or_else(|| f.get("id").and_then(|v| v.as_str()).map(str::to_string));
        s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
    };
    let charge = id_field("charge");
    let payment_intent = id_field("payment_intent");
    let reason = d
        .get("reason")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|v| !v.is_empty());
    let evidence_due_by = d
        .get("evidence_details")
        .and_then(|e| e.get("due_by"))
        .and_then(serde_json::Value::as_i64);
    Ok(StripeDispute {
        id,
        status,
        amount,
        currency,
        charge,
        payment_intent,
        reason,
        evidence_due_by,
    })
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
        metadata: &[(String, String)],
    ) -> Result<String, StripeError> {
        // Money MUST NOT silently clamp on overflow — a clamp would mis-bill.
        // Surface it as a hard validation error so the caller skips this line.
        let amount = i64::try_from(amount_cents).map_err(|_| {
            StripeError::Validation(format!(
                "invoice item amount_cents {amount_cents} exceeds i64::MAX — refusing to clamp"
            ))
        })?;
        let mut form = vec![
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
        // The CU/usage breakdown (descriptive only — does NOT touch `amount`).
        // The bracket form `metadata[<key>]` is what Stripe expects; the form
        // encoder escapes the brackets on the wire. `zs_item_key` is reserved
        // (set above), so a caller key colliding with it is rejected rather than
        // silently shadowing the adopt-path key.
        for (k, v) in metadata {
            if k == "zs_item_key" {
                return Err(StripeError::Validation(
                    "invoice item metadata key 'zs_item_key' is reserved (set internally)".into(),
                ));
            }
            form.push((format!("metadata[{k}]"), v.clone()));
        }
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

    async fn delete_invoice_item(&self, item_id: &str) -> Result<(), StripeError> {
        let enc = encode_query_component(item_id);
        match self.delete_json(&format!("/v1/invoiceitems/{enc}")).await {
            Ok(_) => Ok(()),
            // Already gone (a prior partial cleanup or a never-posted item) — the
            // desired end-state (no such item) holds, so converge rather than error.
            Err(StripeError::Api { code: Some(code), .. }) if code == "resource_missing" => Ok(()),
            Err(e) => Err(e),
        }
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
            // D1 (real-Stripe): on API version 2025-09-30.clover, POST /v1/invoices
            // defaults `pending_invoice_items_behavior=exclude` — so the draft is
            // created EMPTY and finalize sweeps NOTHING, totalling $0 and billing no
            // infra usage (verified at docs.stripe.com/api/invoices/create:
            // "Defaults to `exclude` if the parameter is omitted" — `include` =
            // "Include any pending invoice items"). We MUST request `include` so the
            // pending `invoice_item`s we posted this period are swept onto THIS draft.
            (
                "pending_invoice_items_behavior".to_string(),
                "include".to_string(),
            ),
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
                // m1: Stripe sums INTEGER CU, so a finite positive aggregate is an exact
                // integer on the wire — but `as u64` floors, silently dropping a
                // fractional remainder if a float repr ever appears (e.g. 41.9999999 →
                // 41). ROUND to the nearest integer explicitly so a representation
                // artefact can't under-count the re-drive delta. `.round()` on a finite
                // value, then `as u64` (now exact), is well-defined.
                total = total.saturating_add(v.round() as u64);
            }
        }
        Ok(total)
    }

    async fn create_connect_account(
        &self,
        email: &str,
        creator_id: &str,
        country: &str,
    ) -> Result<String, StripeError> {
        // Express Connect account. metadata[creator_id] is the OWNERSHIP signal
        // the callback verifies (the account belongs to THIS creator). No
        // Idempotency-Key here: the caller ensures at-most-once via the
        // `creator_accounts` row check (reuse an existing acct_… on re-onboard).
        let form = vec![
            ("type".to_string(), "express".to_string()),
            ("email".to_string(), email.to_string()),
            ("country".to_string(), country.to_string()),
            ("metadata[creator_id]".to_string(), creator_id.to_string()),
        ];
        let json = self.post_form("/v1/accounts", &form, None).await?;
        extract_id(&json, "connect account")
    }

    async fn create_account_link(
        &self,
        account_id: &str,
        refresh_url: &str,
        return_url: &str,
    ) -> Result<String, StripeError> {
        let form = vec![
            ("account".to_string(), account_id.to_string()),
            ("type".to_string(), "account_onboarding".to_string()),
            ("refresh_url".to_string(), refresh_url.to_string()),
            ("return_url".to_string(), return_url.to_string()),
        ];
        let json = self.post_form("/v1/account_links", &form, None).await?;
        json.get("url")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| StripeError::Db("stripe: account_links response missing url".into()))
    }

    async fn retrieve_account(&self, account_id: &str) -> Result<ConnectAccount, StripeError> {
        let enc = encode_query_component(account_id);
        let json = self.get_json(&format!("/v1/accounts/{enc}")).await?;
        let id = extract_id(&json, "account retrieve")?;
        let bool_field = |k: &str| json.get(k).and_then(serde_json::Value::as_bool).unwrap_or(false);
        let creator_id = json
            .get("metadata")
            .and_then(|m| m.get("creator_id"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Ok(ConnectAccount {
            id,
            charges_enabled: bool_field("charges_enabled"),
            payouts_enabled: bool_field("payouts_enabled"),
            details_submitted: bool_field("details_submitted"),
            creator_id,
        })
    }

    async fn create_connect_payment_intent(
        &self,
        connected_account: &str,
        amount_cents: u64,
        currency: &str,
        application_fee_cents: u64,
        description: &str,
        idempotency_key: &str,
    ) -> Result<ConnectPaymentIntent, StripeError> {
        // Money MUST NOT silently clamp on overflow — surface as a hard error.
        let amount = i64::try_from(amount_cents).map_err(|_| {
            StripeError::Validation(format!(
                "payment_intent amount_cents {amount_cents} exceeds i64::MAX — refusing to clamp"
            ))
        })?;
        let fee = i64::try_from(application_fee_cents).map_err(|_| {
            StripeError::Validation(format!(
                "application_fee_cents {application_fee_cents} exceeds i64::MAX — refusing to clamp"
            ))
        })?;
        // The fee is the SERVER-resolved value (from the creator's FeePolicy); the
        // SDK/creator code never reaches this. `transfer_data[destination]` routes
        // the charge (minus fee) to the connected account; `application_fee_amount`
        // is the platform's cut.
        let form = vec![
            ("amount".to_string(), amount.to_string()),
            ("currency".to_string(), currency.to_string()),
            ("description".to_string(), description.to_string()),
            ("application_fee_amount".to_string(), fee.to_string()),
            (
                "transfer_data[destination]".to_string(),
                connected_account.to_string(),
            ),
        ];
        let json = self
            .post_form("/v1/payment_intents", &form, Some(idempotency_key))
            .await?;
        let id = extract_id(&json, "payment intent")?;
        let client_secret = json
            .get("client_secret")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Ok(ConnectPaymentIntent { id, client_secret })
    }

    async fn invoice_settlement_ids(
        &self,
        provider_invoice_id: &str,
    ) -> Result<(Option<String>, Option<String>), StripeError> {
        // EXPAND the per-payment PaymentIntent so the response inlines the full
        // object (its `id` = pi_…, its `latest_charge` = ch_…). The legacy
        // top-level `payment_intent`/`charge` fields are read too, for any
        // pre-Basil account/API version.
        let enc = encode_query_component(provider_invoice_id);
        let path = format!(
            "/v1/invoices/{enc}?expand[]=payments.data.payment.payment_intent"
        );
        let invoice = self.get_json(&path).await?;
        Ok(parse_invoice_settlement_ids(&invoice))
    }

    async fn create_refund(
        &self,
        provider_invoice_id: &str,
        amount_cents: u64,
        currency: &str,
        idempotency_key: &str,
    ) -> Result<String, StripeError> {
        // Money MUST NOT silently clamp on overflow — surface as a hard error.
        let amount = i64::try_from(amount_cents).map_err(|_| {
            StripeError::Validation(format!(
                "refund amount_cents {amount_cents} exceeds i64::MAX — refusing to clamp"
            ))
        })?;
        if amount <= 0 {
            return Err(StripeError::Validation(format!(
                "refund amount must be > 0 (got {amount})"
            )));
        }
        // `POST /v1/refunds` refunds a `charge` or a `payment_intent`, NOT an invoice.
        // `provider_invoice_id` is whatever the caller recorded as the refund target —
        // the authoritative settling `pi_…`/`ch_…` (captured at `invoice.paid` from the
        // expanded fetch) when available, else the Stripe invoice `in_…`.
        //
        //   * `pi_…` / `ch_…` → refund that money object DIRECTLY (the common path).
        //   * `in_…`          → resolve the invoice's settling PaymentIntent via an
        //                        EXPANDED fetch, then refund THAT.
        //
        // D2 (real-Stripe): the bare `invoice.payment_intent` field was REMOVED on API
        // 2025-09-30.clover (Basil 2025-03-31+) — reading it returns NULL and the cash
        // refund 500s. For an `in_…` we therefore go via the EXPANDED
        // `payments.data.payment.payment_intent` path (`invoice_settlement_ids`), never
        // the bare field.
        let (refund_key, refund_target) = if provider_invoice_id.starts_with("pi_") {
            ("payment_intent", provider_invoice_id.to_string())
        } else if provider_invoice_id.starts_with("ch_") {
            ("charge", provider_invoice_id.to_string())
        } else {
            // An `in_…` (or any non-pi/ch id): resolve its settling PaymentIntent.
            let (payment_intent, _charge) =
                self.invoice_settlement_ids(provider_invoice_id).await?;
            let payment_intent = payment_intent.ok_or_else(|| {
                StripeError::Validation(format!(
                    "stripe: invoice {provider_invoice_id} has no settling payment_intent \
                     (expand payments.data.payment.payment_intent returned none) — cannot refund cash"
                ))
            })?;
            ("payment_intent", payment_intent)
        };
        // NOTE: `POST /v1/refunds` does NOT accept a `currency` parameter — the refund
        // is denominated in the original charge's currency automatically. Sending one
        // is a 400 `parameter_unknown` (verified at docs.stripe.com/api/refunds/create:
        // the accepted params are amount/charge/payment_intent/reason/metadata/…, no
        // `currency`). `currency` is retained on the method signature for the ledger /
        // over-refund accounting the CALLER does, but is intentionally NOT sent to Stripe.
        let _ = currency;
        let form = vec![
            (refund_key.to_string(), refund_target),
            ("amount".to_string(), amount.to_string()),
            // requested_by_customer is the closest Stripe reason for an operator
            // goodwill / over-charge correction. It is audit-only at Stripe.
            ("reason".to_string(), "requested_by_customer".to_string()),
        ];
        // The deterministic Idempotency-Key (derived from refund.id) makes a
        // crash-retry replay the SAME `re_…` rather than double-refund.
        let json = self
            .post_form("/v1/refunds", &form, Some(idempotency_key))
            .await?;
        extract_id(&json, "refund")
    }

    async fn get_invoice(&self, invoice_id: &str) -> Result<StripeInvoice, StripeError> {
        let enc = encode_query_component(invoice_id);
        let json = self.get_json(&format!("/v1/invoices/{enc}")).await?;
        let id = extract_id(&json, "invoice retrieve")?;
        let i64_field = |k: &str| json.get(k).and_then(serde_json::Value::as_i64).unwrap_or(0);
        Ok(StripeInvoice {
            id,
            status: json
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            amount_due: i64_field("amount_due"),
            amount_paid: i64_field("amount_paid"),
            total: i64_field("total"),
        })
    }

    async fn get_refund(&self, refund_id: &str) -> Result<StripeRefund, StripeError> {
        let enc = encode_query_component(refund_id);
        let json = self.get_json(&format!("/v1/refunds/{enc}")).await?;
        let id = extract_id(&json, "refund retrieve")?;
        Ok(StripeRefund {
            id,
            status: json
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            amount: json.get("amount").and_then(serde_json::Value::as_i64).unwrap_or(0),
        })
    }

    async fn get_dispute(&self, dispute_id: &str) -> Result<StripeDispute, StripeError> {
        let enc = encode_query_component(dispute_id);
        let json = self.get_json(&format!("/v1/disputes/{enc}")).await?;
        parse_dispute(&json)
    }

    async fn list_disputes(
        &self,
        created_gte: i64,
        limit: u32,
    ) -> Result<Vec<StripeDispute>, StripeError> {
        // Stripe caps `limit` at 100; clamp defensively. `created[gte]` is the
        // inclusive lower bound (verified at docs.stripe.com/api/disputes/list).
        let limit = limit.clamp(1, 100);
        let path = format!("/v1/disputes?created[gte]={created_gte}&limit={limit}");
        let json = self.get_json(&path).await?;
        let Some(rows) = json.get("data").and_then(|d| d.as_array()) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(parse_dispute(row)?);
        }
        Ok(out)
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
    fn settlement_ids_basil_expanded_payment_intent_object() {
        // Real Basil shape after expand[]=payments.data.payment.payment_intent:
        // the payment_intent is a FULL object; pi_ is its id, ch_ is latest_charge.
        let inv = serde_json::json!({
            "id": "in_basil",
            "object": "invoice",
            "payments": { "object": "list", "data": [
                { "payment": { "type": "payment_intent",
                    "payment_intent": { "id": "pi_basil", "object": "payment_intent",
                        "latest_charge": "ch_basil" } } }
            ] }
        });
        let (pi, ch) = parse_invoice_settlement_ids(&inv);
        assert_eq!(pi.as_deref(), Some("pi_basil"), "pi_ from expanded payment_intent.id");
        assert_eq!(ch.as_deref(), Some("ch_basil"), "ch_ from payment_intent.latest_charge");
    }

    #[test]
    fn settlement_ids_basil_omits_top_level_fields() {
        // The Basil invoice has NO top-level payment_intent/charge — confirm we do
        // NOT depend on them (this is exactly what broke against real Stripe).
        let inv = serde_json::json!({
            "id": "in_basil2", "object": "invoice", "status": "paid",
            "payments": { "object": "list", "data": [
                { "payment": { "type": "payment_intent",
                    "payment_intent": { "id": "pi_x", "latest_charge": "ch_x" } } }
            ] }
        });
        assert!(inv.get("payment_intent").is_none(), "fixture has no top-level pi");
        let (pi, ch) = parse_invoice_settlement_ids(&inv);
        assert_eq!(pi.as_deref(), Some("pi_x"));
        assert_eq!(ch.as_deref(), Some("ch_x"));
    }

    #[test]
    fn settlement_ids_legacy_top_level() {
        // Pre-Basil: ids live at the top level of the invoice object.
        let inv = serde_json::json!({
            "id": "in_legacy", "object": "invoice",
            "payment_intent": "pi_legacy", "charge": "ch_legacy"
        });
        let (pi, ch) = parse_invoice_settlement_ids(&inv);
        assert_eq!(pi.as_deref(), Some("pi_legacy"));
        assert_eq!(ch.as_deref(), Some("ch_legacy"));
    }

    #[test]
    fn settlement_ids_absent_is_none() {
        // A $0 / credit-only invoice has no settlement object.
        let inv = serde_json::json!({ "id": "in_zero", "object": "invoice",
            "payments": { "object": "list", "data": [] } });
        let (pi, ch) = parse_invoice_settlement_ids(&inv);
        assert!(pi.is_none() && ch.is_none());
    }

    #[test]
    fn extract_id_present_and_absent() {
        let with = serde_json::json!({"id": "cus_123", "object": "customer"});
        assert_eq!(extract_id(&with, "customer").unwrap(), "cus_123");
        let without = serde_json::json!({"object": "customer"});
        assert!(extract_id(&without, "customer").is_err());
    }
}
