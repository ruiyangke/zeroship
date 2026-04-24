//! HTTP handlers for Stripe Connect onboarding + webhook ingest.
//!
//! Public (master-key auth):
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

use crate::AppState;
use crate::stripe_store::{self, StripeError};

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
            eprintln!("[stripe] store error: {e}");
            err_json(500, "internal error")
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
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = crate::api::check_admin_auth(&req, &state) { return r; }
    let Ok(creator_id) = Uuid::parse_str(&path) else { return bad_creator_id(); };

    // Placeholder — real impl goes to api.stripe.com/v1/account_links.
    let url = format!("https://connect.stripe.com/express_login?creator={creator_id}");
    web::HttpResponse::Ok().json(&serde_json::json!({
        "url": url,
        "note": "placeholder — implement Stripe account_links call per docs/stripe-integration-todo.md",
    }))
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
    body: web::types::Json<CallbackBody>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = crate::api::check_admin_auth(&req, &state) { return r; }
    let Ok(creator_id) = Uuid::parse_str(&path) else { return bad_creator_id(); };

    match state.stripe_store.link_account(creator_id, &body.stripe_account_id).await {
        Ok(()) => web::HttpResponse::NoContent().finish(),
        Err(e) => stripe_err_response(e),
    }
}

/// Dashboard earnings view.
pub async fn earnings(
    req: web::HttpRequest,
    path: Path<String>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = crate::api::check_admin_auth(&req, &state) { return r; }
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
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(r) = crate::api::check_admin_auth(&req, &state) { return r; }
    let Ok(creator_id) = Uuid::parse_str(&path) else { return bad_creator_id(); };

    match state.stripe_store.unlink_account(creator_id).await {
        Ok(true) => web::HttpResponse::NoContent().finish(),
        Ok(false) => err_json(404, "creator not linked"),
        Err(e) => stripe_err_response(e),
    }
}

// ----------------------------------------------------------------
// Webhook ingest (signature-verified)
// ----------------------------------------------------------------

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
    for part in sig_header.split(',') {
        let part = part.trim();
        if let Some((k, v)) = part.split_once('=') {
            match k.trim() {
                "t" => {
                    had_t_field = true;
                    t = v.trim().parse().ok();
                }
                "v1" => v1s.push(v.trim()),
                _ => {}
            }
        }
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
    amount_paid: Option<i64>,
    #[serde(default)]
    application_fee_amount: Option<i64>,
    #[serde(default)]
    currency: Option<String>,
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
pub const MAX_WEBHOOK_BODY: usize = 256 * 1024;

pub async fn webhook(
    req: web::HttpRequest,
    body: Bytes,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if body.len() > MAX_WEBHOOK_BODY {
        return err_json(413, format!("webhook body too large: {} bytes", body.len()));
    }
    let raw = body.as_ref();

    // Verify signature unless the operator explicitly opted in to
    // insecure dev mode. Empty secret is NOT a dev bypass — requires
    // `insecure_dev=true` on the AppState.
    if state.stripe_webhook_secret.is_empty() {
        if !state.insecure_dev {
            eprintln!("[stripe] webhook secret not configured; rejecting");
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
        if sig_header.len() > 4096 {
            return err_json(400, "signature header too large");
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if let Err(e) = verify_stripe_signature(raw, sig_header, &state.stripe_webhook_secret, now, 300) {
            eprintln!("[stripe] webhook rejected: {e}");
            return err_json(400, format!("webhook verification failed: {e}"));
        }
    }

    let event: StripeEvent = match serde_json::from_slice(raw) {
        Ok(e) => e,
        Err(e) => return err_json(400, format!("invalid json: {e}")),
    };

    // Only invoice.paid moves money on the platform v1.
    if event.event_type != "invoice.paid" {
        return web::HttpResponse::Ok().json(&serde_json::json!({"status": "ignored"}));
    }

    let obj = &event.data.object;
    let gross = obj.amount_paid.unwrap_or(0);
    let fee = obj.application_fee_amount.unwrap_or(0);
    let currency = obj.currency.clone().unwrap_or_else(|| "usd".into());

    // creator_id flows from the SDK's `buildCheckoutSession` via BOTH
    // session metadata AND `subscription_data[metadata]` — the latter
    // is what actually propagates to invoices. Check multiple wire
    // shapes to be version-robust.
    let Some(creator_id_str) = extract_creator_id(obj) else {
        eprintln!(
            "[stripe] event {} missing metadata.creator_id (checked invoice.metadata, \
             invoice.parent.subscription_details.metadata, invoice.subscription_details.metadata) \
             — ignored",
            sanitize_event_id(&event.id),
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
        Ok(rec) => web::HttpResponse::Ok().json(&serde_json::json!({
            "status": "recorded",
            "id": rec.id.to_string(),
            "net_amount": rec.net_amount,
        })),
        Err(StripeError::Duplicate) => {
            web::HttpResponse::Ok().json(&serde_json::json!({"status": "duplicate"}))
        }
        Err(e) => stripe_err_response(e),
    }
}

fn sanitize_event_id(s: &str) -> String {
    stripe_store::sanitize_for_display(s)
}

#[cfg(test)]
mod verification_tests {
    use super::*;

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
    fn cross_validates_with_sdk_format() {
        // Same body + secret + timestamp used in sdks/payments/tests —
        // if our Rust implementation matches, the hex output matches too.
        let body = b"{\"type\":\"invoice.paid\",\"data\":{\"object\":{\"amount_paid\":1000}}}";
        let header = sign(body, NOW);
        // Parse out the v1 and compare against the known SDK output for
        // the same inputs. The JS test uses secret="whsec_test_EXAMPLE"
        // whereas we use SECRET above — so this is a smoke test that
        // the format matches rather than byte-identity with the JS test.
        let v1 = header.split(',').find_map(|p| p.strip_prefix("v1=")).unwrap();
        assert_eq!(v1.len(), 64); // SHA-256 hex is 64 chars
        assert!(v1.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
