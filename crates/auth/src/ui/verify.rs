//! `/verify` GET handler — redeems an email-verification token and marks
//! `auth.users.email_verified_at = NOW()`.
//!
//! Per proposal §8.3 (Phase 5). The token is issued by [`crate::ui::signup`]
//! on a successful signup and emailed to the user. Clicking the link in
//! the email lands here; we atomically consume the row via
//! [`crate::identity::verification::redeem`], stamp the user's
//! `email_verified_at`, emit an audit event, and render a confirmation
//! page.
//!
//! No session is minted here — verification is decoupled from sign-in.
//! The success page links back to `/login` so the user can continue.

use askama::Template;
use ntex::web::HttpResponse;
use serde::Deserialize;
use std::sync::Arc;

use crate::audit::{self, AuditEvent};
use crate::identity::verification;
use crate::ui::{ErrorPage, PublicErrorMessage, VerifyOkPage};

#[derive(Debug, Deserialize)]
pub struct VerifyQuery {
    /// Raw verification token. base64url, no padding (32-byte CSPRNG).
    pub token: String,
}

/// `/verify?token=<token>` — atomically redeem the verification token, set
/// `email_verified_at = NOW()` on the user, render the success page.
///
/// Invalid/expired tokens render the generic [`ErrorPage`]; the user can
/// request a fresh verification email from the (future) `/me` action.
#[allow(clippy::future_not_send)]
pub async fn get(
    query: ntex::web::types::Query<VerifyQuery>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    // 1. Redeem atomically.
    let redeemed = match verification::redeem(db.as_ref(), &query.token).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "verification_redeemed",
                    outcome: "failure",
                    detail: serde_json::json!({ "reason": "invalid_or_expired" }),
                    ..Default::default()
                },
            )
            .await;
            return render_error(PublicErrorMessage::SessionExpired);
        }
        Err(e) => {
            tracing::error!(error = %e, "verification redeem db error");
            return render_error(PublicErrorMessage::ContactSupport);
        }
    };

    // 2. Stamp `email_verified_at` on the user. Guarded by
    //    `email_verified_at IS NULL` so a duplicate redeem (e.g. user
    //    re-issues then redeems the original after the new one already
    //    verified) doesn't bump the timestamp — first verification wins.
    if let Err(e) = db
        .execute(
            "UPDATE auth.users SET email_verified_at = NOW(), updated_at = NOW() \
             WHERE id = $1 AND email_verified_at IS NULL",
            &[&redeemed.user_id],
        )
        .await
    {
        tracing::error!(error = %e, user_id = %redeemed.user_id, "set email_verified_at failed");
        return render_error(PublicErrorMessage::ContactSupport);
    }

    audit::emit(
        db.as_ref(),
        &AuditEvent {
            event_type: "verification_redeemed",
            outcome: "success",
            user_id: Some(&redeemed.user_id),
            ..Default::default()
        },
    )
    .await;

    let page = VerifyOkPage {
        email: &redeemed.email,
    };
    let body = page
        .render()
        .unwrap_or_else(|_| "<h1>Email verified.</h1>".to_string());
    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.body(body)
}

fn render_error(message: PublicErrorMessage) -> HttpResponse {
    let page = ErrorPage {
        message,
        error_code: message.error_code(),
    };
    let body = page
        .render()
        .unwrap_or_else(|_| format!("<h1>{}</h1>", message.as_str()));
    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.body(body)
}
