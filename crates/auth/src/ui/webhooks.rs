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
//!    `auth.email_suppressions` (the same table the mailer's
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
//!    `auth.email_suppressions`. Transient bounces log only.
//!
//! Reference: <https://docs.aws.amazon.com/sns/latest/dg/sns-verify-signature-of-message.html>

use std::sync::Arc;

use ntex::http::header::AUTHORIZATION;
use ntex::web::{
    types::{Json, State},
    HttpRequest, HttpResponse,
};

use crate::audit::{self, AuditEvent};
use crate::config::AuthConfig;
use crate::mailer::bounce::{bounce_type_is_permanent, verify_basic_auth, PostmarkEvent};
use crate::mailer::sns::{self, is_valid_sns_cert_url, SesEvent, SnsEnvelope};
use crate::store::suppressions;

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

#[cfg(test)]
mod tests {
    use super::email_domain;

    #[test]
    fn email_domain_omits_local_part() {
        assert_eq!(email_domain("victim@example.com"), "example.com");
        assert_eq!(email_domain("not-an-email"), "");
    }
}
