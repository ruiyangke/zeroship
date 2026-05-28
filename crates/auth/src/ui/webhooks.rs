//! Webhook handlers — Postmark bounce/complaint today; SES-SNS deferred to Phase 6.
//!
//! Postmark posts JSON to `POST /webhooks/postmark` for delivery events.
//! Authentication is HTTP Basic (configured per-server in the Postmark
//! dashboard; the user/password pair is matched against
//! `AUTH_POSTMARK_WEBHOOK_USER` / `AUTH_POSTMARK_WEBHOOK_PASSWORD`). On
//! every request we:
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

use std::sync::Arc;

use ntex::http::header::AUTHORIZATION;
use ntex::web::{
    types::{Json, State},
    HttpRequest, HttpResponse,
};

use crate::audit::{self, AuditEvent};
use crate::config::AuthConfig;
use crate::mailer::bounce::{bounce_type_is_permanent, verify_basic_auth, PostmarkEvent};
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
                tracing::error!(error = %e, email = %b.email, "suppression add failed");
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
                    ..Default::default()
                },
            )
            .await;
        }
        PostmarkEvent::Bounce(b) => {
            // Soft bounce — log only. Transient failures (mailbox full,
            // server down) recover; suppressing on them would
            // permanently block legitimate users.
            tracing::info!(email = %b.email, kind = %b.r#type, "postmark soft bounce");
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
                tracing::error!(error = %e, email = %c.email, "suppression add failed");
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
                    ..Default::default()
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
