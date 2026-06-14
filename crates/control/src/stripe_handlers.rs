//! HTTP handlers for Stripe Connect onboarding + webhook ingest.
//!
//! User-facing (AuthzGuard):
//!   POST   /api/creators/:id/stripe/onboard    → return onboarding URL
//!   POST   /api/creators/:id/stripe/callback   → link acct_xxx
//!   GET    /api/creators/:id/earnings          → totals + recent payouts
//!   DELETE /api/creators/:id/stripe            → unlink
//!
//! Unauthenticated (signature-verified):
//!   POST   /internal/webhooks/stripe           → Stripe-delivered events

use std::sync::Arc;

use hmac::{Hmac, Mac};
use ntex::util::Bytes;
use ntex::web::{self, types::{Path, State}};
use serde::Deserialize;
use sha2::Sha256;
use uuid::Uuid;
use zeroship_authz::{Action as AuthzAction, Resource};

use crate::audit::{self, Action, AuditEntry};
use crate::authz_guard::AuthzGuard;
use crate::http_util;
use crate::stripe_client::StripeApi;
use crate::AppState;
use crate::stripe_store::{self, StripeError};

fn source_ip(req: &web::HttpRequest, state: &AppState) -> Option<String> {
    http_util::source_ip(req, state.trust_proxy)
}

async fn rate_limit(
    req: &web::HttpRequest,
    limiter: &crate::RateLimiter,
    namespace: &str,
    state: &AppState,
) -> Option<web::HttpResponse> {
    http_util::rate_limit(
        req,
        state.control_pg.as_ref(),
        namespace,
        limiter.quota(),
        state.trust_proxy,
    )
    .await
}

type HmacSha256 = Hmac<Sha256>;

fn err_json(status: u16, msg: impl Into<String>) -> web::HttpResponse {
    web::HttpResponse::build(ntex::http::StatusCode::from_u16(status).unwrap())
        .json(&serde_json::json!({"error": msg.into()}))
}

fn stripe_err_response(e: StripeError) -> web::HttpResponse {
    match &e {
        StripeError::Duplicate => {
            web::HttpResponse::Ok().json(&serde_json::json!({"status":"duplicate"}))
        }
        StripeError::NotFound => err_json(404, "not found"),
        StripeError::Validation(m) => err_json(400, m.clone()),
        StripeError::Db(_) => {
            // Don't leak SQL error detail.
            tracing::error!(error = %e, "stripe: store error");
            err_json(500, "internal error")
        }
        StripeError::Api { .. } => {
            // Upstream Stripe rejected the call. Log the detail; surface a
            // generic 502 (bad upstream) without echoing Stripe internals.
            tracing::error!(error = %e, "stripe: upstream API error");
            err_json(502, "stripe upstream error")
        }
    }
}

fn bad_creator_id() -> web::HttpResponse { err_json(400, "bad creator_id") }

// ----------------------------------------------------------------
// Onboarding (master-key)
// ----------------------------------------------------------------

/// Generate a creator onboarding link.
///
/// In production this should POST to `/v1/account_links` on Stripe's
/// API (requires `STRIPE_SECRET_KEY` at the platform level). For v1 we
/// return a placeholder URL so the dashboard flow can be wired without
/// live Stripe integration. Swap in the real Stripe call via
/// `docs/stripe-integration-todo.md` when dashboard UX lands.
pub async fn onboard(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = rate_limit(&req, &state.admin_limiter, "admin", &state).await { return r; }
    if let Err(resp) = authz.require(AuthzAction::BillingWrite, Resource::Any, &state).await {
        return resp;
    }
    let Ok(creator_id) = Uuid::parse_str(&path) else { return bad_creator_id(); };

    // Placeholder — real impl goes to api.stripe.com/v1/account_links.
    let url = format!("https://connect.stripe.com/express_login?creator={creator_id}");
    web::HttpResponse::Ok().json(&serde_json::json!({
        "url": url,
        "note": "placeholder — implement Stripe account_links call per docs/stripe-integration-todo.md",
    }))
}

// ----------------------------------------------------------------
// Stream-1 infra-billing setup (billing PR6)
// ----------------------------------------------------------------

/// `POST /api/creators/:id/billing/setup` — ensure the creator has a platform
/// Stripe **Customer** (`cus_…`), then return a Checkout **setup-mode** session
/// URL so the dashboard can collect + save a PaymentMethod.
///
/// Idempotent on the Customer: if a `cus_…` already exists for the creator
/// (`creator_billing.stripe_customer_id`), reuse it — a second call does NOT
/// create a second Customer. This is the Stream-1 (infra cost) identity, wholly
/// distinct from the Stream-2 Connect `acct_…` `onboard` flow above.
pub async fn billing_setup(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = rate_limit(&req, &state.admin_limiter, "admin", &state).await { return r; }
    // Parse + bind the path id BEFORE authz so we can enforce ownership.
    let Ok(creator_id) = Uuid::parse_str(&path) else { return bad_creator_id(); };

    // CRIT-10: billing/setup is SELF-SERVICE — a creator sets up their OWN card.
    // Allow when the authenticated principal IS the creator (`:id` bound to the
    // principal), OR when a platform billing operator acts (Cedar BillingWrite).
    // This both opens self-service and closes the cross-creator hole (a creator
    // calling billing/setup for a DIFFERENT creator's id is denied).
    // CRIT-10: billing/setup is SELF-SERVICE — a creator sets up their OWN card.
    // Allow when the authenticated principal IS the creator (`:id` bound to the
    // principal), OR when a platform billing operator acts (Cedar BillingWrite).
    // This both opens self-service and closes the cross-creator hole (a creator
    // calling billing/setup for a DIFFERENT creator's id is denied).
    if authz.principal_id != creator_id {
        if let Err(resp) = authz.require(AuthzAction::BillingWrite, Resource::Any, &state).await {
            return resp;
        }
    }

    if state.stripe_secret_key.is_empty() {
        tracing::error!("stripe: billing_setup called with no STRIPE_SECRET_KEY configured");
        return err_json(500, "stripe not configured");
    }
    let stripe = crate::stripe_client::StripeClient::new(crate::SecretString::new(
        state.stripe_secret_key.expose_secret().to_string(),
    ))
    .with_base_url(state.stripe_base_url.clone());

    // Ensure a Customer exists (create lazily, once).
    let customer = match state.stripe_store.get_customer(creator_id).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            let email = match creator_email(&state, creator_id).await {
                Ok(Some(e)) => e,
                Ok(None) => return err_json(404, "creator not found"),
                Err(e) => return stripe_err_response(e),
            };
            let cus = match stripe.create_customer(&email, &creator_id.to_string()).await {
                Ok(c) => c,
                Err(e) => return stripe_err_response(e),
            };
            if let Err(e) = state.stripe_store.set_customer(creator_id, &cus).await {
                return stripe_err_response(e);
            }
            cus
        }
        Err(e) => return stripe_err_response(e),
    };

    // Build the hosted setup session.
    let base = format!("{}://console.{}", state.app_scheme(), state.app_base_domain);
    let success_url = format!("{base}/billing?setup=success");
    let cancel_url = format!("{base}/billing?setup=cancel");
    match stripe
        .create_checkout_setup_session(&customer, &success_url, &cancel_url)
        .await
    {
        Ok(url) => web::HttpResponse::Ok().json(&serde_json::json!({
            "url": url,
            "customer_id": customer,
        })),
        Err(e) => stripe_err_response(e),
    }
}

/// Look up a creator's email (the creator is a user — D4). `None` if no such
/// user row.
async fn creator_email(state: &AppState, creator_id: Uuid) -> Result<Option<String>, StripeError> {
    let conn = state
        .registry
        .conn()
        .await
        .map_err(|e| StripeError::Db(format!("{e}")))?;
    let rows = conn
        .query(
            "SELECT email::text AS email FROM zeroship.users WHERE id = $1",
            &[&creator_id],
        )
        .await
        .map_err(|e| StripeError::Db(e.to_string()))?;
    Ok(rows.first().map(|r| r.get::<_, String>("email")))
}

#[derive(Debug, Deserialize)]
pub struct CallbackBody {
    pub stripe_account_id: String,
}

/// Complete onboarding: the dashboard collects the `acct_xxx` from
/// Stripe's return URL and POSTs it here.
pub async fn callback(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    body: web::types::Json<CallbackBody>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = rate_limit(&req, &state.admin_limiter, "admin", &state).await { return r; }
    if let Err(resp) = authz.require(AuthzAction::BillingWrite, Resource::Any, &state).await {
        return resp;
    }
    let Ok(creator_id) = Uuid::parse_str(&path) else { return bad_creator_id(); };

    match state.stripe_store.link_account(creator_id, &body.stripe_account_id).await {
        Ok(()) => {
            let ip = source_ip(&req, &state);
            audit::log(&state.registry, AuditEntry {
                app_id: None,
                creator_id: Some(creator_id),
                actor_user_id: Some(authz.principal_id),
                actor_token_id: authz.token_id,
                action: Action::LinkAccount,
                resource: Some(&body.stripe_account_id),
                source_ip: ip.as_deref(),
            }).await;
            web::HttpResponse::NoContent().finish()
        }
        Err(e) => stripe_err_response(e),
    }
}

/// Dashboard earnings view.
pub async fn earnings(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = rate_limit(&req, &state.admin_limiter, "admin", &state).await { return r; }
    if let Err(resp) = authz.require(AuthzAction::BillingRead, Resource::Any, &state).await {
        return resp;
    }
    let Ok(creator_id) = Uuid::parse_str(&path) else { return bad_creator_id(); };

    let totals = match state.stripe_store.total_earnings(creator_id).await {
        Ok(t) => t,
        Err(e) => return stripe_err_response(e),
    };
    let recent = match state.stripe_store.recent_payouts(creator_id, 50).await {
        Ok(r) => r,
        Err(e) => return stripe_err_response(e),
    };

    web::HttpResponse::Ok().json(&serde_json::json!({
        "totals": {
            "gross": totals.gross,
            "fee": totals.fee,
            "net": totals.net,
        },
        "recent": recent.iter().map(|p| serde_json::json!({
            "id": p.id.to_string(),
            "event_id": p.event_id,
            "event_type": p.event_type,
            "gross_amount": p.gross_amount,
            "platform_fee": p.platform_fee,
            "net_amount": p.net_amount,
            "currency": p.currency,
            "occurred_at": p.occurred_at,
        })).collect::<Vec<_>>(),
    }))
}

/// Unlink the creator's Stripe account. Cascades and deletes payouts.
pub async fn unlink(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = rate_limit(&req, &state.admin_limiter, "admin", &state).await { return r; }
    if let Err(resp) = authz.require(AuthzAction::BillingWrite, Resource::Any, &state).await {
        return resp;
    }
    let Ok(creator_id) = Uuid::parse_str(&path) else { return bad_creator_id(); };

    match state.stripe_store.unlink_account(creator_id).await {
        Ok(true) => {
            let ip = source_ip(&req, &state);
            audit::log(&state.registry, AuditEntry {
                app_id: None,
                creator_id: Some(creator_id),
                actor_user_id: Some(authz.principal_id),
                actor_token_id: authz.token_id,
                action: Action::UnlinkAccount,
                resource: None,
                source_ip: ip.as_deref(),
            }).await;
            web::HttpResponse::NoContent().finish()
        }
        Ok(false) => err_json(404, "creator not linked"),
        Err(e) => stripe_err_response(e),
    }
}

// ----------------------------------------------------------------
// Webhook ingest (signature-verified)
// ----------------------------------------------------------------

/// Cap on number of `v1=...` entries we'll process. Stripe never
/// sends more than a handful (one per active secret during rotation;
/// typically 1–2). A header padded with hundreds of garbage v1
/// entries would otherwise force constant-time-compare work per entry
/// — a CPU amplification attack via webhook (~60x at MAX_SIGNATURE_HEADER_BYTES).
const MAX_V1_ENTRIES: usize = 8;

/// Verify a Stripe webhook signature header. Mirrors the TypeScript
/// `verifyWebhook` in `@zeroship/payments`.
///
/// Stripe delivers the header like `t=<unix>,v1=<hex>[,v1=<hex>]...`.
/// During a secret rotation the response can carry multiple v1
/// entries (one per active secret); we accept the delivery if ANY v1
/// matches our computed HMAC in constant time, so rotation windows
/// don't drop events.
///
/// `secret` must be non-empty — an empty HMAC key is a misconfiguration
/// that would otherwise silently accept a different ciphertext.
pub fn verify_stripe_signature(
    body: &[u8],
    sig_header: &str,
    secret: &str,
    now_unix: i64,
    tolerance: i64,
) -> Result<i64, String> {
    if secret.is_empty() {
        return Err("empty signing secret".into());
    }
    let mut t: Option<i64> = None;
    let mut had_t_field = false;
    let mut v1s: Vec<&str> = Vec::new();
    let mut v1_overflow = false;
    for part in sig_header.split(',') {
        let part = part.trim();
        if let Some((k, v)) = part.split_once('=') {
            match k.trim() {
                "t" => {
                    had_t_field = true;
                    t = v.trim().parse().ok();
                }
                "v1" => {
                    if v1s.len() < MAX_V1_ENTRIES {
                        v1s.push(v.trim());
                    } else {
                        v1_overflow = true;
                    }
                }
                _ => {}
            }
        }
    }
    if v1_overflow {
        return Err(format!("too many v1 entries (max {MAX_V1_ENTRIES})"));
    }
    let t = match (had_t_field, t) {
        (false, _) => return Err("missing t".into()),
        (true, None) => return Err("bad t".into()),
        (true, Some(v)) => v,
    };
    if v1s.is_empty() {
        return Err("missing v1".into());
    }
    if (now_unix - t).abs() > tolerance {
        return Err("stale".into());
    }
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).map_err(|e| e.to_string())?;
    mac.update(format!("{t}.").as_bytes());
    mac.update(body);
    let expected = mac.finalize().into_bytes();
    let expected_hex = hex::encode(expected.as_slice());
    // Constant-time compare via `subtle::ConstantTimeEq` — hand-rolled
    // byte loops are a common source of correctness regressions.
    // `bitor_assign`-style OR of each v1 match lets us accept any v1
    // without early-exiting once one matches.
    use subtle::ConstantTimeEq;
    let mut matched = subtle::Choice::from(0u8);
    for v1 in &v1s {
        if v1.len() == expected_hex.len() {
            matched |= expected_hex.as_bytes().ct_eq(v1.as_bytes());
        }
    }
    if !bool::from(matched) {
        return Err("signature mismatch".into());
    }
    Ok(t)
}

/// Minimal shape we parse out of Stripe webhook events. Owned strings
/// (`String`) rather than borrowed — any JSON-escaped character in a
/// field would otherwise fail deserialization and 400 a retryable event.
#[derive(Deserialize, Debug)]
struct StripeEvent {
    id: String,
    #[serde(rename = "type")]
    event_type: String,
    created: i64,
    data: StripeEventData,
}

#[derive(Deserialize, Debug)]
struct StripeEventData {
    object: StripeObject,
}

#[derive(Deserialize, Debug)]
struct StripeObject {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    amount_paid: Option<i64>,
    #[serde(default)]
    application_fee_amount: Option<i64>,
    #[serde(default)]
    currency: Option<String>,
    /// The Stripe Customer (`cus_…`) the invoice belongs to. Stream-1's
    /// infra-billing invoices carry this; we reverse-resolve it to a creator via
    /// `creator_billing.stripe_customer_id` when no metadata.creator_id is set.
    #[serde(default)]
    customer: Option<String>,
    /// Invoice's own metadata (generally empty — Stripe doesn't copy
    /// session metadata here).
    #[serde(default)]
    metadata: Option<serde_json::Map<String, serde_json::Value>>,
    /// Newer Stripe wire surfaces subscription metadata at
    /// `invoice.parent.subscription_details.metadata`. For older API
    /// versions it lives directly on the Subscription (fetched by id).
    #[serde(default)]
    parent: Option<InvoiceParent>,
    /// Legacy path: pre-2024 API has `subscription_details` at the top
    /// level of the invoice object.
    #[serde(default)]
    subscription_details: Option<SubscriptionDetails>,
}

#[derive(Deserialize, Debug)]
struct InvoiceParent {
    #[serde(default)]
    subscription_details: Option<SubscriptionDetails>,
}

#[derive(Deserialize, Debug)]
struct SubscriptionDetails {
    #[serde(default)]
    metadata: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Look up the `creator_id` across all known metadata locations.
/// Stripe's webhook wire has moved around — check every reasonable
/// spot so callers only have to set metadata ONCE (on the session)
/// and the SDK writes it to `subscription_data[metadata]` which
/// propagates to both `parent.subscription_details.metadata` and
/// the subscription itself.
fn extract_creator_id(obj: &StripeObject) -> Option<String> {
    let get = |m: &serde_json::Map<String, serde_json::Value>| {
        m.get("creator_id").and_then(|v| v.as_str()).map(str::to_string)
    };
    obj.metadata.as_ref().and_then(get)
        .or_else(|| obj.parent.as_ref().and_then(|p| p.subscription_details.as_ref())
            .and_then(|sd| sd.metadata.as_ref()).and_then(get))
        .or_else(|| obj.subscription_details.as_ref()
            .and_then(|sd| sd.metadata.as_ref()).and_then(get))
}

/// Max raw webhook body we'll accept. Stripe's own `invoice.paid` is a
/// few KB; we pad generously. Larger is rejected before we allocate
/// anything for parsing — defense against POSTing gigabytes.
const MAX_WEBHOOK_BODY_BYTES: usize = 256 * 1024;
/// Max signature header. Stripe's is ~150 bytes; cap to refuse parser-DoS.
const MAX_SIGNATURE_HEADER_BYTES: usize = 4096;

pub async fn webhook(
    req: web::HttpRequest,
    body: Bytes,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    // Internal endpoint, no user authz: Stripe authenticates with the
    // webhook signature and this route has no user principal.
    // Rate limit FIRST — body cap second. Cheap-to-reject things go
    // before expensive ones (parsing 256 KiB, HMAC, DB write).
    if let Some(r) = rate_limit(&req, &state.webhook_limiter, "webhook", &state).await {
        return r;
    }
    if body.len() > MAX_WEBHOOK_BODY_BYTES {
        return err_json(413, format!("webhook body too large: {} bytes", body.len()));
    }
    let raw = body.as_ref();

    // Verify signature unless the operator explicitly opted in to
    // insecure dev mode. Empty secret is NOT a dev bypass — requires
    // `insecure_dev=true` on the AppState.
    if state.stripe_webhook_secret.is_empty() {
        if !state.insecure_dev {
            tracing::error!("stripe: webhook secret not configured; rejecting");
            return err_json(500, "webhook secret not configured");
        }
        // insecure_dev: skip verification (allows `stripe listen` without
        // the signing loop). Logged at startup.
    } else {
        let sig_header = req
            .headers()
            .get("stripe-signature")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if sig_header.len() > MAX_SIGNATURE_HEADER_BYTES {
            return err_json(400, "signature header too large");
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if let Err(e) = verify_stripe_signature(raw, sig_header, state.stripe_webhook_secret.expose_secret(), now, 300) {
            tracing::warn!(error = %e, "stripe: webhook rejected (signature verification)");
            return err_json(400, format!("webhook verification failed: {e}"));
        }
    }

    let event: StripeEvent = match serde_json::from_slice(raw) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!(error = %e, "stripe: invalid webhook json");
            return err_json(400, invalid_json_message());
        }
    };

    let obj = &event.data.object;

    // Stream-1 (infra-billing) lifecycle events. These ride the SAME verified
    // ingest path and resolve the creator via the same `extract_creator_id`
    // (metadata.creator_id, stamped by `create_customer`).
    match event.event_type.as_str() {
        "setup_intent.succeeded" => {
            return handle_setup_intent_succeeded(&req, &state, &event, obj).await;
        }
        "invoice.payment_failed" => {
            return handle_invoice_payment_failed(&req, &state, &event, obj).await;
        }
        // `invoice.paid` carries TWO concerns:
        //   * Stream-1 (infra recovery, G2): a previously-failed infra invoice
        //     was paid → recover the creator's account status (past_due/suspended
        //     → active). This is the REVERSIBILITY rail. Handled here for the
        //     infra creator (resolved by metadata OR customer reverse-resolve)
        //     regardless of whether the event also carries Connect metadata.
        //   * Stream-2 (Connect revenue): the payout-ledger record below (only
        //     when `metadata.creator_id` is present).
        "invoice.paid" => {
            if let Some(cid) = resolve_infra_creator(&state, obj).await {
                let store = crate::account_status::AccountStatusStore::new(state.registry.clone());
                match store.record_payment_recovered(cid).await {
                    Ok(Some(t)) => audit_account_transition(&req, &state, &t, &event.id).await,
                    Ok(None) => { /* nothing to recover (already active / no row) */ }
                    Err(e) => {
                        tracing::error!(error = %e, "stripe: invoice.paid status recovery failed");
                    }
                }
            }
        }
        _ => {
            return web::HttpResponse::Ok().json(&serde_json::json!({"status": "ignored"}));
        }
    }

    let gross = obj.amount_paid.unwrap_or(0);
    let fee = obj.application_fee_amount.unwrap_or(0);
    let currency = obj.currency.clone().unwrap_or_else(|| "usd".into());

    // creator_id flows from the SDK's `buildCheckoutSession` via BOTH
    // session metadata AND `subscription_data[metadata]` — the latter
    // is what actually propagates to invoices. Check multiple wire
    // shapes to be version-robust.
    let Some(creator_id_str) = extract_creator_id(obj) else {
        tracing::warn!(
            event_id = %sanitize_event_id(&event.id),
            "stripe: event missing metadata.creator_id (checked invoice.metadata, \
             invoice.parent.subscription_details.metadata, invoice.subscription_details.metadata) — ignored"
        );
        return web::HttpResponse::Ok().json(&serde_json::json!({"status": "missing_creator_id"}));
    };
    let Ok(creator_id) = Uuid::parse_str(&creator_id_str) else {
        return err_json(
            400,
            format!("bad creator_id in metadata: {}", stripe_store::sanitize_for_display(&creator_id_str)),
        );
    };

    // SHA-256 of the raw body — lets us detect a same-event_id retry
    // arriving with different content (legitimate Stripe retries send
    // the exact same body, so this catches tampering only).
    use sha2::Digest;
    let payload_hash = sha2::Sha256::digest(raw).to_vec();

    match state
        .stripe_store
        .record_payout(
            creator_id,
            &event.id,
            &event.event_type,
            gross,
            fee,
            &currency,
            event.created,
            Some(&payload_hash),
        )
        .await
    {
        Ok(rec) => {
            let ip = source_ip(&req, &state);
            let detail = serde_json::json!({
                "creator_id": creator_id.to_string(),
                "amount_cents": rec.gross_amount,
                "platform_fee_cents": rec.platform_fee,
                "net_amount_cents": rec.net_amount,
                "currency": &rec.currency,
                "stripe_event_id": &rec.event_id,
                "stripe_event_type": &rec.event_type,
                "stripe_object_id": obj.id.as_deref(),
                "stripe_payout_id": obj.id.as_deref().unwrap_or(rec.event_id.as_str()),
                "payout_id": rec.id.to_string(),
            });
            audit::log_with_detail(
                &state.registry,
                AuditEntry {
                    app_id: None,
                    creator_id: Some(creator_id),
                    actor_user_id: None,
                    actor_token_id: None,
                    action: Action::RecordPayout,
                    resource: Some(&event.id),
                    source_ip: ip.as_deref(),
                },
                &detail,
            )
            .await;

            web::HttpResponse::Ok().json(&serde_json::json!({
                "status": "recorded",
                "id": rec.id.to_string(),
                "net_amount": rec.net_amount,
            }))
        }
        Err(StripeError::Duplicate) => {
            web::HttpResponse::Ok().json(&serde_json::json!({"status": "duplicate"}))
        }
        Err(e) => stripe_err_response(e),
    }
}

/// `setup_intent.succeeded` — the creator finished the Checkout setup flow and
/// has a saved default PaymentMethod. Mark `creator_billing.default_pm_set`.
async fn handle_setup_intent_succeeded(
    req: &web::HttpRequest,
    state: &AppState,
    event: &StripeEvent,
    obj: &StripeObject,
) -> web::HttpResponse {
    let Some(creator_id_str) = extract_creator_id(obj) else {
        tracing::warn!(
            event_id = %sanitize_event_id(&event.id),
            "stripe: setup_intent.succeeded missing metadata.creator_id — ignored"
        );
        return web::HttpResponse::Ok().json(&serde_json::json!({"status": "missing_creator_id"}));
    };
    let Ok(creator_id) = Uuid::parse_str(&creator_id_str) else {
        return err_json(400, "bad creator_id in metadata");
    };
    match state.stripe_store.set_default_pm(creator_id).await {
        Ok(()) => {
            let ip = source_ip(req, state);
            audit::log_with_detail(
                &state.registry,
                AuditEntry {
                    app_id: None,
                    creator_id: Some(creator_id),
                    actor_user_id: None,
                    actor_token_id: None,
                    action: Action::SetupIntentSucceeded,
                    resource: Some(&event.id),
                    source_ip: ip.as_deref(),
                },
                &serde_json::json!({
                    "stripe_event_type": "setup_intent.succeeded",
                    "creator_id": creator_id.to_string(),
                    "default_pm_set": true,
                }),
            )
            .await;
            web::HttpResponse::Ok().json(&serde_json::json!({"status": "default_pm_set"}))
        }
        Err(e) => stripe_err_response(e),
    }
}

/// `invoice.payment_failed` — a finalized infra-billing invoice could not be
/// charged. Audit it AND (billing G2) move the creator's account status to
/// `past_due`, starting the dunning window. We do NOT mark the Stripe invoice
/// uncollectible — Stripe's own retry/dunning keeps running; the platform's
/// `max_dunning_days` timeout (the dunning cron) is the suspension deadline.
///
/// `past_due` is the GRACE state — the gateway STILL serves the creator's apps.
/// Only the later dunning-exhaustion sweep suspends (402). Reversible: a recovery
/// (`invoice.paid`) clears it back to active.
async fn handle_invoice_payment_failed(
    req: &web::HttpRequest,
    state: &AppState,
    event: &StripeEvent,
    obj: &StripeObject,
) -> web::HttpResponse {
    let creator_id = resolve_infra_creator(state, obj).await;
    if creator_id.is_none() {
        tracing::warn!(
            event_id = %sanitize_event_id(&event.id),
            "stripe: invoice.payment_failed could not resolve creator_id (no metadata, no matching customer)"
        );
    }

    // G2 state mutation — only with a resolved creator. The signature was already
    // verified by `webhook` before we got here (webhook-truth-only); a forged /
    // unsigned event never reaches this function. Idempotent on the invoice id.
    if let Some(cid) = creator_id {
        let store = crate::account_status::AccountStatusStore::new(state.registry.clone());
        match store.record_payment_failed(cid, obj.id.as_deref()).await {
            Ok(Some(t)) => {
                audit_account_transition(req, state, &t, &event.id).await;
            }
            Ok(None) => { /* no state change (redelivery / already past_due/suspended) */ }
            Err(e) => {
                tracing::error!(error = %e, "stripe: invoice.payment_failed status update failed");
            }
        }
    }

    let ip = source_ip(req, state);
    audit::log_with_detail(
        &state.registry,
        AuditEntry {
            app_id: None,
            creator_id,
            actor_user_id: None,
            actor_token_id: None,
            action: Action::InvoicePaymentFailed,
            resource: Some(&event.id),
            source_ip: ip.as_deref(),
        },
        &serde_json::json!({
            "stripe_event_type": "invoice.payment_failed",
            "creator_id": creator_id.map(|c| c.to_string()),
            "stripe_invoice_id": obj.id.as_deref(),
        }),
    )
    .await;
    web::HttpResponse::Ok().json(&serde_json::json!({"status": "payment_failed_recorded"}))
}

/// Resolve the creator owning an infra-billing invoice: prefer
/// `metadata.creator_id` (stamped on PR6 invoices), else reverse-resolve the
/// Customer (`cus_…`) via `creator_billing` (the case PR6 infra invoices hit,
/// where Stripe surfaces `customer` but no creator metadata on the invoice).
async fn resolve_infra_creator(state: &AppState, obj: &StripeObject) -> Option<Uuid> {
    if let Some(cid) = extract_creator_id(obj).and_then(|s| Uuid::parse_str(&s).ok()) {
        return Some(cid);
    }
    if let Some(customer) = obj.customer.as_deref() {
        match state.stripe_store.get_creator_by_customer(customer).await {
            Ok(c) => return c,
            Err(e) => {
                tracing::warn!(error = %e, "stripe: infra-invoice customer reverse-resolve failed");
            }
        }
    }
    None
}

/// Audit one account-state transition (G2). The detail carries the edge + reason
/// so ops can answer "when/why was this creator past_due/suspended/recovered."
async fn audit_account_transition(
    req: &web::HttpRequest,
    state: &AppState,
    t: &crate::account_status::AccountTransition,
    event_id: &str,
) {
    use crate::account_status::account_state_str;
    let ip = source_ip(req, state);
    audit::log_with_detail(
        &state.registry,
        AuditEntry {
            app_id: None,
            creator_id: Some(t.creator_id),
            actor_user_id: None,
            actor_token_id: None,
            action: Action::AccountStateChange,
            resource: Some(event_id),
            source_ip: ip.as_deref(),
        },
        &serde_json::json!({
            "from": account_state_str(t.from),
            "to": account_state_str(t.to),
            "reason": t.reason,
            "creator_id": t.creator_id.to_string(),
        }),
    )
    .await;
}

fn invalid_json_message() -> &'static str {
    "invalid json"
}

fn sanitize_event_id(s: &str) -> String {
    stripe_store::sanitize_for_display(s)
}

#[cfg(test)]
mod verification_tests {
    use super::*;

    // Shared cross-validation fixture — these MUST match the constants
    // in `sdks/payments/tests/webhook.test.ts` exactly. If either side
    // drifts (algorithm change, payload-format change, hex casing,
    // anything), `cross_validates_with_sdk_format` fails immediately.
    const CROSS_SECRET: &str = "whsec_cross_validation_FIXTURE_v1";
    const CROSS_BODY: &[u8] =
        b"{\"id\":\"evt_cross\",\"type\":\"invoice.paid\",\"created\":1700000000}";
    const CROSS_TIMESTAMP: i64 = 1_700_000_000;
    /// Hex of HMAC-SHA256(CROSS_SECRET, "{CROSS_TIMESTAMP}.{CROSS_BODY}")
    /// computed offline and pinned. The TS suite asserts the same hex.
    const CROSS_EXPECTED_HEX: &str =
        "3a9a1b18f1a3f804c7323a527d3f8588d54cac8e89d3c8572be160ebc904f765";

    const SECRET: &str = "whsec_test_CONTROL";
    const NOW: i64 = 1_700_000_000;

    fn sign(body: &[u8], t: i64) -> String {
        let mut mac = HmacSha256::new_from_slice(SECRET.as_bytes()).unwrap();
        mac.update(format!("{t}.").as_bytes());
        mac.update(body);
        let sig = mac.finalize().into_bytes();
        format!("t={t},v1={}", hex::encode(sig.as_slice()))
    }

    #[test]
    fn accepts_fresh_signature() {
        let body = b"{\"type\":\"invoice.paid\"}";
        let header = sign(body, NOW);
        let result = verify_stripe_signature(body, &header, SECRET, NOW, 300);
        assert_eq!(result.unwrap(), NOW);
    }

    #[test]
    fn rejects_tampered_body() {
        let body = b"{\"x\":1}";
        let header = sign(body, NOW);
        let tampered = b"{\"x\":2}";
        assert!(verify_stripe_signature(tampered, &header, SECRET, NOW, 300).is_err());
    }

    #[test]
    fn rejects_stale_timestamp() {
        let body = b"{}";
        let header = sign(body, NOW - 400);
        assert_eq!(
            verify_stripe_signature(body, &header, SECRET, NOW, 300).unwrap_err(),
            "stale"
        );
    }

    #[test]
    fn rejects_missing_v1() {
        let header = format!("t={NOW}");
        assert!(verify_stripe_signature(b"{}", &header, SECRET, NOW, 300).is_err());
    }

    #[test]
    fn accepts_extra_v0_legacy() {
        let body = b"{}";
        let mut header = sign(body, NOW);
        header.push_str(",v0=legacyignored");
        assert_eq!(
            verify_stripe_signature(body, &header, SECRET, NOW, 300).unwrap(),
            NOW,
        );
    }

    #[test]
    fn rejects_too_many_v1_entries_dos_amplification() {
        // Attacker pads the header with hundreds of v1=garbage to
        // amplify per-request HMAC compare work. Cap is MAX_V1_ENTRIES.
        let body = b"{}";
        let valid = sign(body, NOW);
        let mut header = valid;
        for _ in 0..(MAX_V1_ENTRIES + 5) {
            header.push_str(",v1=deadbeefcafebabe");
        }
        let err = verify_stripe_signature(body, &header, SECRET, NOW, 300).unwrap_err();
        assert!(err.contains("too many v1"), "got: {err}");
    }

    #[test]
    fn cross_validates_with_sdk_format() {
        // Pinned cross-validation: same secret + body + timestamp as the
        // TS test in sdks/payments/tests/webhook.test.ts. If either
        // side's HMAC implementation drifts (algorithm, encoding,
        // payload format, hex casing), one of these assertions fails
        // immediately and forces the maintainer to investigate.
        let mut mac = HmacSha256::new_from_slice(CROSS_SECRET.as_bytes()).unwrap();
        mac.update(format!("{CROSS_TIMESTAMP}.").as_bytes());
        mac.update(CROSS_BODY);
        let actual_hex = hex::encode(mac.finalize().into_bytes().as_slice());
        assert_eq!(actual_hex, CROSS_EXPECTED_HEX,
            "Rust HMAC drifted from pinned fixture — investigate before changing the constant");

        // And confirm verify_stripe_signature accepts the same fixture
        // through the public API (i.e., not just the raw HMAC).
        let header = format!("t={CROSS_TIMESTAMP},v1={CROSS_EXPECTED_HEX}");
        let result = verify_stripe_signature(
            CROSS_BODY, &header, CROSS_SECRET, CROSS_TIMESTAMP, 300,
        );
        assert_eq!(result.unwrap(), CROSS_TIMESTAMP);
    }

    #[test]
    fn invalid_json_message_is_constant() {
        assert_eq!(invalid_json_message(), "invalid json");
    }
}
