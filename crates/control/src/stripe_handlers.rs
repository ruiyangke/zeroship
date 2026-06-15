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

/// `true` iff `s` is a 3-letter lowercase ISO currency code (`^[a-z]{3}$`).
/// Stripe currencies are lowercase 3-letter codes; reject anything else before
/// it reaches the wire (m1).
fn is_valid_currency(s: &str) -> bool {
    s.len() == 3 && s.bytes().all(|b| b.is_ascii_lowercase())
}

/// Stripe's PaymentIntent `description` max length (chars). A longer value is a
/// 400 `string_too_long` on the wire.
const MAX_STRIPE_DESCRIPTION: usize = 1000;

/// Cap `s` to [`MAX_STRIPE_DESCRIPTION`] chars on a CHAR boundary (m4) so a
/// multi-byte UTF-8 char is never split mid-sequence (which would be invalid
/// UTF-8). Returns the input unchanged when it already fits.
fn cap_stripe_description(s: &str) -> &str {
    match s.char_indices().nth(MAX_STRIPE_DESCRIPTION) {
        Some((byte_idx, _)) => &s[..byte_idx],
        None => s,
    }
}

// ----------------------------------------------------------------
// Onboarding (master-key)
// ----------------------------------------------------------------

/// Country code an Express Connect account is created in. Stripe requires a
/// supported country at account-create time; US is the launch market.
const CONNECT_ACCOUNT_COUNTRY: &str = "US";

/// `POST /api/creators/:id/stripe/onboard` — start (or resume) Stripe **Connect**
/// onboarding for a creator (billing G1, Stream-2, ISS-30).
///
/// REPLACES the old placeholder `connect.stripe.com/express_login?...` URL with a
/// REAL flow: ensure the creator has a Connect `acct_…` (create an Express
/// account once, stamping `metadata.creator_id` for ownership verification),
/// persist it, then return a real `account_links` hosted-onboarding URL.
///
/// **`:id` is bound to the principal** (self-service, like `billing_setup`): a
/// creator onboards their OWN account, OR a platform billing operator
/// (`BillingWrite`/`Resource::Any`) acts on their behalf. A creator calling
/// onboard for a DIFFERENT creator's id is denied — closing the cross-creator hole.
pub async fn onboard(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = rate_limit(&req, &state.admin_limiter, "admin", &state).await { return r; }
    let Ok(creator_id) = Uuid::parse_str(&path) else { return bad_creator_id(); };

    // Bind `:id` to the principal: self-service OR an operator with BillingWrite.
    if authz.principal_id != creator_id {
        if let Err(resp) = authz.require(AuthzAction::BillingWrite, Resource::Any, &state).await {
            return resp;
        }
    }

    if state.stripe_secret_key.expose_secret().is_empty() {
        tracing::error!("stripe: onboard called with no STRIPE_SECRET_KEY configured");
        return err_json(500, "stripe not configured");
    }
    let stripe = crate::stripe_client::StripeClient::new(crate::SecretString::new(
        state.stripe_secret_key.expose_secret().to_string(),
    ))
    .with_base_url(state.stripe_base_url.clone());

    // Reuse the creator's existing Connect account if onboarding was already
    // started (idempotent — re-onboarding resumes the SAME acct_…). Otherwise
    // create an Express account stamped with metadata.creator_id (the ownership
    // signal the callback verifies) and persist it.
    let account_id = match state.stripe_store.get_account(creator_id).await {
        Ok(Some(acct)) => acct.stripe_account_id,
        Ok(None) => {
            let email = match creator_email(&state, creator_id).await {
                Ok(Some(e)) => e,
                Ok(None) => return err_json(404, "creator not found"),
                Err(e) => return stripe_err_response(e),
            };
            let acct = match stripe
                .create_connect_account(&email, &creator_id.to_string(), CONNECT_ACCOUNT_COUNTRY)
                .await
            {
                Ok(a) => a,
                Err(e) => return stripe_err_response(e),
            };
            // Persist via the verified-link path (history + live row). The acct_…
            // here is SERVER-MINTED (we just created it on Stripe), not client input.
            if let Err(e) = state.stripe_store.link_account(creator_id, &acct).await {
                return stripe_err_response(e);
            }
            let ip = source_ip(&req, &state);
            audit::log(&state.registry, AuditEntry {
                app_id: None,
                creator_id: Some(creator_id),
                actor_user_id: Some(authz.principal_id),
                actor_token_id: authz.token_id,
                action: Action::CreateAccount,
                resource: Some(&acct),
                source_ip: ip.as_deref(),
            }).await;
            acct
        }
        Err(e) => return stripe_err_response(e),
    };

    // Build the hosted onboarding link. refresh_url is re-entered if the link
    // expires; return_url is where Stripe sends the creator when done (the
    // dashboard then POSTs callback to refresh status).
    let base = format!("{}://console.{}", state.app_scheme(), state.app_base_domain);
    let refresh_url = format!("{base}/billing/connect?refresh=1");
    let return_url = format!("{base}/billing/connect?done=1");
    match stripe
        .create_account_link(&account_id, &refresh_url, &return_url)
        .await
    {
        Ok(url) => web::HttpResponse::Ok().json(&serde_json::json!({
            "url": url,
            "account_id": account_id,
        })),
        Err(e) => stripe_err_response(e),
    }
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
    /// OPTIONAL hint from the dashboard's return URL. It is NEVER trusted to
    /// LINK an account: ownership is verified SERVER-SIDE against the acct_… we
    /// minted in `onboard` (stored on `creator_accounts`) + Stripe's
    /// `metadata.creator_id`. A mismatching/forged acct_… is rejected (ISS-30).
    #[serde(default)]
    pub stripe_account_id: Option<String>,
}

/// `POST /api/creators/:id/stripe/callback` — refresh Connect onboarding status
/// after the creator returns from the Stripe-hosted flow (billing G1, ISS-30).
///
/// **SECURITY (ISS-30 fix).** The old handler blindly `link_account`'d a POSTed
/// `acct_…` — a creator could bind an account they don't control. This handler
/// instead drives the verification SERVER-SIDE:
///   1. Load the acct_… we MINTED for this creator in `onboard` (server truth on
///      `creator_accounts`). No stored account ⇒ 400 (onboard first).
///   2. If the body carries an acct_… hint, it MUST equal the stored one — a
///      foreign/forged acct_… is REJECTED (403), never linked.
///   3. `retrieve_account` from Stripe and verify `metadata.creator_id` (which we
///      stamped at create) equals this creator — REJECT (403) otherwise.
///   4. Persist the verified `charges_enabled`/`payouts_enabled`/`details_submitted`
///      flags from Stripe's truth.
///
/// `:id` is bound to the principal (self-service) OR an operator (BillingWrite).
pub async fn callback(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    body: web::types::Json<CallbackBody>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = rate_limit(&req, &state.admin_limiter, "admin", &state).await { return r; }
    let Ok(creator_id) = Uuid::parse_str(&path) else { return bad_creator_id(); };

    // Bind `:id` to the principal: self-service OR an operator with BillingWrite.
    if authz.principal_id != creator_id {
        if let Err(resp) = authz.require(AuthzAction::BillingWrite, Resource::Any, &state).await {
            return resp;
        }
    }

    if state.stripe_secret_key.expose_secret().is_empty() {
        tracing::error!("stripe: callback called with no STRIPE_SECRET_KEY configured");
        return err_json(500, "stripe not configured");
    }

    // (1) The acct_… is SERVER TRUTH — the one we minted in `onboard`.
    let stored = match state.stripe_store.get_account(creator_id).await {
        Ok(Some(acct)) => acct.stripe_account_id,
        Ok(None) => return err_json(400, "no connect account; call onboard first"),
        Err(e) => return stripe_err_response(e),
    };

    // (2) A body hint, if present, must MATCH the stored acct_… — a forged
    // foreign acct_… is rejected, never linked.
    if let Some(claimed) = body.stripe_account_id.as_deref() {
        if claimed != stored {
            tracing::warn!(
                creator_id = %creator_id,
                claimed = %stripe_store::sanitize_for_display(claimed),
                "stripe: callback rejected — POSTed acct_… does not match the creator's onboarded account"
            );
            return err_json(403, "stripe account not owned by this creator");
        }
    }

    // (3) Verify ownership against Stripe's truth: retrieve the account and confirm
    // metadata.creator_id (stamped at create) is THIS creator.
    let stripe = crate::stripe_client::StripeClient::new(crate::SecretString::new(
        state.stripe_secret_key.expose_secret().to_string(),
    ))
    .with_base_url(state.stripe_base_url.clone());
    let account = match stripe.retrieve_account(&stored).await {
        Ok(a) => a,
        Err(e) => return stripe_err_response(e),
    };
    let owner_ok = account
        .creator_id
        .as_deref()
        .and_then(|s| Uuid::parse_str(s).ok())
        == Some(creator_id);
    if !owner_ok {
        tracing::warn!(
            creator_id = %creator_id,
            "stripe: callback rejected — retrieved account metadata.creator_id does not match"
        );
        return err_json(403, "stripe account not owned by this creator");
    }

    // (4) Persist Stripe's verified onboarding flags.
    match state
        .stripe_store
        .set_account_flags(
            creator_id,
            &stored,
            account.charges_enabled,
            account.payouts_enabled,
            account.details_submitted,
        )
        .await
    {
        Ok(_) => {
            let ip = source_ip(&req, &state);
            audit::log(&state.registry, AuditEntry {
                app_id: None,
                creator_id: Some(creator_id),
                actor_user_id: Some(authz.principal_id),
                actor_token_id: authz.token_id,
                action: Action::LinkAccount,
                resource: Some(&stored),
                source_ip: ip.as_deref(),
            }).await;
            web::HttpResponse::Ok().json(&serde_json::json!({
                "account_id": stored,
                "charges_enabled": account.charges_enabled,
                "payouts_enabled": account.payouts_enabled,
                "details_submitted": account.details_submitted,
            }))
        }
        Err(e) => stripe_err_response(e),
    }
}

// ----------------------------------------------------------------
// Server-stamped Connect checkout (billing G1, ISS-29 fee-bypass fix)
// ----------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ConnectCheckoutBody {
    /// The amount the creator charges THEIR end-user, in cents. BUSINESS input —
    /// the creator names what to charge their customer.
    pub amount_cents: u64,
    /// ISO currency (e.g. "usd").
    pub currency: String,
    /// Optional human description on the charge.
    #[serde(default)]
    pub description: Option<String>,
    /// An idempotency discriminator for THIS cart/checkout (the SDK supplies a
    /// stable value per end-user cart so a retry replays the same PaymentIntent).
    #[serde(default)]
    pub cart_id: Option<String>,
}

/// `POST /api/creators/:id/connect/checkout` — create a Connect PaymentIntent
/// with the platform's `application_fee_amount` stamped **SERVER-SIDE** (billing
/// G1, ISS-29 fix).
///
/// **The fee is server-authoritative.** The body carries only BUSINESS params
/// (amount, currency, end-user). The platform resolves the creator's server-held
/// [`crate::fee_policy::FeePolicy`] and computes `application_fee_amount` itself;
/// the SDK/creator code can NOT name, set, or override the fee. Any
/// `application_fee*` a client tries to send is simply not read here (the body
/// has no such field) — there is no wire path for it.
///
/// `:id` is bound to the principal (self-service) OR an operator (BillingWrite).
pub async fn connect_checkout(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    body: web::types::Json<ConnectCheckoutBody>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = rate_limit(&req, &state.admin_limiter, "admin", &state).await { return r; }
    let Ok(creator_id) = Uuid::parse_str(&path) else { return bad_creator_id(); };

    if authz.principal_id != creator_id {
        if let Err(resp) = authz.require(AuthzAction::BillingWrite, Resource::Any, &state).await {
            return resp;
        }
    }

    if body.amount_cents == 0 {
        return err_json(400, "amount_cents must be positive");
    }
    // m1: validate the currency is a 3-letter ISO code (`^[a-z]{3}$`) before it
    // touches the Stripe wire — keep junk off the upstream call.
    if !is_valid_currency(&body.currency) {
        return err_json(400, "currency must be a 3-letter ISO code (lowercase)");
    }
    // M2: a non-empty `cart_id` is REQUIRED. Without it every checkout for a
    // creator collapses onto one idempotency key, replaying the first
    // PaymentIntent (a different-amount charge silently returns a stale intent).
    let cart = body.cart_id.as_deref().map(str::trim).unwrap_or("");
    if cart.is_empty() {
        return err_json(400, "cart_id is required");
    }
    if state.stripe_secret_key.is_empty() {
        tracing::error!("stripe: connect_checkout called with no STRIPE_SECRET_KEY configured");
        return err_json(500, "stripe not configured");
    }

    // The connected account must exist + be ready to take charges. This is
    // server truth (the acct_… we minted + verified), never client input.
    let account = match state.stripe_store.get_account(creator_id).await {
        Ok(Some(a)) => a,
        Ok(None) => return err_json(400, "creator has no connected stripe account"),
        Err(e) => return stripe_err_response(e),
    };
    // M1: the `charges_enabled` flag (verified by `callback` from Stripe's truth)
    // gates the charge path. A creator who ran `onboard` but never finished
    // Stripe onboarding has the account row but charges_enabled=false — reject
    // BEFORE any PaymentIntent POST.
    if !account.charges_enabled {
        return err_json(400, "creator stripe account not ready (complete onboarding)");
    }

    // Resolve the SERVER-HELD fee policy and compute the fee. The default (no
    // row) is 15%. The creator cannot influence this value.
    let store = crate::fee_policy::FeePolicyStore::new(state.registry.clone());
    let policy = match store.get(creator_id).await {
        Ok(p) => p,
        Err(e) => return stripe_err_response(e),
    };
    let fee_cents = policy.fee_cents(body.amount_cents);

    let stripe = crate::stripe_client::StripeClient::new(crate::SecretString::new(
        state.stripe_secret_key.expose_secret().to_string(),
    ))
    .with_base_url(state.stripe_base_url.clone());

    // Deterministic idempotency key per (creator, cart, amount, currency) so an
    // at-least-once retry of the SAME cart replays the same PaymentIntent, but a
    // changed amount/currency (or a different cart) gets a DISTINCT key — Stripe
    // can no longer replay a stale intent for a different charge (M2). `cart` is
    // guaranteed non-empty (validated above).
    let idempotency_key = format!(
        "connect_pi:{creator_id}:{cart}:{}:{}",
        body.amount_cents, body.currency
    );
    // m4: cap the description before it hits the wire. Stripe's PaymentIntent
    // `description` max is 1000 chars; a longer one is a 400 `string_too_long`.
    // Done here (not in the client) so the cap is visible at the creator-facing
    // boundary where the value originates.
    let raw_description = body.description.as_deref().unwrap_or("zeroship connect charge");
    let description: &str = cap_stripe_description(raw_description);

    match stripe
        .create_connect_payment_intent(
            &account.stripe_account_id,
            body.amount_cents,
            &body.currency,
            fee_cents,
            description,
            &idempotency_key,
        )
        .await
    {
        Ok(pi) => web::HttpResponse::Ok().json(&serde_json::json!({
            "payment_intent_id": pi.id,
            "client_secret": pi.client_secret,
            // Echo the SERVER-resolved fee for transparency (read-only).
            "application_fee_cents": fee_cents,
        })),
        Err(e) => stripe_err_response(e),
    }
}

// ----------------------------------------------------------------
// Fee policy administration (OPERATOR-ONLY — ISS-29)
// ----------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct FeePolicyBody {
    /// "fixed" | "percent".
    pub kind: String,
    /// kind="fixed": the flat fee in cents.
    #[serde(default)]
    pub amount_cents: Option<u64>,
    /// kind="percent": basis points (1500 = 15%).
    #[serde(default)]
    pub percent_bps: Option<u32>,
    #[serde(default)]
    pub cap_cents: Option<u64>,
    #[serde(default)]
    pub floor_cents: Option<u64>,
}

/// `PUT /api/creators/:id/fee-policy` — set a creator's application-fee policy.
///
/// **OPERATOR-ONLY (ISS-29).** A creator self-editing their own fee is a
/// privilege escalation (they could zero it). This handler ALWAYS requires Cedar
/// `BillingWrite` on `Resource::Any` (the operator/master-key grant) — there is
/// NO self-service branch, even when the principal IS the path creator. The fee
/// lives on the server and only the platform may change it.
pub async fn set_fee_policy(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    body: web::types::Json<FeePolicyBody>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = rate_limit(&req, &state.admin_limiter, "admin", &state).await { return r; }
    // OPERATOR-ONLY — no self-service branch. A creator principal is denied even
    // for their OWN id.
    if let Err(resp) = authz.require(AuthzAction::BillingWrite, Resource::Any, &state).await {
        return resp;
    }
    let Ok(creator_id) = Uuid::parse_str(&path) else { return bad_creator_id(); };

    let policy = match body.kind.as_str() {
        "fixed" => {
            let Some(amount) = body.amount_cents else {
                return err_json(400, "fixed fee requires amount_cents");
            };
            crate::fee_policy::FeePolicy::Fixed { amount_cents: amount }
        }
        "percent" => {
            let Some(bps) = body.percent_bps else {
                return err_json(400, "percent fee requires percent_bps");
            };
            if bps > 10_000 {
                return err_json(400, "percent_bps must be in [0, 10000]");
            }
            // m2: a floor above the cap pins every fee to the cap regardless of
            // percent — almost certainly an operator typo. Reject it (the DB has
            // a matching CHECK as defense in depth).
            if let (Some(floor), Some(cap)) = (body.floor_cents, body.cap_cents) {
                if floor > cap {
                    return err_json(400, "floor_cents must not exceed cap_cents");
                }
            }
            crate::fee_policy::FeePolicy::Percent {
                bps,
                cap_cents: body.cap_cents,
                floor_cents: body.floor_cents,
            }
        }
        other => return err_json(400, format!("unknown fee policy kind: {}", stripe_store::sanitize_for_display(other))),
    };

    let store = crate::fee_policy::FeePolicyStore::new(state.registry.clone());
    match store.set(creator_id, policy).await {
        Ok(()) => {
            let ip = source_ip(&req, &state);
            audit::log_with_detail(&state.registry, AuditEntry {
                app_id: None,
                creator_id: Some(creator_id),
                actor_user_id: Some(authz.principal_id),
                actor_token_id: authz.token_id,
                action: Action::SetFeePolicy,
                resource: None,
                source_ip: ip.as_deref(),
            }, &serde_json::json!({
                "creator_id": creator_id.to_string(),
                "kind": body.kind,
                "amount_cents": body.amount_cents,
                "percent_bps": body.percent_bps,
                "cap_cents": body.cap_cents,
                "floor_cents": body.floor_cents,
            })).await;
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
    /// Connect webhooks carry a TOP-LEVEL `account` (`acct_…`) identifying the connected
    /// account the event came from (verified docs.stripe.com/connect/webhooks: "Each event
    /// for a connected account contains a top-level `account` property"). The Connect
    /// failure handlers (`payout.failed` / `payment_intent.payment_failed`) resolve the
    /// creator through this when the object itself doesn't carry the `acct_…`. Absent on
    /// platform-account events.
    #[serde(default)]
    account: Option<String>,
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
    // ── Dispute (charge.dispute.*) fields (PR-8) ────────────────────────────
    /// The disputed amount (network-held), in cents. On a dispute object this is the
    /// clawback amount; on an invoice object it is unset.
    #[serde(default)]
    amount: Option<i64>,
    /// The Stripe dispute `status` (`needs_response`/`under_review`/`won`/`lost`/…).
    #[serde(default)]
    status: Option<String>,
    /// The dispute `reason` (`fraudulent`/`duplicate`/…).
    #[serde(default)]
    reason: Option<String>,
    /// The disputed charge (`ch_…`). One of the candidates we resolve back to an invoice.
    #[serde(default)]
    charge: Option<String>,
    /// The disputed PaymentIntent (`pi_…`). Another resolution candidate.
    #[serde(default)]
    payment_intent: Option<String>,
    /// `evidence_details.due_by` — the evidence-submission deadline.
    #[serde(default)]
    evidence_details: Option<DisputeEvidenceDetails>,
    /// Modern (Basil 2025-03-31+) Invoice wire surface: the per-payment settlement objects
    /// moved OFF the top level (`invoice.payment_intent`/`invoice.charge`) into
    /// `invoice.payments.data[].payment.{payment_intent,charge}`. On a paid invoice this is
    /// where the `pi_…`/`ch_…` live so we can record the dispute-resolution linkage; the
    /// legacy top-level `payment_intent`/`charge` fields above cover older API versions.
    #[serde(default)]
    payments: Option<InvoicePayments>,
    // ── Connect Account (account.updated) fields (M2) ───────────────────────
    /// `charges_enabled` on a connected `account` object — Stripe flips this to
    /// FALSE on a risk/KYC hold. The `account.updated` handler caches it so the
    /// `connect_checkout` gate reflects reality. Unset on non-account objects.
    #[serde(default)]
    charges_enabled: Option<bool>,
    /// `payouts_enabled` on a connected `account` object (M2).
    #[serde(default)]
    payouts_enabled: Option<bool>,
    /// `details_submitted` on a connected `account` object (M2).
    #[serde(default)]
    details_submitted: Option<bool>,
    // ── Connect revenue attribution (M4) ────────────────────────────────────
    /// The connected account the charge settled ON BEHALF OF (`acct_…`). On a
    /// Connect-revenue invoice / charge this is the destination account that
    /// actually received the funds. The payout handler confirms it matches the
    /// claimed creator's `creator_accounts.stripe_account_id` before crediting —
    /// so a forged `metadata.creator_id` cannot attribute another account's
    /// revenue to itself.
    #[serde(default)]
    on_behalf_of: Option<String>,
    /// `transfer_data[destination]` (`acct_…`) — the other place the destination
    /// connected account surfaces on a Connect charge/PaymentIntent. Checked as a
    /// fallback when `on_behalf_of` is absent.
    #[serde(default)]
    transfer_data: Option<TransferData>,
    // ── Connect failure fields (payout.failed / payment_intent.payment_failed) ───────
    /// `failure_code` on a Payout object (`account_closed`/`insufficient_funds`/… —
    /// verified docs.stripe.com/api/payouts/object). Unset on non-payout objects.
    #[serde(default)]
    failure_code: Option<String>,
    /// `failure_message` on a Payout object (human-readable failure reason).
    #[serde(default)]
    failure_message: Option<String>,
    /// `last_payment_error` on a PaymentIntent object (`payment_intent.payment_failed`):
    /// the decline `{code, message}`. Unset on non-PI objects / a PI with no error.
    #[serde(default)]
    last_payment_error: Option<LastPaymentError>,
}

#[derive(Deserialize, Debug)]
struct LastPaymentError {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize, Debug)]
struct TransferData {
    #[serde(default)]
    destination: Option<String>,
}

#[derive(Deserialize, Debug)]
struct InvoicePayments {
    #[serde(default)]
    data: Vec<InvoicePaymentEntry>,
}

#[derive(Deserialize, Debug)]
struct InvoicePaymentEntry {
    #[serde(default)]
    payment: Option<InvoicePaymentObject>,
}

#[derive(Deserialize, Debug)]
struct InvoicePaymentObject {
    #[serde(default)]
    payment_intent: Option<String>,
    #[serde(default)]
    charge: Option<String>,
}

#[derive(Deserialize, Debug)]
struct DisputeEvidenceDetails {
    /// Unix seconds by which evidence must be submitted (nullable).
    #[serde(default)]
    due_by: Option<i64>,
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

/// `true` iff the invoice carries the platform's POSITIVE infra marker
/// (`metadata.invoice_kind == "infra"`, stamped by the billing reconciler's
/// `create_invoice`). This is the recovery gate (critic #6): an `invoice.paid`
/// without this marker is NOT a platform infra invoice — even if its Stripe
/// Customer reverse-resolves to a `creator_billing.stripe_customer_id` (a Connect
/// end-user invoice could collide) — so it must NOT un-suspend a creator. The
/// marker is checked across the same wire locations as `creator_id` because
/// Stripe surfaces invoice metadata directly and via subscription details.
fn is_infra_invoice(obj: &StripeObject) -> bool {
    let has = |m: &serde_json::Map<String, serde_json::Value>| {
        m.get("invoice_kind").and_then(|v| v.as_str()) == Some("infra")
    };
    obj.metadata.as_ref().is_some_and(has)
        || obj.parent.as_ref().and_then(|p| p.subscription_details.as_ref())
            .and_then(|sd| sd.metadata.as_ref()).is_some_and(has)
        || obj.subscription_details.as_ref()
            .and_then(|sd| sd.metadata.as_ref()).is_some_and(has)
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

    // ── Replay-dedup (billing G6) ──────────────────────────────────────────
    // Order is load-bearing: signature verification ran FIRST (above), so a
    // forged/unsigned event never reaches the ledger or the handlers. Only a
    // VERIFIED event is checked here. Stripe re-delivers at-least-once; a
    // previously-PROCESSED event-id is 200-acked WITHOUT re-dispatching so the
    // setup_intent / payment_failed / invoice.paid handlers don't re-run their
    // side effects (and don't emit a duplicate audit row).
    //
    // CLAIM-AFTER-SUCCESS: the ledger is written only AFTER the handler returns
    // 2xx (see the tail of this fn). A handler that errored is NOT recorded, so
    // Stripe's retry re-processes it — exactly-once EFFECTIVE (no double-process
    // AND no lost event on handler failure).
    //
    // M3 (dedup concurrency): `event_processed` → dispatch → `mark_event_processed`
    // is a check-then-act across un-serialized connections, so two CONCURRENT
    // redeliveries of the SAME event could both pass the check and both dispatch
    // (saved today only by per-handler idempotency — the ledger does nothing under
    // concurrency). Take a SESSION advisory lock keyed on the event id FIRST so
    // same-event deliveries serialize: the second waits for the first to release,
    // by which point the first has CLAIMED the event, so the second 200-acks as a
    // duplicate. A session lock (not xact) is required because the dispatch spans
    // PG→HTTP→PG with fresh per-step connections (no single long transaction). The
    // lock is held on a dedicated connection and released explicitly below (drop
    // also releases it). Claim-after-success fail-closed semantics are unchanged.
    let lock_conn = match state.stripe_store.lock_event(&event.id).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "stripe: failed to take per-event advisory lock — failing closed");
            return err_json(500, "internal error");
        }
    };
    let resp = process_locked_event(&req, &state, &event, raw).await;
    crate::stripe_store::StripeStore::unlock_event(&lock_conn, &event.id).await;
    drop(lock_conn);
    resp
}

/// Run the dedup-checked dispatch for ONE event WHILE the per-event advisory lock
/// is held (M3). Factored out of [`webhook`] so the lock is released on EVERY
/// return path (the caller unlocks after this returns). Returns the HTTP response.
async fn process_locked_event(
    req: &web::HttpRequest,
    state: &AppState,
    event: &StripeEvent,
    raw: &[u8],
) -> web::HttpResponse {
    match state.stripe_store.event_processed(&event.id).await {
        Ok(true) => {
            // Already processed on a prior delivery — idempotent ack.
            return web::HttpResponse::Ok().json(&serde_json::json!({"status": "duplicate"}));
        }
        Ok(false) => { /* first delivery — dispatch below */ }
        Err(e) => {
            // Ledger unreachable: fail CLOSED with a retryable 5xx rather than
            // risk processing without a dedup guard. Stripe retries.
            tracing::error!(error = %e, "stripe: replay-dedup ledger check failed");
            return err_json(500, "internal error");
        }
    }

    let resp = dispatch_event(req, state, event, raw).await;

    // Record PROCESSED only on handler success (2xx). On a non-2xx the event is
    // left unclaimed so Stripe's retry re-processes it (no lost event).
    if resp.status().is_success() {
        if let Err(e) = state
            .stripe_store
            .mark_event_processed(&event.id, &event.event_type)
            .await
        {
            // The handler already applied its (idempotent) effect; failing to
            // record the dedup row only means a redelivery re-runs an
            // idempotent handler. Log and still ack — do NOT 5xx, which would
            // force a guaranteed redelivery of an already-applied event.
            tracing::error!(error = %e, "stripe: failed to record processed event in dedup ledger");
        }
    }
    resp
}

/// Dispatch a SIGNATURE-VERIFIED, NOT-YET-PROCESSED webhook event to its
/// handler. Returns the HTTP response; the caller (`webhook`) records the
/// event-id as processed iff this returns 2xx (claim-after-success).
async fn dispatch_event(
    req: &web::HttpRequest,
    state: &AppState,
    event: &StripeEvent,
    raw: &[u8],
) -> web::HttpResponse {
    let obj = &event.data.object;

    // Stream-1 (infra-billing) lifecycle events. These ride the SAME verified
    // ingest path and resolve the creator via the same `extract_creator_id`
    // (metadata.creator_id, stamped by `create_customer`).
    match event.event_type.as_str() {
        "setup_intent.succeeded" => {
            return handle_setup_intent_succeeded(req, state, event, obj).await;
        }
        "invoice.payment_failed" => {
            return handle_invoice_payment_failed(req, state, event, obj).await;
        }
        // DISPUTES / CHARGEBACKS (billing-ops PR-8, design flow I). A cardholder disputed
        // a charge; Stripe held the funds. Record the dispute + the cash clawback (a
        // negative dispute_debit invoice_payments row), claim-after-success on
        // stripe_events_seen like every branch. `.created` opens the dispute (+ fires the
        // `disputed` notification via the cron off the new billing_disputes row);
        // `.closed` resolves it (won → a compensating dispute_reversal restores the
        // budget; lost → the debit stands). `.updated` mid-lifecycle is recorded too (a
        // terminal status on an update is treated like a close).
        "charge.dispute.created" => {
            return handle_dispute_created(req, state, event, obj).await;
        }
        "charge.dispute.closed" | "charge.dispute.updated" => {
            return handle_dispute_closed_or_updated(req, state, event, obj).await;
        }
        // M2 (the money hole): a connected account whose `charges_enabled`/
        // `payouts_enabled` Stripe flips to FALSE (risk/KYC) must be observed —
        // `connect_checkout` gates on the CACHED flag, so a stale `true` would let a
        // disabled account keep taking charges. Update the cached flags for the
        // `acct_…` so the gate reflects reality.
        "account.updated" => {
            return handle_account_updated(req, state, event, obj).await;
        }
        // M2 dispute FUNDS events. The cash movement has a SINGLE source of truth: the
        // dispute LIFECYCLE handlers (`.created` debits, `.closed won` reverses). The
        // funds-flow events (`funds_withdrawn`/`funds_reinstated`) describe the SAME
        // money already accounted there, so acting on them too would DOUBLE-count. We
        // therefore treat them as AUDIT-ONLY no-ops (record nothing to the ledger) — a
        // loud info line, then ack. This keeps `Σ(invoice_payments)` driven by exactly
        // one rail (no double-debit / double-restore).
        "charge.dispute.funds_withdrawn" | "charge.dispute.funds_reinstated" => {
            tracing::info!(
                event_id = %sanitize_event_id(&event.id),
                event_type = %event.event_type,
                "stripe: dispute funds event acked (audit-only; cash movement is owned by the dispute lifecycle handlers, no double-count)"
            );
            return web::HttpResponse::Ok().json(&serde_json::json!({"status": "funds_event_audited"}));
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
            // RECOVERY GATE (critic #6): only a PLATFORM INFRA invoice may
            // un-suspend a creator. Gate on the POSITIVE `invoice_kind=infra`
            // marker the reconciler stamps — NOT on the mere absence of Connect
            // metadata. A Connect end-user `invoice.paid` whose Stripe Customer
            // happens to collide with a platform `creator_billing.stripe_customer_id`
            // lacks this marker, so it can never falsely recover a suspension.
            // (`billing_runs` match is the defense-in-depth alternative, but the
            // marker is the load-bearing signal and is present on every
            // reconciler-created invoice.)
            if is_infra_invoice(obj) {
                if let Some(cid) = resolve_infra_creator(state, obj).await {
                    let store = crate::account_status::AccountStatusStore::new(state.registry.clone());
                    match store.record_payment_recovered(cid, event.created).await {
                        Ok(Some(t)) => audit_account_transition(req, state, &t, &event.id).await,
                        Ok(None) => { /* nothing to recover (already active / no row) */ }
                        Err(e) => {
                            tracing::error!(error = %e, "stripe: invoice.paid status recovery failed");
                        }
                    }
                }
                // PAYMENTS (billing-ops PR-1, design CRITICAL-A): record the cash actually
                // collected as an APPEND-ONLY `invoice_payments` row — NEVER a mutation of
                // the finalized invoice (the immutability trigger forbids it; that is
                // precisely why payment tracking is a side table). cash-collected =
                // Σ(invoice_payments) anchors PR-3's over-refund cap. Map the Stripe invoice
                // (`in_…`) back to the internal `zeroship.invoices.id` via
                // `billing_provider_refs`; skip a $0 fully-credit-covered invoice (no charge
                // ⇒ no row ⇒ cash-collected stays 0).
                //
                // FAIL-CLOSED (gap #26 review, MAJOR-2): a TRANSIENT append failure must
                // NOT be swallowed — that would mark the event processed (claim-after-
                // success below) and permanently DROP a cash row (under-counting
                // cash_collected forever, since the redelivery is acked as a duplicate).
                // Instead propagate the error so the webhook returns non-2xx → the event is
                // left UNCLAIMED → Stripe retries. The retry safely re-appends because the
                // append is now idempotent on `provider_ref` (CRITICAL-1: ON CONFLICT DO
                // NOTHING), so the duplicate-write window the old ordering opened is closed.
                if let Err(e) = record_infra_payment(state, obj).await {
                    tracing::error!(error = %e, "stripe: invoice.paid payment-row append failed — failing closed for retry");
                    return err_json(500, "internal error");
                }
                // D3: an INFRA `invoice.paid` is FULLY handled here — ACK 200 and
                // RETURN. It must NOT fall through to the Stream-2 `record_payout`
                // path below, whose `payouts.creator_id → creator_accounts(creator_id)`
                // FK an infra-only creator (no Connect account) cannot satisfy. The
                // fall-through 500'd AFTER the infra writes committed (non-atomic),
                // so the event never acked and Stripe retried forever (poison). The
                // payout path is for Connect REVENUE events, which are NOT infra
                // invoices and are still reached for non-infra `invoice.paid` below.
                return web::HttpResponse::Ok().json(&serde_json::json!({"status": "infra_recorded"}));
            }
        }
        // MONEY-CRITICAL: a refund we recorded `issued` that LATER transitioned to a
        // terminal FAILURE (`failed`/`canceled`) — the cash did NOT return to the
        // cardholder. Reconcile our ledger: flip the refund to failed (so the over-refund
        // cap stops counting it → the creator can re-refund) and, for a credit-destination
        // refund, claw back the minted `refund_to_credit` grant. Idempotent + fail-closed.
        "charge.refund.updated" => {
            return handle_refund_updated(req, state, event, obj).await;
        }
        // A payout to a creator's connected account FAILED (bad bank details / closed
        // account). Record it in `payout_failures` (idempotent on the `po_…`) and notify
        // the creator via the `payout_failed` notification (off the new row, by the cron).
        "payout.failed" => {
            return handle_payout_failed(req, state, event, obj).await;
        }
        // An end-user's Connect checkout charge FAILED. No money moved (informational), but
        // it must be SURFACED, not silently dropped — record it in
        // `connect_checkout_failures` (idempotent on the `pi_…`) + a `checkout_failed`
        // creator notification.
        "payment_intent.payment_failed" => {
            return handle_payment_intent_failed(req, state, event, obj).await;
        }
        // MUST-ENABLE webhook event set (configure these on the Stripe endpoint):
        //   setup_intent.succeeded, invoice.payment_failed, invoice.paid,
        //   account.updated, charge.dispute.created, charge.dispute.closed,
        //   charge.dispute.updated, charge.dispute.funds_withdrawn,
        //   charge.dispute.funds_reinstated,
        //   charge.refund.updated, payout.failed, payment_intent.payment_failed
        //   (all handled above — none deferred).
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

    // M4 (payout attribution): `metadata.creator_id` is CLIENT-influenced — a
    // forged or copy-pasted id could attribute ANOTHER account's revenue to a
    // different creator. Resolve the connected account the charge actually settled
    // ON BEHALF OF (`on_behalf_of`, else `transfer_data.destination`) and confirm it
    // is the claimed creator's OWN live `creator_accounts.stripe_account_id` before
    // crediting earnings. A mismatch (or a missing settling account on a revenue
    // event) is REJECTED — not credited.
    let settling_account = obj
        .on_behalf_of
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            obj.transfer_data
                .as_ref()
                .and_then(|t| t.destination.as_deref())
                .map(str::trim)
                .filter(|s| !s.is_empty())
        });
    let Some(settling_account) = settling_account else {
        tracing::warn!(
            event_id = %sanitize_event_id(&event.id),
            "stripe: connect-revenue invoice.paid carries no settling account (on_behalf_of / transfer_data.destination) — refusing to credit a payout"
        );
        return web::HttpResponse::Ok().json(&serde_json::json!({"status": "no_settling_account"}));
    };
    match state
        .stripe_store
        .account_belongs_to_creator(creator_id, settling_account)
        .await
    {
        Ok(true) => { /* attribution verified — the claimed creator owns the settling account */ }
        Ok(false) => {
            tracing::warn!(
                event_id = %sanitize_event_id(&event.id),
                creator_id = %creator_id,
                settling_account = %stripe_store::sanitize_for_display(settling_account),
                "stripe: payout attribution MISMATCH — claimed creator does not own the settling account; refusing to credit"
            );
            return web::HttpResponse::Ok().json(&serde_json::json!({"status": "attribution_mismatch"}));
        }
        Err(e) => {
            // FAIL CLOSED on a transient lookup error — retry rather than credit blind.
            tracing::error!(error = %e, "stripe: payout attribution check failed — failing closed for retry");
            return err_json(500, "internal error");
        }
    }

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
            let ip = source_ip(req, state);
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

/// `account.updated` (M2, the money hole). A connected account's onboarding /
/// capability flags changed at Stripe — most importantly `charges_enabled` /
/// `payouts_enabled` being flipped to FALSE on a risk / KYC hold. The
/// `connect_checkout` gate reads the CACHED `charges_enabled`, so a stale `true`
/// would let a now-disabled account keep taking charges. Re-cache the flags from
/// the event's account object so the gate reflects Stripe's truth.
///
/// The account object's `id` IS the `acct_…`; we update the live row by that id
/// (globally unique). An `account.updated` for an `acct_…` we never linked (a
/// platform/express account not in `creator_accounts`) is a benign no-op ack.
async fn handle_account_updated(
    req: &web::HttpRequest,
    state: &AppState,
    event: &StripeEvent,
    obj: &StripeObject,
) -> web::HttpResponse {
    let Some(account_id) = obj.id.as_deref().filter(|s| !s.is_empty()) else {
        tracing::warn!(event_id = %sanitize_event_id(&event.id), "stripe: account.updated missing account id — ignored");
        return web::HttpResponse::Ok().json(&serde_json::json!({"status": "missing_account_id"}));
    };
    // Default a missing flag to FALSE — the fail-closed direction (a charge gate
    // should never be opened by an absent field).
    let charges_enabled = obj.charges_enabled.unwrap_or(false);
    let payouts_enabled = obj.payouts_enabled.unwrap_or(false);
    let details_submitted = obj.details_submitted.unwrap_or(false);

    match state
        .stripe_store
        .update_account_flags_by_account_id(
            account_id,
            charges_enabled,
            payouts_enabled,
            details_submitted,
        )
        .await
    {
        Ok(matched) => {
            if matched {
                let ip = source_ip(req, state);
                audit::log_with_detail(
                    &state.registry,
                    AuditEntry {
                        app_id: None,
                        creator_id: None,
                        actor_user_id: None,
                        actor_token_id: None,
                        action: Action::AccountStateChange,
                        resource: Some(&event.id),
                        source_ip: ip.as_deref(),
                    },
                    &serde_json::json!({
                        "stripe_event_type": "account.updated",
                        "stripe_account_id": account_id,
                        "charges_enabled": charges_enabled,
                        "payouts_enabled": payouts_enabled,
                        "details_submitted": details_submitted,
                    }),
                )
                .await;
            }
            web::HttpResponse::Ok().json(&serde_json::json!({
                "status": if matched { "account_flags_updated" } else { "account_not_linked" },
            }))
        }
        Err(e) => {
            // FAIL CLOSED: a transient error updating the gate flags must retry, not
            // be acked — otherwise a risk-disabled account stays cached as enabled.
            tracing::error!(error = %e, "stripe: account.updated flag cache update failed — failing closed for retry");
            stripe_err_response(e)
        }
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
    // unsigned event never reaches this function. Order-safe: `event.created` is
    // threaded so a stale failure that predates a recovery can't re-arm past_due.
    if let Some(cid) = creator_id {
        let store = crate::account_status::AccountStatusStore::new(state.registry.clone());
        match store.record_payment_failed(cid, obj.id.as_deref(), event.created).await {
            Ok(Some(t)) => {
                audit_account_transition(req, state, &t, &event.id).await;
            }
            Ok(None) => { /* no state change (redelivery / already past_due/suspended) */ }
            Err(e) => {
                // M1: FAIL CLOSED. A transient error recording the dunning transition
                // must NOT be swallowed-then-acked — that would mark the event processed
                // (claim-after-success), Stripe would never redeliver, and the creator
                // would never enter dunning (keeps consuming free infra on a dead card).
                // Return 5xx so the event is left UNCLAIMED for retry, matching the
                // infra-`invoice.paid` discipline. The transition is idempotent
                // (last_recovered_at high-water + the active→past_due guard), so a retry
                // re-arms exactly once.
                tracing::error!(error = %e, "stripe: invoice.payment_failed status update failed — failing closed for retry");
                return err_json(500, "internal error");
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

/// Append a `charge` `invoice_payments` row for a paid INFRA invoice (billing-ops
/// PR-1). Maps the Stripe invoice (`obj.id`, an `in_…`) back to the internal
/// `zeroship.invoices.id` via `billing_provider_refs`, then appends the cash
/// actually collected (`obj.amount_paid`) WITHOUT touching the finalized invoice.
/// A $0 invoice (fully credit-covered, or no `amount_paid`) records NO row, so
/// cash-collected stays 0 — exactly right.
///
/// FAIL-CLOSED (gap #26 review, MAJOR-2): a TRANSIENT DB failure (conn, ref lookup,
/// or the append itself) is PROPAGATED so the caller can return non-2xx and leave
/// the event UNCLAIMED for Stripe to retry — never silently dropped (which would
/// permanently lose a cash row). The retry is safe because `append_charge` is
/// idempotent on `provider_ref`. Cases that are legitimately "nothing to anchor"
/// (missing invoice id, no internal invoice ref, $0 cash) are NOT errors — they
/// return `Ok(())` so the webhook still acks 200.
async fn record_infra_payment(
    state: &AppState,
    obj: &StripeObject,
) -> Result<(), crate::registry::RegistryError> {
    let Some(provider_invoice_id) = obj.id.as_deref() else {
        tracing::warn!("stripe: infra invoice.paid missing invoice id — no payment row appended");
        return Ok(());
    };
    let amount = obj.amount_paid.unwrap_or(0);
    if amount <= 0 {
        // $0 fully-credit-covered invoice (or no cash) ⇒ no charge ⇒ no row.
        return Ok(());
    }
    let currency = obj.currency.clone().unwrap_or_else(|| "usd".into());

    // (1) Resolve the internal invoice + APPEND the cash charge row first. The
    // connection is scoped to this block and DROPPED before the HTTP fetch below so we
    // never hold a PG connection across a cyper `.await` (the same fresh-conn-per-step
    // discipline the Connect `callback`/`onboard` handlers use: PG → HTTP → PG).
    let internal_id = {
        let conn = state.registry.conn().await?;
        let internal_id = match crate::invoice_payments::invoice_id_for_provider_invoice(
            &conn,
            provider_invoice_id,
        )
        .await?
        {
            Some(id) => id,
            None => {
                // No finalized internal invoice maps to this Stripe invoice (a
                // pre-reconciler-finalize race, or not a platform invoice). Nothing
                // to anchor a payment against — not an error, ack the webhook.
                tracing::warn!(
                    "stripe: infra invoice.paid has no internal invoice ref — no payment row appended"
                );
                return Ok(());
            }
        };
        let pay_id = crate::invoice_payments::append_charge(
            &conn,
            &internal_id,
            amount,
            &currency,
            Some(provider_invoice_id),
        )
        .await?;
        tracing::info!(
            invoice_id = %internal_id,
            payment_id = %pay_id,
            amount_cents = amount,
            "stripe: appended invoice_payments charge row"
        );
        internal_id
        // `conn` dropped here (before the HTTP fetch).
    };

    // (2) DISPUTE-RESOLUTION LINKAGE (PR-8 CRITICAL-1). A future `charge.dispute.*`
    // carries only the settling `pi_…`/`ch_…` (never the `in_…`). Capture those for THIS
    // paid invoice and persist them as
    // `billing_provider_refs(ref_kind='payment_intent'|'charge')` so the dispute handler
    // can resolve back to us.
    //
    // D2 (real-Stripe): the delivered `invoice.paid` event payload carries NEITHER a
    // top-level `payment_intent`/`charge` NOR an inline `payments` list on API
    // 2025-09-30.clover (Basil 2025-03-31+) — reading them off `obj` records NOTHING, so
    // a real dispute could never resolve. A webhook payload cannot be expanded, so FETCH
    // the settlement ids via an expanded retrieve
    // (`expand[]=payments.data.payment.payment_intent`) and record THOSE. We fall back to
    // any ids already inline on `obj` (the faithful-envelope / legacy shape).
    //
    // FAIL-CLOSED: a transient fetch failure propagates → non-2xx → the event is left
    // UNCLAIMED → Stripe retries. The charge row already committed, but `append_charge`
    // is idempotent on `provider_ref` and `record_payment_object_refs` is ON CONFLICT DO
    // NOTHING, so the retry never double-writes.
    let (pi, ch) = settlement_ids_for(state, provider_invoice_id, obj)
        .await
        .map_err(|e| {
            crate::registry::RegistryError::Database(format!(
                "stripe: invoice.paid settlement-id fetch failed: {e}"
            ))
        })?;

    // (3) Record the fetched linkage on a FRESH connection (post-HTTP). This ALSO promotes
    // any dispute that raced ahead of this invoice.paid (parked in pending_disputes), so the
    // dispute-vs-linkage ordering is symmetric (0055): a created-before-paid resolves here.
    let mut conn = state.registry.conn().await?;
    crate::invoice_payments::record_payment_object_refs(
        &mut conn,
        &internal_id,
        pi.as_deref(),
        ch.as_deref(),
    )
    .await?;
    Ok(())
}

/// Resolve the settling `(payment_intent, charge)` for an infra `invoice.paid` (D2).
///
/// PREFER ids already inline on the webhook `obj` (the legacy top-level shape, or the
/// faithful Basil `payments.data[].payment.{payment_intent,charge}` envelope a correct
/// integration would deliver). If NEITHER is present — the real-Stripe default on API
/// 2025-09-30.clover — FETCH the invoice with `expand[]=payments.data.payment.payment_intent`
/// and read the ids from the expanded object. A `StripeClient` is built from the same
/// `state.stripe_secret_key`/`stripe_base_url` the other handlers use; when no secret is
/// configured (a unit fixture exercising only the inline shape) the fetch is skipped.
async fn settlement_ids_for(
    state: &AppState,
    provider_invoice_id: &str,
    obj: &StripeObject,
) -> Result<(Option<String>, Option<String>), crate::stripe_store::StripeError> {
    let (pi, ch) = invoice_payment_object_ids(obj);
    if pi.is_some() || ch.is_some() {
        return Ok((pi, ch));
    }
    if state.stripe_secret_key.expose_secret().is_empty() {
        // No Stripe credential (a unit fixture); nothing to fetch — the inline shape
        // was the only source. Not an error: a $0/credit invoice legitimately has none.
        return Ok((None, None));
    }
    let stripe = crate::stripe_client::StripeClient::new(crate::SecretString::new(
        state.stripe_secret_key.expose_secret().to_string(),
    ))
    .with_base_url(state.stripe_base_url.clone());
    stripe.invoice_settlement_ids(provider_invoice_id).await
}

/// Extract the settling PaymentIntent (`pi_…`) and Charge (`ch_…`) from a paid Stripe
/// Invoice object, robust across API versions:
///   * legacy (pre-Basil): top-level `invoice.payment_intent` / `invoice.charge`.
///   * modern (Basil 2025-03-31+): `invoice.payments.data[].payment.{payment_intent,charge}`
///     (the top-level fields were removed when multiple partial payments shipped).
/// Returns the FIRST non-empty id seen for each, preferring the top-level (older) shape and
/// falling back to the nested entries.
fn invoice_payment_object_ids(obj: &StripeObject) -> (Option<String>, Option<String>) {
    let nonempty = |s: &Option<String>| s.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    let mut pi = nonempty(&obj.payment_intent);
    let mut ch = nonempty(&obj.charge);
    if let Some(payments) = obj.payments.as_ref() {
        for entry in &payments.data {
            let Some(p) = entry.payment.as_ref() else { continue };
            if pi.is_none() {
                pi = nonempty(&p.payment_intent);
            }
            if ch.is_none() {
                ch = nonempty(&p.charge);
            }
        }
    }
    (pi, ch)
}

/// `charge.dispute.created` (billing-ops PR-8, design flow I). A cardholder disputed a
/// charge; Stripe held the funds. We:
///   1. resolve the disputed Stripe payment object back to our internal invoice (mirroring
///      `invoice.paid`'s resolution),
///   2. in ONE txn, UPSERT a `billing_disputes` row (`status='open'`) AND append a
///      NEGATIVE `dispute_debit` `invoice_payments` row (= cash clawed back). The negative
///      row lowers `Σ(invoice_payments)`, so PR-3's over-refund cap auto-tightens — no
///      cross-table trigger.
/// The dispute is NEVER auto-refunded (the funds already moved) and NEVER mutates the
/// invoice. The `disputed` notification fires once via the notify cron off the new row.
///
/// FAIL-CLOSED: a transient DB failure propagates → non-2xx → the event is left UNCLAIMED
/// → Stripe retries. The record is idempotent on the `du_…` (the dispute-row UNIQUE + the
/// dispute payment-row dedup index), so a retry never double-debits.
async fn handle_dispute_created(
    req: &web::HttpRequest,
    state: &AppState,
    event: &StripeEvent,
    obj: &StripeObject,
) -> web::HttpResponse {
    let Some(provider_dispute_id) = obj.id.as_deref().filter(|s| !s.is_empty()) else {
        tracing::warn!(event_id = %sanitize_event_id(&event.id), "stripe: charge.dispute.created missing dispute id — ignored");
        return web::HttpResponse::Ok().json(&serde_json::json!({"status": "missing_dispute_id"}));
    };
    let amount = obj.amount.unwrap_or(0);
    if amount <= 0 {
        tracing::warn!(event_id = %sanitize_event_id(&event.id), "stripe: charge.dispute.created non-positive amount — ignored");
        return web::HttpResponse::Ok().json(&serde_json::json!({"status": "ignored_zero_amount"}));
    }
    let currency = obj.currency.clone().unwrap_or_else(|| "usd".into());

    let conn = match state.registry.conn().await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "stripe: dispute.created conn failed — failing closed");
            return err_json(500, "internal error");
        }
    };
    // Resolve the disputed object → our invoice via the pi_/ch_ linkage recorded at
    // invoice.paid (CRITICAL-1). A Stripe Dispute object has NO `invoice` field — only
    // `payment_intent` (pi_…) and `charge` (ch_…) — so those are the only candidates.
    let candidates: Vec<&str> = [obj.payment_intent.as_deref(), obj.charge.as_deref()]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    // BUG-4: a dispute with NEITHER a `payment_intent` NOR a `charge` has NO settling object
    // to resolve OR to park against — `resolve_invoice_for_dispute` returns None and
    // `park_pending_dispute` would REJECT (it requires a candidate) → a 500 that Stripe
    // retries forever (poison). Stripe always sends at least one of pi_/ch_, so this is purely
    // defensive, but it must ACK like every other unrecognized-input ignore path (200 + warn),
    // NOT 5xx into a retry storm.
    if candidates.is_empty() {
        tracing::warn!(
            event_id = %sanitize_event_id(&event.id),
            "stripe: charge.dispute.created carries NO payment_intent/charge — no settling object to anchor; ignored"
        );
        return web::HttpResponse::Ok()
            .json(&serde_json::json!({"status": "dispute_no_settling_object"}));
    }
    let evidence_due_at = obj
        .evidence_details
        .as_ref()
        .and_then(|d| d.due_by)
        .and_then(|ts| chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0));

    let internal_id = match crate::disputes::resolve_invoice_for_dispute(&conn, &candidates).await {
        Ok(Some(id)) => id,
        Ok(None) => {
            // No pi_/ch_→invoice linkage YET. This is EITHER a charge we never invoiced (a
            // Connect end-user dispute) OR — the bug the live e2e surfaced — a
            // `charge.dispute.created` that raced AHEAD of its `invoice.paid` (which writes
            // the linkage). Dropping it here is NOT self-healing (the later invoice.paid never
            // re-checks; a Stripe resend is dedup-acked). So PARK the dispute facts in
            // `pending_disputes` (idempotent on the du_…); `record_payment_object_refs` will
            // promote it the moment the linkage lands. A genuinely-unrecognized charge parks
            // harmlessly and simply never resolves — no poison, no 5xx-retry storm (0055).
            let pending = crate::disputes::PendingDispute {
                provider_dispute_id,
                payment_intent: obj.payment_intent.as_deref(),
                charge: obj.charge.as_deref(),
                amount_cents: amount,
                currency: &currency,
                reason: obj.reason.as_deref(),
                evidence_due_at,
            };
            match crate::disputes::park_pending_dispute(&conn, &pending).await {
                Ok(parked) => {
                    tracing::warn!(
                        event_id = %sanitize_event_id(&event.id),
                        parked,
                        "stripe: charge.dispute.created has no invoice linkage yet — parked pending resolution"
                    );
                    return web::HttpResponse::Ok().json(&serde_json::json!({
                        "status": "dispute_pending_linkage",
                        "parked": parked,
                    }));
                }
                Err(e) => {
                    tracing::error!(error = %e, "stripe: dispute.created park failed — failing closed for retry");
                    return err_json(500, "internal error");
                }
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "stripe: dispute.created invoice resolution failed — failing closed");
            return err_json(500, "internal error");
        }
    };

    let mut conn = conn;
    let rec = match crate::disputes::record_dispute_created(
        &mut conn,
        &internal_id,
        amount,
        &currency,
        obj.reason.as_deref(),
        evidence_due_at,
        provider_dispute_id,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "stripe: dispute.created record failed — failing closed for retry");
            return err_json(500, "internal error");
        }
    };

    let ip = source_ip(req, state);
    audit::log_with_detail(
        &state.registry,
        AuditEntry {
            app_id: None,
            creator_id: None,
            actor_user_id: None,
            actor_token_id: None,
            action: Action::RecordDispute,
            resource: Some(&event.id),
            source_ip: ip.as_deref(),
        },
        &serde_json::json!({
            "stripe_event_type": "charge.dispute.created",
            "dispute_id": rec.dispute_id,
            "invoice_id": rec.invoice_id,
            "provider_dispute_id": provider_dispute_id,
            "amount_cents": amount,
            "newly_created": rec.newly_created,
        }),
    )
    .await;
    web::HttpResponse::Ok().json(&serde_json::json!({
        "status": "dispute_recorded",
        "dispute_id": rec.dispute_id,
    }))
}

/// `charge.dispute.closed` / `charge.dispute.updated` (PR-8). Progress an existing dispute
/// to its terminal status: `won` appends a compensating positive `dispute_reversal` row
/// (restoring the over-refund budget); `lost` leaves the `dispute_debit` standing. A
/// non-terminal `.updated` (still needs_response/under_review) is a no-op ack — the dispute
/// stays `open` and the debit stands. Idempotent on the `du_…`; a `.closed` arriving before
/// its `.created` (no dispute row yet) is acked.
async fn handle_dispute_closed_or_updated(
    req: &web::HttpRequest,
    state: &AppState,
    event: &StripeEvent,
    obj: &StripeObject,
) -> web::HttpResponse {
    let Some(provider_dispute_id) = obj.id.as_deref().filter(|s| !s.is_empty()) else {
        tracing::warn!(event_id = %sanitize_event_id(&event.id), "stripe: charge.dispute.closed/updated missing dispute id — ignored");
        return web::HttpResponse::Ok().json(&serde_json::json!({"status": "missing_dispute_id"}));
    };
    let stripe_status = obj.status.as_deref().unwrap_or("");
    // MINOR-6: surface an UNKNOWN Stripe dispute status (one not in our mapping table)
    // rather than silently bucketing it to `open`. A new Stripe lifecycle value should be
    // noticed, not swallowed.
    if !crate::disputes::DisputeStatus::is_known_stripe_status(stripe_status) {
        tracing::warn!(
            event_id = %sanitize_event_id(&event.id),
            status = %stripe_store::sanitize_for_display(stripe_status),
            "stripe: charge.dispute.closed/updated carries an UNKNOWN dispute status — mapped to open"
        );
    }
    let status = crate::disputes::DisputeStatus::from_stripe(stripe_status);
    if !status.is_terminal() {
        // A mid-lifecycle update (still open). Nothing to resolve; ack.
        return web::HttpResponse::Ok().json(&serde_json::json!({"status": "dispute_still_open"}));
    }

    let mut conn = match state.registry.conn().await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "stripe: dispute.closed conn failed — failing closed");
            return err_json(500, "internal error");
        }
    };
    // Resolve the anchor invoice from the dispute's pi_/ch_ so a CLOSE-BEFORE-CREATE
    // (MAJOR-4) can seed a terminal row. A close-before-create with no resolvable invoice
    // (an unrecorded charge) yields ctx=None → record_dispute_closed acks with Ok(None).
    let candidates: Vec<&str> = [obj.payment_intent.as_deref(), obj.charge.as_deref()]
        .into_iter()
        .flatten()
        .collect();
    let invoice_id = match crate::disputes::resolve_invoice_for_dispute(&conn, &candidates).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, "stripe: dispute.closed invoice resolution failed — failing closed");
            return err_json(500, "internal error");
        }
    };
    let currency = obj.currency.clone().unwrap_or_else(|| "usd".into());
    let amount = obj.amount.unwrap_or(0);
    let ctx = invoice_id.as_deref().filter(|_| amount > 0).map(|iid| {
        crate::disputes::DisputeCloseContext {
            invoice_id: iid,
            amount_cents: amount,
            currency: &currency,
            reason: obj.reason.as_deref(),
        }
    });
    let rec = match crate::disputes::record_dispute_closed(&mut conn, provider_dispute_id, status, ctx).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            // No dispute row for this du_… (close-before-create or an unrecorded charge).
            tracing::warn!(event_id = %sanitize_event_id(&event.id), "stripe: charge.dispute.closed with no recorded dispute — acked");
            return web::HttpResponse::Ok().json(&serde_json::json!({"status": "no_dispute_row"}));
        }
        Err(e) => {
            tracing::error!(error = %e, "stripe: dispute.closed record failed — failing closed for retry");
            return err_json(500, "internal error");
        }
    };

    let ip = source_ip(req, state);
    audit::log_with_detail(
        &state.registry,
        AuditEntry {
            app_id: None,
            creator_id: None,
            actor_user_id: None,
            actor_token_id: None,
            action: Action::RecordDispute,
            resource: Some(&event.id),
            source_ip: ip.as_deref(),
        },
        &serde_json::json!({
            "stripe_event_type": event.event_type,
            "dispute_id": rec.dispute_id,
            "invoice_id": rec.invoice_id,
            "provider_dispute_id": provider_dispute_id,
            "status": rec.status.as_str(),
        }),
    )
    .await;
    web::HttpResponse::Ok().json(&serde_json::json!({
        "status": "dispute_resolved",
        "dispute_status": rec.status.as_str(),
    }))
}

/// `charge.refund.updated` (webhook follow-up, MONEY-CRITICAL). A Stripe `Refund` (`re_…`)
/// we previously recorded `issued` later transitioned. We act ONLY on the terminal-failure
/// states (`failed`/`canceled`) — the cash did NOT return to the cardholder — and otherwise
/// ack (a `succeeded`/`pending`/`requires_action` update needs no reconciliation).
///
/// On failure we [`crate::refund::reconcile_failed_refund`] (idempotent, in one txn):
///   * flip our `refunds` row to the terminal status (so the over-refund cap STOPS counting
///     it → a failed cash refund no longer permanently reduces refundable cash; the creator
///     CAN re-refund),
///   * for a `destination='credit'` refund, claw back the minted `refund_to_credit` grant
///     via a compensating negative `refund_clawback` entry (the credit must not survive a
///     refund whose cash never moved). Balance is conserved.
///
/// Idempotent: a redelivered `charge.refund.updated` for an already-failed refund is a
/// no-op (no double-reversal / double-claw). FAIL-CLOSED: a transient DB error 5xx's so the
/// event is left UNCLAIMED for Stripe's retry, matching the M1 discipline.
async fn handle_refund_updated(
    req: &web::HttpRequest,
    state: &AppState,
    event: &StripeEvent,
    obj: &StripeObject,
) -> web::HttpResponse {
    let Some(re_id) = obj.id.as_deref().filter(|s| !s.is_empty()) else {
        tracing::warn!(event_id = %sanitize_event_id(&event.id), "stripe: charge.refund.updated missing refund id — ignored");
        return web::HttpResponse::Ok().json(&serde_json::json!({"status": "missing_refund_id"}));
    };
    let stripe_status = obj.status.as_deref().unwrap_or("");
    // Only the cash-did-not-return states reconcile; everything else is a benign ack.
    let Some(terminal) = crate::refund::stripe_refund_failure_status(stripe_status) else {
        return web::HttpResponse::Ok().json(&serde_json::json!({"status": "refund_update_noop"}));
    };

    let mut conn = match state.registry.conn().await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "stripe: refund.updated conn failed — failing closed");
            return err_json(500, "internal error");
        }
    };
    match crate::refund::reconcile_failed_refund(&mut conn, re_id, terminal).await {
        Ok(crate::refund::RefundFailureOutcome::Reversed { refund_id, clawed_back_credit }) => {
            let ip = source_ip(req, state);
            audit::log_with_detail(
                &state.registry,
                AuditEntry {
                    app_id: None,
                    creator_id: None,
                    actor_user_id: None,
                    actor_token_id: None,
                    action: Action::RefundFailed,
                    resource: Some(&event.id),
                    source_ip: ip.as_deref(),
                },
                &serde_json::json!({
                    "stripe_event_type": "charge.refund.updated",
                    "refund_id": refund_id,
                    "provider_refund_id": re_id,
                    "terminal_status": terminal,
                    "credit_clawed_back": clawed_back_credit,
                }),
            )
            .await;
            web::HttpResponse::Ok().json(&serde_json::json!({
                "status": "refund_reversed",
                "refund_id": refund_id,
                "credit_clawed_back": clawed_back_credit,
            }))
        }
        Ok(crate::refund::RefundFailureOutcome::AlreadyReversed(refund_id)) => web::HttpResponse::Ok()
            .json(&serde_json::json!({"status": "refund_already_reversed", "refund_id": refund_id})),
        Ok(crate::refund::RefundFailureOutcome::Unknown) => {
            tracing::warn!(event_id = %sanitize_event_id(&event.id), "stripe: charge.refund.updated for an unknown re_… (not a platform refund) — acked");
            web::HttpResponse::Ok().json(&serde_json::json!({"status": "refund_unknown"}))
        }
        Err(e) => {
            tracing::error!(error = %e, "stripe: refund.updated reconciliation failed — failing closed for retry");
            err_json(500, "internal error")
        }
    }
}

/// Resolve the connected account (`acct_…`) for a Connect event, preferring the event's
/// top-level `account` (the Connect-event signal) and falling back to the object's
/// `on_behalf_of` / `transfer_data.destination` (a destination charge on the platform).
fn connect_account_for<'a>(event: &'a StripeEvent, obj: &'a StripeObject) -> Option<&'a str> {
    event
        .account
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| obj.on_behalf_of.as_deref().map(str::trim).filter(|s| !s.is_empty()))
        .or_else(|| {
            obj.transfer_data
                .as_ref()
                .and_then(|t| t.destination.as_deref())
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
}

/// `payout.failed` (webhook follow-up). A payout to a creator's connected account bounced.
/// Resolve the creator from the connected account, record an idempotent `payout_failures`
/// row, and let the notify cron emit the `payout_failed` notification off it. FAIL-CLOSED on
/// a DB error.
async fn handle_payout_failed(
    req: &web::HttpRequest,
    state: &AppState,
    event: &StripeEvent,
    obj: &StripeObject,
) -> web::HttpResponse {
    let Some(po_id) = obj.id.as_deref().filter(|s| !s.is_empty()) else {
        tracing::warn!(event_id = %sanitize_event_id(&event.id), "stripe: payout.failed missing payout id — ignored");
        return web::HttpResponse::Ok().json(&serde_json::json!({"status": "missing_payout_id"}));
    };
    let Some(account_id) = connect_account_for(event, obj) else {
        tracing::warn!(event_id = %sanitize_event_id(&event.id), "stripe: payout.failed carries no connected account (top-level account / transfer_data) — acked, not attributable");
        return web::HttpResponse::Ok().json(&serde_json::json!({"status": "no_connected_account"}));
    };
    let creator_id = match state.stripe_store.get_creator_by_account(account_id).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            tracing::warn!(event_id = %sanitize_event_id(&event.id), account = %stripe_store::sanitize_for_display(account_id), "stripe: payout.failed for an account we don't have linked — acked");
            return web::HttpResponse::Ok().json(&serde_json::json!({"status": "account_not_linked"}));
        }
        Err(e) => {
            tracing::error!(error = %e, "stripe: payout.failed creator resolve failed — failing closed for retry");
            return err_json(500, "internal error");
        }
    };
    let amount = obj.amount.unwrap_or(0);
    let currency = obj.currency.clone().unwrap_or_else(|| "usd".into());
    match state
        .stripe_store
        .record_payout_failure(
            creator_id,
            po_id,
            account_id,
            amount,
            &currency,
            obj.failure_code.as_deref(),
            obj.failure_message.as_deref(),
            event.created,
        )
        .await
    {
        Ok(inserted) => {
            if inserted {
                let ip = source_ip(req, state);
                audit::log_with_detail(
                    &state.registry,
                    AuditEntry {
                        app_id: None,
                        creator_id: Some(creator_id),
                        actor_user_id: None,
                        actor_token_id: None,
                        action: Action::PayoutFailed,
                        resource: Some(&event.id),
                        source_ip: ip.as_deref(),
                    },
                    &serde_json::json!({
                        "stripe_event_type": "payout.failed",
                        "creator_id": creator_id.to_string(),
                        "stripe_payout_id": po_id,
                        "stripe_account_id": account_id,
                        "amount_cents": amount,
                        "failure_code": obj.failure_code.as_deref(),
                    }),
                )
                .await;
            }
            web::HttpResponse::Ok().json(&serde_json::json!({
                "status": if inserted { "payout_failure_recorded" } else { "duplicate" },
            }))
        }
        Err(e) => {
            tracing::error!(error = %e, "stripe: payout.failed record failed — failing closed for retry");
            stripe_err_response(e)
        }
    }
}

/// `payment_intent.payment_failed` (webhook follow-up). An end-user's Connect checkout
/// charge failed. No money moved — informational — but surfaced (recorded + a creator
/// notification) rather than silently dropped. Resolve the creator from the PI's
/// destination account (or the Connect event's top-level account), record an idempotent
/// `connect_checkout_failures` row. FAIL-CLOSED on a DB error.
async fn handle_payment_intent_failed(
    req: &web::HttpRequest,
    state: &AppState,
    event: &StripeEvent,
    obj: &StripeObject,
) -> web::HttpResponse {
    let Some(pi_id) = obj.id.as_deref().filter(|s| !s.is_empty()) else {
        tracing::warn!(event_id = %sanitize_event_id(&event.id), "stripe: payment_intent.payment_failed missing PI id — ignored");
        return web::HttpResponse::Ok().json(&serde_json::json!({"status": "missing_pi_id"}));
    };
    let Some(account_id) = connect_account_for(event, obj) else {
        // A PLATFORM (non-Connect) PI failure is not creator-attributable here — ack.
        tracing::info!(event_id = %sanitize_event_id(&event.id), "stripe: payment_intent.payment_failed carries no connected account — acked (not a Connect checkout)");
        return web::HttpResponse::Ok().json(&serde_json::json!({"status": "no_connected_account"}));
    };
    let creator_id = match state.stripe_store.get_creator_by_account(account_id).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            tracing::warn!(event_id = %sanitize_event_id(&event.id), account = %stripe_store::sanitize_for_display(account_id), "stripe: payment_intent.payment_failed for an unlinked account — acked");
            return web::HttpResponse::Ok().json(&serde_json::json!({"status": "account_not_linked"}));
        }
        Err(e) => {
            tracing::error!(error = %e, "stripe: payment_intent.payment_failed creator resolve failed — failing closed for retry");
            return err_json(500, "internal error");
        }
    };
    // PI amount: `amount` is the intended charge; default 0 if absent.
    let amount = obj.amount.unwrap_or(0);
    let currency = obj.currency.clone().unwrap_or_else(|| "usd".into());
    let (failure_code, failure_message) = obj
        .last_payment_error
        .as_ref()
        .map(|e| (e.code.as_deref(), e.message.as_deref()))
        .unwrap_or((None, None));
    match state
        .stripe_store
        .record_checkout_failure(
            creator_id,
            pi_id,
            account_id,
            amount,
            &currency,
            failure_code,
            failure_message,
            event.created,
        )
        .await
    {
        Ok(inserted) => {
            if inserted {
                let ip = source_ip(req, state);
                audit::log_with_detail(
                    &state.registry,
                    AuditEntry {
                        app_id: None,
                        creator_id: Some(creator_id),
                        actor_user_id: None,
                        actor_token_id: None,
                        action: Action::CheckoutFailed,
                        resource: Some(&event.id),
                        source_ip: ip.as_deref(),
                    },
                    &serde_json::json!({
                        "stripe_event_type": "payment_intent.payment_failed",
                        "creator_id": creator_id.to_string(),
                        "stripe_payment_intent_id": pi_id,
                        "stripe_account_id": account_id,
                        "amount_cents": amount,
                        "failure_code": failure_code,
                    }),
                )
                .await;
            }
            web::HttpResponse::Ok().json(&serde_json::json!({
                "status": if inserted { "checkout_failure_recorded" } else { "duplicate" },
            }))
        }
        Err(e) => {
            tracing::error!(error = %e, "stripe: payment_intent.payment_failed record failed — failing closed for retry");
            stripe_err_response(e)
        }
    }
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

    // m4: the connect-checkout description is capped at the Stripe max, on a char
    // boundary (never splitting a multi-byte char).
    #[test]
    fn description_capped_at_stripe_max_on_char_boundary() {
        // Short input passes through untouched.
        assert_eq!(cap_stripe_description("hello"), "hello");
        // Exactly at the cap is untouched.
        let exact: String = "a".repeat(MAX_STRIPE_DESCRIPTION);
        assert_eq!(cap_stripe_description(&exact), exact);
        // One past the cap is truncated to exactly MAX chars.
        let over: String = "a".repeat(MAX_STRIPE_DESCRIPTION + 50);
        let capped = cap_stripe_description(&over);
        assert_eq!(capped.chars().count(), MAX_STRIPE_DESCRIPTION);
        // Multi-byte chars are never split: a string of 4-byte emoji capped stays
        // valid UTF-8 and is exactly MAX chars.
        let emoji: String = "😀".repeat(MAX_STRIPE_DESCRIPTION + 10);
        let capped = cap_stripe_description(&emoji);
        assert_eq!(capped.chars().count(), MAX_STRIPE_DESCRIPTION);
        assert!(capped.chars().all(|c| c == '😀'), "no split multi-byte char");
    }

    // PR-8 CRITICAL-1: the pi_/ch_ capture must read BOTH Stripe Invoice wire shapes.
    #[test]
    fn invoice_payment_object_ids_legacy_top_level() {
        // Pre-Basil: top-level invoice.payment_intent / invoice.charge.
        let obj: StripeObject = serde_json::from_value(serde_json::json!({
            "id": "in_x",
            "amount_paid": 4500,
            "payment_intent": "pi_legacy",
            "charge": "ch_legacy"
        }))
        .unwrap();
        let (pi, ch) = invoice_payment_object_ids(&obj);
        assert_eq!(pi.as_deref(), Some("pi_legacy"));
        assert_eq!(ch.as_deref(), Some("ch_legacy"));
    }

    #[test]
    fn invoice_payment_object_ids_modern_nested() {
        // Basil 2025-03-31+: nested under payments.data[].payment.
        let obj: StripeObject = serde_json::from_value(serde_json::json!({
            "id": "in_x",
            "amount_paid": 4500,
            "payments": { "object": "list", "data": [
                { "payment": { "type": "payment_intent", "payment_intent": "pi_modern", "charge": "ch_modern" } }
            ]}
        }))
        .unwrap();
        let (pi, ch) = invoice_payment_object_ids(&obj);
        assert_eq!(pi.as_deref(), Some("pi_modern"), "reads nested payments.data[].payment.payment_intent");
        assert_eq!(ch.as_deref(), Some("ch_modern"), "reads nested payments.data[].payment.charge");
    }

    #[test]
    fn invoice_payment_object_ids_none_when_absent() {
        let obj: StripeObject =
            serde_json::from_value(serde_json::json!({ "id": "in_x", "amount_paid": 0 })).unwrap();
        let (pi, ch) = invoice_payment_object_ids(&obj);
        assert!(pi.is_none() && ch.is_none(), "a paid invoice with no settle ids yields none");
    }

    // MINOR-6: a documented Stripe status is known; junk is not.
    #[test]
    fn unknown_dispute_status_is_flagged() {
        use crate::disputes::DisputeStatus;
        assert!(DisputeStatus::is_known_stripe_status("needs_response"));
        assert!(DisputeStatus::is_known_stripe_status("won"));
        assert!(DisputeStatus::is_known_stripe_status("warning_closed"));
        assert!(!DisputeStatus::is_known_stripe_status("charge_refunded"));
        assert!(!DisputeStatus::is_known_stripe_status("brand_new_status"));
    }
}
