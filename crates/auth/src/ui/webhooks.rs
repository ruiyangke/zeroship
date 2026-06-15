//! Webhook handlers — Postmark + SES-SNS bounce/complaint.
//!
//! ## Postmark (`POST /webhooks/postmark`)
//!
//! Postmark posts JSON for delivery events. Authentication is HTTP Basic
//! (configured per-server in the Postmark dashboard; the user/password
//! pair is matched against `AUTH_POSTMARK_WEBHOOK_USER` /
//! `AUTH_POSTMARK_WEBHOOK_PASSWORD`). On every request we:
//!
//! 1. Reject 401 if credentials aren't configured or the supplied
//!    `Authorization: Basic …` doesn't match.
//! 2. Deserialize the body into [`PostmarkEvent`] — anything we don't
//!    recognise (Delivery, Open, Click, …) gets a 200 and is dropped.
//! 3. Hard bounces and spam complaints add the recipient to
//!    `zeroship.email_suppressions` (the same table the mailer's
//!    pre-send check consults) and emit a structured audit event.
//! 4. Soft bounces are logged at INFO level but NOT suppressed —
//!    they're transient (mailbox full, server down).
//!
//! Postmark retries on non-2xx — so once we've validated and started
//! processing we always return 200 (genuine 500s on DB failure are
//! still surfaced; Postmark's retry then converges).
//!
//! ## SES-SNS (`POST /webhooks/ses-sns`)
//!
//! AWS SES bounce/complaint events arrive via an SNS topic. The handler:
//!
//! 1. Parses the SNS envelope.
//! 2. Validates `SignatureVersion == "1"` and the `SigningCertURL` host
//!    (`sns.<region>.amazonaws.com`, anti-SSRF).
//! 3. Fetches the cert + RSA-SHA1 verifies the canonical string-to-sign.
//! 4. On `SubscriptionConfirmation` — auto-confirms by GET-ing
//!    `SubscribeURL` (ONLY after the signature verifies).
//! 5. On `Notification` — parses the inner SES event (a JSON-encoded
//!    STRING in `Message`) and adds Permanent bounces + Complaints to
//!    `zeroship.email_suppressions`. Transient bounces log only.
//!
//! Reference: <https://docs.aws.amazon.com/sns/latest/dg/sns-verify-signature-of-message.html>

use std::sync::Arc;

use ntex::http::header::AUTHORIZATION;
use ntex::util::Bytes;
use ntex::web::{
    types::{Json, State},
    HttpRequest, HttpResponse,
};

use crate::audit::{self, AuditEvent};
use crate::config::AuthConfig;
use crate::store::{ratelimit, relay};
use zeroship_mailer::bounce::{bounce_type_is_permanent, verify_basic_auth, PostmarkEvent};
use zeroship_mailer::forward::{build_bounce, build_forward, BounceReason};
use zeroship_mailer::inbound::{normalize_alias, InboundMessage, RELAY_MAX_HOPS, SPAM_SCORE_THRESHOLD};
use zeroship_mailer::sns::{self, is_valid_sns_cert_url, SesEvent, SnsEnvelope};
use zeroship_mailer::{suppressions, Email, MailerError, RelayForwardMailer};

/// Per-alias leaky-bucket capacity / refill (sub-spec §7): burst 20,
/// ≈60/hour steady. Sized so a busy receipt/newsletter alias is never
/// throttled but a flood is.
const RELAY_ALIAS_CAPACITY: f64 = 20.0;
const RELAY_ALIAS_REFILL_PER_SEC: f64 = 0.0167;
/// Per-app leaky-bucket capacity / refill (sub-spec §7): burst 200,
/// ≈20/min steady. Caps an app's total inbound forward volume.
const RELAY_APP_CAPACITY: f64 = 200.0;
const RELAY_APP_REFILL_PER_SEC: f64 = 0.333;
/// Consecutive over-limit windows on one alias before auto-revoke is requested
/// (sub-spec §7 — the second-tier signal that distinguishes a transient spike
/// from sustained abuse). The streak counter rides its own `zeroship.rate_limits`
/// bucket and is reset on any successful forward.
const RELAY_ABUSE_STREAK: f64 = 5.0;
/// Retry-After (seconds) returned on a transient over-limit 503 so Postmark
/// re-delivers the spike later (smoothing the burst).
const RELAY_RETRY_AFTER_SECS: u32 = 60;

/// `POST /webhooks/postmark`. Always registered; rejects 401 when
/// credentials aren't configured at runtime so misrouted webhook traffic
/// doesn't silently succeed in dev.
//
// ntex's per-thread service futures are intentionally `!Send`.
#[allow(clippy::future_not_send)]
pub async fn postmark(
    req: HttpRequest,
    body: Json<serde_json::Value>,
    cfg: State<Arc<AuthConfig>>,
    db: State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    // 1. Auth check — runs before payload parse so we don't burn CPU
    //    deserializing attacker-supplied JSON on unauthorised requests.
    let auth_h = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let (Some(expected_user), Some(expected_pass)) = (
        cfg.postmark_webhook_user.as_deref(),
        cfg.postmark_webhook_password.as_deref(),
    ) else {
        tracing::warn!("postmark webhook hit but credentials not configured — rejecting");
        return HttpResponse::Unauthorized().finish();
    };
    if !verify_basic_auth(auth_h, expected_user, expected_pass) {
        return HttpResponse::Unauthorized().finish();
    }

    // 2. Parse event. A malformed body is a 400 — Postmark won't retry
    //    payload-parse failures (they're permanent for a given payload).
    let event: PostmarkEvent = match serde_json::from_value(body.into_inner()) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "postmark webhook: payload parse failed");
            return HttpResponse::BadRequest().finish();
        }
    };

    // 3. Branch on event type.
    match event {
        PostmarkEvent::Bounce(b) if bounce_type_is_permanent(&b.r#type) => {
            if let Err(e) = suppressions::add(
                db.as_ref(),
                &b.email,
                &format!("postmark_{}", b.r#type),
                b.description.as_deref(),
            )
            .await
            {
                tracing::error!(
                    error = %e,
                    email_domain = %email_domain(&b.email),
                    "suppression add failed"
                );
                return HttpResponse::InternalServerError().finish();
            }
            // Audit detail uses email_domain only (not the full address)
            // to keep PII out of the audit stream — matches the
            // forgot.rs convention. `unwrap_or("")` covers the
            // pathological no-@ case so we never panic on a malformed
            // address landing here.
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "mailer_bounce",
                    outcome: "success",
                    auth_method: Some("postmark"),
                    detail: serde_json::json!({
                        "email_domain": b.email.split('@').nth(1).unwrap_or(""),
                        "bounce_type": b.r#type,
                    }),
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
        }
        PostmarkEvent::Bounce(b) => {
            // Soft bounce — log only. Transient failures (mailbox full,
            // server down) recover; suppressing on them would
            // permanently block legitimate users.
            tracing::info!(
                email_domain = %email_domain(&b.email),
                kind = %b.r#type,
                "postmark soft bounce"
            );
        }
        PostmarkEvent::SpamComplaint(c) => {
            if let Err(e) = suppressions::add(
                db.as_ref(),
                &c.email,
                "postmark_complaint",
                c.description.as_deref(),
            )
            .await
            {
                tracing::error!(
                    error = %e,
                    email_domain = %email_domain(&c.email),
                    "suppression add failed"
                );
                return HttpResponse::InternalServerError().finish();
            }
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "mailer_complaint",
                    outcome: "success",
                    auth_method: Some("postmark"),
                    detail: serde_json::json!({
                        "email_domain": c.email.split('@').nth(1).unwrap_or(""),
                    }),
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
        }
        PostmarkEvent::Other => {
            // Delivery / Open / Click / SubscriptionChange — accepted
            // silently. We never asked Postmark to send these; if
            // they arrive the dashboard config drifted, not a bug.
        }
    }

    HttpResponse::Ok().finish()
}

/// `POST /webhooks/ses-sns`. Verifies the SNS signature, auto-confirms
/// subscription requests, and adds permanent bounces + complaints to the
/// suppression list.
///
/// Status codes:
/// - `200` — accepted (notification processed, or unrecognised inner
///   event type accepted-and-ignored, or `UnsubscribeConfirmation`)
/// - `400` — malformed envelope, unsupported `SignatureVersion`, or
///   `SigningCertURL` host not on the allowlist
/// - `401` — RSA verify rejected the signature
/// - `500` — auto-confirm GET to `SubscribeURL` failed (so the operator
///   retries; SNS itself doesn't re-deliver the `SubscriptionConfirmation`,
///   but a 500 surfaces in the dashboard)
//
// ntex's per-thread service futures are intentionally `!Send`.
#[allow(clippy::future_not_send)]
#[allow(clippy::too_many_lines)]
pub async fn ses_sns(
    req: HttpRequest,
    body: Json<serde_json::Value>,
    db: State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    // 1. Parse outer SNS envelope.
    let envelope: SnsEnvelope = match serde_json::from_value(body.into_inner()) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "sns webhook: bad envelope");
            return HttpResponse::BadRequest().finish();
        }
    };

    // 2. Validate SignatureVersion — support SNS v1 (RSA-SHA1 legacy)
    //    and v2 (RSA-SHA256).
    if !matches!(envelope.signature_version.as_str(), "1" | "2") {
        tracing::warn!(
            version = %envelope.signature_version,
            "sns webhook: unsupported SignatureVersion"
        );
        return HttpResponse::BadRequest()
            .body("only SignatureVersion 1 or 2 is supported");
    }

    // 3. Validate SigningCertURL host (anti-SSRF). Done BEFORE the
    //    network fetch so a malicious URL can't be coerced into
    //    triggering an outbound request.
    if !is_valid_sns_cert_url(&envelope.signing_cert_url) {
        tracing::warn!(
            url = %envelope.signing_cert_url,
            "sns webhook: rejecting bogus SigningCertURL"
        );
        return HttpResponse::BadRequest().finish();
    }

    // 4. Fetch cert + RSA verify the canonical string.
    if let Err(e) = sns::verify(&envelope).await {
        tracing::warn!(error = %e, "sns webhook: signature verification failed");
        return HttpResponse::Unauthorized().finish();
    }

    // 5. Branch on Type.
    match envelope.r#type.as_str() {
        "SubscriptionConfirmation" => {
            // Auto-confirm by GET-ing SubscribeURL — but only AFTER
            // the signature is verified (otherwise we'd let attackers
            // weaponize us into a GET reflector).
            let Some(url) = envelope.subscribe_url.as_deref() else {
                tracing::warn!("sns webhook: SubscriptionConfirmation without SubscribeURL");
                return HttpResponse::BadRequest().finish();
            };
            if let Err(e) = sns::confirm_subscription(url).await {
                tracing::warn!(error = %e, "sns webhook: subscribe confirm failed");
                return HttpResponse::InternalServerError().finish();
            }
            tracing::info!(topic = %envelope.topic_arn, "sns: subscription confirmed");
            HttpResponse::Ok().finish()
        }
        "Notification" => {
            // The `Message` field of an SNS Notification is a JSON
            // **string**, not an embedded object. Parse it as a fresh
            // JSON document to recover the SES event.
            let inner: SesEvent = match serde_json::from_str(&envelope.message) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "sns webhook: SES inner payload parse");
                    // The SNS envelope itself is valid; we just don't
                    // understand the inner. Accept so SNS doesn't retry.
                    return HttpResponse::Ok().finish();
                }
            };
            handle_ses_event(db.as_ref(), inner, &req).await;
            HttpResponse::Ok().finish()
        }
        _ => {
            // `UnsubscribeConfirmation` — accept silently; an operator
            // unsubscribed the topic in the AWS console, no platform
            // action needed.
            HttpResponse::Ok().finish()
        }
    }
}

/// Dispatch a parsed SES inner event. Suppression-list writes +
/// audit emission only; never returns an error to the caller.
async fn handle_ses_event(db: &compio_postgres::Client, ev: SesEvent, req: &HttpRequest) {
    match ev {
        SesEvent::Bounce { bounce } if bounce.bounce_type == "Permanent" => {
            for rec in &bounce.bounced_recipients {
                if let Err(e) = suppressions::add(
                    db,
                    &rec.email_address,
                    "ses_permanent_bounce",
                    None,
                )
                .await
                {
                    tracing::error!(error = %e, email_domain = %email_domain(&rec.email_address),
                                    "ses-sns suppression add failed");
                }
            }
            audit::emit(
                db,
                &AuditEvent {
                    event_type: "mailer_bounce",
                    outcome: "success",
                    auth_method: Some("ses_sns"),
                    detail: serde_json::json!({
                        "count": bounce.bounced_recipients.len(),
                        "kind": "permanent",
                    }),
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
        }
        SesEvent::Bounce { bounce } => {
            // Transient bounces (Transient, Undetermined, …) — log only.
            tracing::info!(
                kind = %bounce.bounce_type,
                count = bounce.bounced_recipients.len(),
                "ses transient bounce — not suppressed"
            );
        }
        SesEvent::Complaint { complaint } => {
            for rec in &complaint.complained_recipients {
                if let Err(e) =
                    suppressions::add(db, &rec.email_address, "ses_complaint", None).await
                {
                    tracing::error!(error = %e, email_domain = %email_domain(&rec.email_address),
                                    "ses-sns complaint suppression add failed");
                }
            }
            audit::emit(
                db,
                &AuditEvent {
                    event_type: "mailer_complaint",
                    outcome: "success",
                    auth_method: Some("ses_sns"),
                    detail: serde_json::json!({
                        "count": complaint.complained_recipients.len(),
                    }),
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
        }
        SesEvent::Other => {
            // Delivery / DeliveryDelay / Send / Open / Click — we never
            // asked SES to post these but if they arrive, drop them.
        }
    }
}

fn email_domain(email: &str) -> &str {
    email.split_once('@').map(|(_, domain)| domain).unwrap_or("")
}

// ─── Relay inbound (`POST /webhooks/relay-inbound`) — Slice 5b ──────────────
//
// The Postmark Inbound webhook: a third party emailed `{alias}@{relay_domain}`,
// Postmark parsed the MIME to JSON and POSTs it here. We resolve the alias to
// the user's real inbox and re-originate the message from the relay identity
// with the §5.3 privacy surgery. The gates run in the sub-spec §4.3 order;
// every non-transient outcome returns 200 so Postmark's retry queue stays empty
// (a non-2xx makes Postmark RETRY then black-hole — §4.3/§8), and the handler
// itself EMITS a bounce (never silent-drops a known-but-unforwardable message).

/// The relay-forward app display name. v1 has no per-app name lookup wired into
/// this path, so forwards are branded with a neutral platform label; the
/// per-app name is a v2 enhancement (it would JOIN zeroship.oauth_clients).
const RELAY_APP_DISPLAY: &str = "App";

/// `POST /webhooks/relay-inbound`. Extracts the shared `Arc<Client>` (like
/// `ses_sns`) plus the dedicated `RelayForwardMailer` (§5.2a). Verifies Basic
/// auth, dedups, runs the loop/lookup/suppression/spam/rate gates, then builds
/// + sends the forward — or emits an explicit bounce + 200.
//
// ntex's per-thread service futures are intentionally `!Send`.
#[allow(clippy::future_not_send)]
#[allow(clippy::too_many_lines)]
pub async fn relay_inbound(
    req: HttpRequest,
    // Raw bytes, NOT `Json<…>`: a `Json` extractor would JSON-deserialize the
    // whole body BEFORE the handler runs (so the auth gate could not precede the
    // parse). `Bytes` only buffers the raw payload (bounded by ntex's
    // `PayloadConfig` size limit), and we deserialize it AFTER the auth gate —
    // so an unauthenticated attacker never forces a JSON parse of arbitrary
    // attacker-supplied content. (Closes the false "auth before parse" comment.)
    body: Bytes,
    cfg: State<Arc<AuthConfig>>,
    db: State<Arc<compio_postgres::Client>>,
    relay_mailer: State<RelayForwardMailer>,
) -> HttpResponse {
    // 1. Basic-auth gate — runs BEFORE the JSON parse below. The raw `body`
    //    bytes are buffered by ntex (size-bounded by PayloadConfig), but they
    //    are NOT deserialized until after this gate, so an unauthenticated POST
    //    never burns CPU on a JSON parse of attacker-supplied content. An
    //    unverified POST is a forwarding/suppression spoof oracle, so the
    //    handler 401s when creds aren't configured (mirrors the postmark hook).
    let auth_h = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let (Some(expected_user), Some(expected_pass)) = (
        cfg.relay_inbound_user.as_deref(),
        cfg.relay_inbound_password.as_deref(),
    ) else {
        tracing::warn!("relay-inbound hit but credentials not configured — rejecting");
        return HttpResponse::Unauthorized().finish();
    };
    if !verify_basic_auth(auth_h, expected_user, expected_pass) {
        return HttpResponse::Unauthorized().finish();
    }

    // 0. Parse the Postmark Inbound payload (AFTER the auth gate, from the raw
    //    bytes). A malformed body is NOT retryable (Postmark would re-send it
    //    identically) — 200 + log, do not bounce.
    let msg: InboundMessage = match serde_json::from_slice(&body) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "relay-inbound: payload parse failed — dropping");
            return HttpResponse::Ok().finish();
        }
    };

    // 2. Idempotency / replay guard on MessageID (§7.1). Basic auth proves the
    //    secret, NOT per-message freshness — a captured POST replays N forwards
    //    without this. This is a NON-committing read-only probe: the sentinel is
    //    committed only at a TERMINAL outcome (forward / bounce / deliberate
    //    drop), AFTER every retryable (503) gate. Committing here would dedupe a
    //    legitimate Postmark retry of a transient-fault (503) message into a
    //    silent drop — the never-silent-drop bug (§8). Already seen ⇒ 200 +
    //    drop (the prior sighting already terminally handled it).
    match relay::already_seen(db.as_ref(), &msg.message_id).await {
        Ok(false) => {} // fresh — proceed through the gates
        Ok(true) => {
            tracing::info!(message_id = %msg.message_id, "relay-inbound: replay — dropping");
            return HttpResponse::Ok().finish();
        }
        Err(e) => {
            // DB momentarily down — transient; let Postmark retry. No sentinel
            // was written, so the retry is processed (not silent-dropped).
            tracing::error!(error = %e, "relay-inbound: dedup probe error");
            return HttpResponse::ServiceUnavailable().finish();
        }
    }

    // 3. Loop guard (§7 hop cap) — a forward of our own forward carries
    //    X-ZS-Relay: <hop>. Drop when the hop count is at/above RELAY_MAX_HOPS
    //    (bounds a relay↔relay loop even across two aliases). Below-cap inbounds
    //    are forwarded with hop+1 (§9). Terminal: commit the dedup sentinel so a
    //    replay is also dropped.
    if msg.is_relay_loop(RELAY_MAX_HOPS) {
        tracing::info!(
            message_id = %msg.message_id,
            hop = msg.loop_hop().unwrap_or(0),
            "relay-inbound: loop hop cap reached — dropping"
        );
        commit_seen_logged(db.as_ref(), &msg.message_id).await;
        return HttpResponse::Ok().finish();
    }

    // 4. Normalize OriginalRecipient → the alias key (§4.4a: lowercase, strip
    //    +tag/MailboxHash) for the exact-match lookup.
    let Some(alias) = normalize_alias(&msg.original_recipient) else {
        tracing::warn!(rcpt = %msg.original_recipient, "relay-inbound: unparseable recipient — dropping");
        commit_seen_logged(db.as_ref(), &msg.message_id).await;
        return HttpResponse::Ok().finish();
    };

    // 5. Resolve alias → real inbox (active map only, §4.5). Unknown/revoked ⇒
    //    explicit bounce ("address no longer active") + 200 (§8).
    let target = match relay::resolve_active_alias(db.as_ref(), &alias).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return emit_bounce_then_ok(
                db.as_ref(),
                &relay_mailer,
                &cfg,
                &msg,
                BounceReason::AddressInactive,
                "alias_inactive",
                &req,
            )
            .await;
        }
        Err(e) => {
            tracing::error!(error = %e, "relay-inbound: alias resolve error");
            return HttpResponse::ServiceUnavailable().finish();
        }
    };

    // O7 — a user REPLY to a relayed message bounces (v1 is app→user one-way).
    //      Checked after lookup so we only ever bounce to a real third party /
    //      user, not to a spoofed sender on an unknown alias.
    if msg.is_reply() {
        return emit_bounce_then_ok(
            db.as_ref(),
            &relay_mailer,
            &cfg,
            &msg,
            BounceReason::RepliesUnsupported,
            "reply_unsupported",
            &req,
        )
        .await;
    }

    // 6. Suppression gate on the REAL inbox (§4.3 step 6). Suppressed ⇒ 200 +
    //    drop, NO bounce (the inbox already bounced/complained; bouncing to the
    //    original sender leaks nothing useful and risks a loop).
    match suppressions::is_suppressed(db.as_ref(), &target.real_inbox).await {
        Ok(true) => {
            // Terminal drop — commit the dedup sentinel so a replay is dropped.
            tracing::info!("relay-inbound: real inbox suppressed — dropping (no bounce)");
            commit_seen_logged(db.as_ref(), &msg.message_id).await;
            return HttpResponse::Ok().finish();
        }
        Ok(false) => {}
        Err(e) => {
            tracing::error!(error = %e, "relay-inbound: suppression check error");
            return HttpResponse::ServiceUnavailable().finish();
        }
    }

    // 7. Spam / sender-auth gate (§5.4). Forwarding spam under the relay's own
    //    DKIM torches its reputation; bouncing spam to a forged sender is
    //    backscatter — so drop + 200, NO forward, NO bounce.
    if msg.is_spam_or_unauthenticated(SPAM_SCORE_THRESHOLD) {
        // Terminal drop — commit the dedup sentinel so a replay is dropped.
        tracing::info!("relay-inbound: spam/dmarc-fail — dropping (no forward, no bounce)");
        commit_seen_logged(db.as_ref(), &msg.message_id).await;
        return HttpResponse::Ok().finish();
    }

    // 8. Per-alias + per-app leaky-bucket rate limit (§7). consume() returns a
    //    BOOL (it does not itself produce a 429); we branch on `consumed`.
    let alias_key = format!("relay:alias:{alias}");
    let app_key = format!("relay:app:{}", target.app_client_id);
    let alias_ok = match ratelimit::consume(
        db.as_ref(),
        &alias_key,
        RELAY_ALIAS_CAPACITY,
        RELAY_ALIAS_REFILL_PER_SEC,
    )
    .await
    {
        Ok(r) => r.consumed,
        Err(e) => {
            tracing::error!(error = %e, "relay-inbound: alias rate-limit error");
            return HttpResponse::ServiceUnavailable().finish();
        }
    };
    let app_ok = match ratelimit::consume(
        db.as_ref(),
        &app_key,
        RELAY_APP_CAPACITY,
        RELAY_APP_REFILL_PER_SEC,
    )
    .await
    {
        Ok(r) => r.consumed,
        Err(e) => {
            tracing::error!(error = %e, "relay-inbound: app rate-limit error");
            return HttpResponse::ServiceUnavailable().finish();
        }
    };

    if !(alias_ok && app_ok) {
        // Second-tier signal: count consecutive over-limit windows. Sustained
        // abuse (streak ≥ ABUSE_STREAK) ⇒ auto-revoke the alias via control's
        // revoke path + 200 drop. A transient spike ⇒ 503 (Postmark retries).
        let streak = bump_abuse_streak(db.as_ref(), &alias).await;
        if streak >= RELAY_ABUSE_STREAK {
            // HONEST auto-revoke (Fix 6): disable our OWN forwarding now, and
            // audit EXACTLY what happened — the cross-service grant revoke is
            // PENDING (no admin control endpoint yet), NOT a completed success.
            let outcome = request_alias_auto_revoke(db.as_ref(), &target).await;
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "relay_auto_revoke",
                    outcome: outcome.audit_outcome(),
                    client_id: Some(&target.app_client_id),
                    auth_method: Some("relay"),
                    detail: auto_revoke_audit_detail(outcome),
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
            // Terminal drop — commit the dedup sentinel.
            commit_seen_logged(db.as_ref(), &msg.message_id).await;
            tracing::warn!(alias_streak = streak, auto_revoke = %outcome.audit_outcome(), "relay-inbound: sustained abuse — local forwarding disabled, dropping");
            return HttpResponse::Ok().finish();
        }
        // Transient spike ⇒ 503 so Postmark RETRIES the message later (smoothing
        // the burst). We deliberately do NOT commit the dedup sentinel here — a
        // committed sentinel would dedupe the retry into a silent drop (§7/§8).
        tracing::info!("relay-inbound: over rate limit — 503 (Postmark retries)");
        return HttpResponse::ServiceUnavailable()
            .header("Retry-After", RELAY_RETRY_AFTER_SECS.to_string())
            .finish();
    }
    // 9. Build + send the forward via the dedicated relay-forward mailer. (The
    //    abuse streak is reset on a SUCCESSFUL forward — see the Ok arm below —
    //    matching §7's "reset on any successful forward".)
    let forward: Email = build_forward(
        &msg,
        &alias,
        &target.real_inbox,
        RELAY_APP_DISPLAY,
        &cfg.relay_domain,
        msg.next_hop(),
    );
    match relay_mailer.0.send(db.as_ref(), forward).await {
        Ok(_) => {
            // Terminal success — commit the dedup sentinel so a replay (or a
            // lost-200 Postmark retry of THIS message) does not forward twice.
            audit_relay(db.as_ref(), "relay_forward", "success", &target.app_client_id, &req).await;
            commit_seen_logged(db.as_ref(), &msg.message_id).await;
            // Successful pass through the limiter resets the abuse streak.
            reset_abuse_streak(db.as_ref(), &alias).await;
            HttpResponse::Ok().finish()
        }
        Err(MailerError::Suppressed(_)) => {
            // The real inbox got suppressed between the gate and the send — drop
            // with 200, no bounce (same rationale as the suppression gate).
            // Terminal — commit the dedup sentinel.
            tracing::info!("relay-inbound: forward suppressed at send — dropping");
            commit_seen_logged(db.as_ref(), &msg.message_id).await;
            HttpResponse::Ok().finish()
        }
        Err(MailerError::Transport(e)) => {
            // Transient transport fault — 503 so Postmark retries. Do NOT commit
            // the dedup sentinel: the retry must be processed, not deduped away
            // into a silent drop (§7/§8).
            tracing::error!(error = %e, "relay-inbound: forward transport error — 503");
            HttpResponse::ServiceUnavailable().finish()
        }
        Err(MailerError::Config(e)) => {
            // Permanent build/config error — emit a bounce + 200 (no retry).
            // emit_bounce_then_ok commits the dedup sentinel.
            tracing::error!(error = %e, "relay-inbound: forward config error — bouncing");
            emit_bounce_then_ok(
                db.as_ref(),
                &relay_mailer,
                &cfg,
                &msg,
                BounceReason::AddressInactive,
                "forward_config_error",
                &req,
            )
            .await
        }
    }
}

/// Emit an explicit bounce to the original sender via the relay-forward mailer,
/// then return 200 to Postmark (sub-spec §8). The bounce is itself
/// suppression-gated by the mailer contract, so we never bounce-loop into a
/// suppressed sender — a `Suppressed`/transport error on the bounce is logged
/// and swallowed (we still 200 Postmark; the message is terminally handled).
///
/// This is a TERMINAL outcome, so it commits the `MessageID` dedup sentinel
/// (§7.1) — a replay of the same bounced message is then dropped, while a
/// genuine transient-fault 503 (which never reaches here) is left un-deduped so
/// Postmark's retry is honoured (§8 never-silent-drop).
#[allow(clippy::future_not_send)]
async fn emit_bounce_then_ok(
    db: &compio_postgres::Client,
    relay_mailer: &RelayForwardMailer,
    cfg: &AuthConfig,
    // The inbound carries both the bounce recipient (`from_full.email`) and the
    // dedup key (`message_id`) — passing it whole keeps the helper at ≤7 args.
    inbound: &InboundMessage,
    reason: BounceReason,
    audit_kind: &str,
    req: &HttpRequest,
) -> HttpResponse {
    let original_sender = &inbound.from_full.email;
    let bounce = build_bounce(original_sender, reason, &cfg.relay_domain);
    match relay_mailer.0.send(db, bounce).await {
        Ok(_) => {}
        Err(MailerError::Suppressed(_)) => {
            tracing::info!("relay-inbound: bounce target suppressed — not re-bouncing");
        }
        Err(e) => {
            tracing::warn!(error = %e, "relay-inbound: bounce emit failed (still 200 to Postmark)");
        }
    }
    audit::emit(
        db,
        &AuditEvent {
            event_type: "relay_bounce",
            outcome: audit_kind,
            auth_method: Some("relay"),
            detail: serde_json::json!({ "sender_domain": email_domain(original_sender) }),
            ..AuditEvent::from_request(req)
        },
    )
    .await;
    commit_seen_logged(db, &inbound.message_id).await;
    HttpResponse::Ok().finish()
}

/// Commit the `MessageID` dedup sentinel at a terminal outcome (sub-spec §7.1),
/// logging (but swallowing) a store error: a commit failure on an
/// already-handled message is at-least-once (a future replay may re-process),
/// never the silent-drop direction, so it must not turn a terminal 200 into a
/// 503. MUST be called ONLY on 200 (terminal) paths — never before a 503 gate.
#[allow(clippy::future_not_send)]
async fn commit_seen_logged(db: &compio_postgres::Client, message_id: &str) {
    if let Err(e) = relay::commit_seen(db, message_id).await {
        tracing::warn!(error = %e, message_id = %message_id, "relay-inbound: dedup commit failed");
    }
}

/// Emit a relay audit event (forward success / auto-revoke). Keyed on the app
/// client_id only; no PII (matches the bounce/complaint audit convention).
#[allow(clippy::future_not_send)]
async fn audit_relay(
    db: &compio_postgres::Client,
    event_type: &'static str,
    outcome: &'static str,
    app_client_id: &str,
    req: &HttpRequest,
) {
    audit::emit(
        db,
        &AuditEvent {
            event_type,
            outcome,
            client_id: Some(app_client_id),
            auth_method: Some("relay"),
            ..AuditEvent::from_request(req)
        },
    )
    .await;
}

/// Increment the per-alias consecutive-over-limit counter (§7) and return the
/// new streak. Stored as a counter in `zeroship.rate_limits` with a huge capacity
/// (so it never blocks) and zero refill (so it only goes up until reset). The
/// returned `tokens` is the consumed count ⇒ streak = capacity - tokens.
#[allow(clippy::future_not_send)]
async fn bump_abuse_streak(db: &compio_postgres::Client, alias: &str) -> f64 {
    // capacity huge, refill 0: each consume permanently lowers `tokens` by 1,
    // so (capacity - tokens) is the count of over-limit windows since reset.
    const ABUSE_CAP: f64 = 1_000_000.0;
    let key = format!("relay:abuse:{alias}");
    match ratelimit::consume(db, &key, ABUSE_CAP, 0.0).await {
        Ok(r) => ABUSE_CAP - r.state.tokens,
        Err(e) => {
            tracing::error!(error = %e, "relay-inbound: abuse-streak bump error");
            0.0
        }
    }
}

/// Reset the per-alias abuse streak on a successful forward (§7) by deleting its
/// counter row so the next over-limit window starts from zero.
#[allow(clippy::future_not_send)]
async fn reset_abuse_streak(db: &compio_postgres::Client, alias: &str) {
    let key = format!("relay:abuse:{alias}");
    if let Err(e) = db
        .execute(
            "DELETE FROM zeroship.rate_limits WHERE bucket_key = $1",
            &[&key],
        )
        .await
    {
        tracing::warn!(error = %e, "relay-inbound: abuse-streak reset failed");
    }
}

/// Outcome of an abuse auto-revoke attempt — what ACTUALLY happened, so the
/// audit trail reflects reality (Fix 6, honest auto-revoke).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoRevokeOutcome {
    /// Auth disabled its OWN forwarding (`app_user_identities.revoked_at`); the
    /// cross-service grant revoke is still PENDING (no admin endpoint yet).
    LocalRevokedCrossServicePending,
    /// The alias was already locally revoked (idempotent re-trigger); cross-
    /// service grant revoke still pending.
    AlreadyLocalRevokedCrossServicePending,
    /// The local disable itself FAILED (DB error) — nothing was revoked.
    Failed,
}

impl AutoRevokeOutcome {
    /// The audit `outcome` string — TRUTHFUL about the cross-service step never
    /// having run. Never a bare `"success"` (which would imply a completed
    /// cross-service revoke that did not happen).
    fn audit_outcome(self) -> &'static str {
        match self {
            Self::LocalRevokedCrossServicePending => "local_revoked_cross_service_pending",
            Self::AlreadyLocalRevokedCrossServicePending => {
                "already_local_revoked_cross_service_pending"
            }
            Self::Failed => "failed",
        }
    }
}

/// Build the `relay_auto_revoke` audit `detail` object for an outcome.
///
/// The two booleans are DISJOINT and STATE-PRECISE so a downstream consumer that
/// sums `local_alias_disabled` counts only genuine new revokes:
/// - `local_alias_disabled` — true ONLY when THIS call newly stamped
///   `revoked_at` (1 row). It is NOT set for the idempotent re-trigger (which
///   revoked 0 rows), so it never conflates "this call disabled it" with "the
///   alias IS disabled".
/// - `already_disabled` — the separate flag for the idempotent re-trigger (a
///   prior trigger already disabled the alias; this call was a no-op).
///
/// `cross_service_grant_revoke` is always `"not_implemented"` — the grant DELETE
/// needs an admin control endpoint that does not exist yet.
fn auto_revoke_audit_detail(outcome: AutoRevokeOutcome) -> serde_json::Value {
    serde_json::json!({
        "local_alias_disabled": matches!(
            outcome,
            AutoRevokeOutcome::LocalRevokedCrossServicePending
        ),
        "already_disabled": matches!(
            outcome,
            AutoRevokeOutcome::AlreadyLocalRevokedCrossServicePending
        ),
        "cross_service_grant_revoke": "not_implemented",
    })
}

/// Auto-revoke an abusive alias (§7), HONESTLY (Fix 6).
///
/// The abuse auto-revoke has two halves:
///   1. **Local disable (DONE here).** Auth owns `app_user_identities.relay_email`,
///      so it sets `revoked_at = now()` on its OWN connection — the IMMEDIATE
///      protection that stops THIS service's forwarding right away
///      (`resolve_active_alias` then returns `None`).
///   2. **Cross-service grant revoke (NOT done — no endpoint yet).** Deleting
///      the `zeroship.oauth_grants` row requires an admin-authenticated control
///      endpoint that does not exist. We do NOT claim it happened; we log it as
///      PENDING at WARN so the gap is observable and operators can revoke
///      out-of-band.
///
/// Returns the outcome so the caller audits exactly what occurred (never a
/// fake "revoked" success for the cross-service step).
#[allow(clippy::future_not_send)]
async fn request_alias_auto_revoke(
    db: &compio_postgres::Client,
    target: &relay::AliasTarget,
) -> AutoRevokeOutcome {
    // (1) Disable our OWN forwarding immediately — the protection we can apply.
    let outcome = match relay::revoke_local_alias(db, &target.app_client_id, target.global_user_id)
        .await
    {
        Ok(n) if n > 0 => AutoRevokeOutcome::LocalRevokedCrossServicePending,
        Ok(_) => AutoRevokeOutcome::AlreadyLocalRevokedCrossServicePending,
        Err(e) => {
            tracing::error!(
                error = %e,
                app_client_id = %target.app_client_id,
                global_user_id = %target.global_user_id,
                "relay auto-revoke: LOCAL alias disable failed"
            );
            AutoRevokeOutcome::Failed
        }
    };

    // (2) The cross-service grant revoke is NOT implemented — do NOT pretend it
    // ran. Surface the gap at WARN so it is observable.
    tracing::warn!(
        app_client_id = %target.app_client_id,
        global_user_id = %target.global_user_id,
        local_outcome = %outcome.audit_outcome(),
        "relay auto-revoke: local forwarding disabled; cross-service grant revoke \
         is PENDING (admin-authenticated control endpoint not implemented)"
    );
    outcome
}

#[cfg(test)]
mod tests {
    use super::{auto_revoke_audit_detail, email_domain, AutoRevokeOutcome};

    #[test]
    fn email_domain_omits_local_part() {
        assert_eq!(email_domain("victim@example.com"), "example.com");
        assert_eq!(email_domain("not-an-email"), "");
    }

    /// Regression (Batch B fix, minor): the audit-detail booleans must NOT
    /// conflate "this call disabled the alias" with "the alias IS disabled".
    /// `local_alias_disabled` is true ONLY for the newly-revoked outcome (1 row
    /// stamped); the idempotent re-trigger (0 rows) sets the separate
    /// `already_disabled` flag instead — so summing `local_alias_disabled`
    /// downstream counts only genuine new revokes, never re-triggers.
    #[test]
    fn auto_revoke_audit_detail_disambiguates_newly_vs_already_disabled() {
        // Newly revoked: local_alias_disabled = true, already_disabled = false.
        let newly = auto_revoke_audit_detail(AutoRevokeOutcome::LocalRevokedCrossServicePending);
        assert_eq!(newly["local_alias_disabled"], serde_json::json!(true));
        assert_eq!(newly["already_disabled"], serde_json::json!(false));
        assert_eq!(
            newly["cross_service_grant_revoke"],
            serde_json::json!("not_implemented")
        );

        // Idempotent re-trigger: this call revoked 0 rows — must NOT set
        // local_alias_disabled (the conflation bug), but DOES set already_disabled.
        let already =
            auto_revoke_audit_detail(AutoRevokeOutcome::AlreadyLocalRevokedCrossServicePending);
        assert_eq!(already["local_alias_disabled"], serde_json::json!(false));
        assert_eq!(already["already_disabled"], serde_json::json!(true));

        // Failed local disable: neither flag is set.
        let failed = auto_revoke_audit_detail(AutoRevokeOutcome::Failed);
        assert_eq!(failed["local_alias_disabled"], serde_json::json!(false));
        assert_eq!(failed["already_disabled"], serde_json::json!(false));
    }

    /// The audit `outcome` string is never a bare `"success"` — it always names
    /// the cross-service step as pending/failed so the trail is honest.
    #[test]
    fn auto_revoke_outcome_never_bare_success() {
        for outcome in [
            AutoRevokeOutcome::LocalRevokedCrossServicePending,
            AutoRevokeOutcome::AlreadyLocalRevokedCrossServicePending,
            AutoRevokeOutcome::Failed,
        ] {
            assert_ne!(outcome.audit_outcome(), "success");
        }
        assert_eq!(
            AutoRevokeOutcome::LocalRevokedCrossServicePending.audit_outcome(),
            "local_revoked_cross_service_pending"
        );
    }
}
