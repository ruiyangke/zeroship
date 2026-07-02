//! `POST /me/delete` + `POST /me/delete/cancel` — the authenticated
//! account-deletion request/undo surface (ISS-12 / GDPR Art. 17).
//!
//! `/me/delete` does NOT erase anything synchronously. It begins the lifecycle:
//! soft-disable + schedule + revoke sessions (all in one DB transaction via
//! [`users::request_deletion`]), then sends a confirm/undo email and audits.
//! The irreversible erasure happens later, after the grace window, in
//! `cron::account_reaper`.
//!
//! `/me/delete/cancel` reverses an in-flight request within the grace window
//! ([`users::cancel_deletion`]) and audits.
//!
//! Both require the `__Host-zsidp_session` cookie + a matching CSRF token,
//! exactly like `me::unlink`.

use std::sync::Arc;

use ntex::http::header::{HeaderValue, COOKIE, LOCATION};
use ntex::web::{self, HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;

use crate::audit::{self, AuditEvent};
use crate::config::AuthConfig;
use crate::cron::account_reaper::GRACE_DAYS;
use crate::csrf;
use crate::oidc;
use crate::sessions::login as session_cookie;
use crate::store::sessions;
use crate::store::users::{self, UserRow};
use zeroship_mailer::templates::{
    build_email, AccountDeletionRequestedHtml, AccountDeletionRequestedText,
};
use zeroship_mailer::{Address, Mailer};

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    pub csrf: String,
}

/// `POST /me/delete` — begin an account-deletion request.
///
/// On success (or when nothing changed because a request was already in
/// flight), 302 back to `/me` — the page then shows the deactivated/scheduled
/// state. CSRF / session failures behave like the rest of `/me`.
#[allow(clippy::future_not_send, clippy::too_many_arguments)]
pub async fn request(
    req: HttpRequest,
    form: web::types::Form<CsrfForm>,
    cfg: web::types::State<Arc<AuthConfig>>,
    db: web::types::State<Arc<compio_postgres::Client>>,
    mailer: web::types::State<Arc<dyn Mailer>>,
    issuer: web::types::State<Arc<oidc::Issuer>>,
) -> HttpResponse {
    if !csrf_ok(&req, &form.csrf, cfg.insecure_dev) {
        return redirect_to_login();
    }
    let Some(user) = resolve_user(&req, db.as_ref(), cfg.insecure_dev).await else {
        return redirect_to_login();
    };

    let request = match users::request_deletion(db.as_ref(), user.id, GRACE_DAYS).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            // Already anonymized / gone — nothing to do. Land on /me.
            return redirect_to_me();
        }
        Err(e) => {
            tracing::error!(error = %e, user_id = %user.id, "request_deletion failed");
            return redirect_to_me();
        }
    };

    match oidc::backchannel_logout::emit_for_user(db.as_ref(), issuer.as_ref(), user.id).await {
        Ok(report) => tracing::info!(
            user_id = %user.id,
            attempted = report.attempted,
            delivered = report.delivered,
            "account-deletion: emitted OIDC back-channel logout tokens"
        ),
        Err(e) => tracing::error!(
            error = %e,
            user_id = %user.id,
            "account-deletion: BCL emission failed"
        ),
    }

    // Confirm/undo email (best-effort: a send failure must not change the
    // outcome — the deletion is already scheduled and audited).
    send_confirmation(&cfg, &**mailer, db.as_ref(), &request).await;

    audit::emit(
        db.as_ref(),
        &AuditEvent {
            event_type: "account_deletion_requested",
            outcome: "success",
            user_id: Some(&user.id),
            detail: json!({
                "scheduled_for": request.scheduled_for.to_rfc3339(),
                "grace_days": GRACE_DAYS,
                "idp_sessions_revoked": request.idp_sessions_revoked,
                "gateway_sessions_revoked": request.gateway_sessions_revoked,
            }),
            ..AuditEvent::from_request(&req)
        },
    )
    .await;

    redirect_to_me()
}

/// `POST /me/delete/cancel` — cancel an in-flight request within the grace
/// window. 302 back to `/me` regardless (the page reflects the restored state).
///
/// Note: a successful cancel re-enables the account, so the session that was
/// revoked at request time no longer validates — the redirect to `/me` will
/// bounce to `/login`, which is the intended "sign in fresh" behaviour.
#[allow(clippy::future_not_send)]
pub async fn cancel(
    req: HttpRequest,
    form: web::types::Form<CsrfForm>,
    cfg: web::types::State<Arc<AuthConfig>>,
    db: web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    if !csrf_ok(&req, &form.csrf, cfg.insecure_dev) {
        return redirect_to_login();
    }
    let Some(user) = resolve_user(&req, db.as_ref(), cfg.insecure_dev).await else {
        return redirect_to_login();
    };

    match users::cancel_deletion(db.as_ref(), user.id).await {
        Ok(cancelled) => {
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "account_deletion_cancelled",
                    outcome: if cancelled { "success" } else { "failure" },
                    user_id: Some(&user.id),
                    detail: json!({ "cancelled": cancelled }),
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
        }
        Err(e) => {
            tracing::error!(error = %e, user_id = %user.id, "cancel_deletion failed");
        }
    }
    redirect_to_me()
}

// ─── helpers ─────────────────────────────────────────────────

async fn send_confirmation(
    cfg: &AuthConfig,
    mailer: &dyn Mailer,
    db: &compio_postgres::Client,
    request: &users::DeletionRequest,
) {
    let link = format!("{}/me", cfg.public_url());
    let name_hint = request.name.split_whitespace().next().unwrap_or("there");
    let scheduled = request.scheduled_for.format("%Y-%m-%d").to_string();
    let html = AccountDeletionRequestedHtml {
        name: name_hint,
        link: &link,
        scheduled_for: &scheduled,
        grace_days: GRACE_DAYS,
    }
    .render_or_empty();
    let text = AccountDeletionRequestedText {
        name: name_hint,
        link: &link,
        scheduled_for: &scheduled,
        grace_days: GRACE_DAYS,
    }
    .render_or_empty();

    let msg = build_email(
        Address {
            email: request.email.clone(),
            name: Some(request.name.clone()),
        },
        Address {
            email: cfg.mail_from_email.clone(),
            name: Some(cfg.mail_from_name.clone()),
        },
        "Your zeroship account is scheduled for deletion".into(),
        text,
        html,
        vec!["account-deletion".into()],
    );
    if let Err(e) = mailer.send(db, msg).await {
        tracing::warn!(error = %e, user_id = %request.user_id, "account-deletion confirmation email send failed");
    }
}

/// Tiny `render()`-or-empty shim so a template error degrades to an empty body
/// rather than aborting the (already-committed) deletion flow.
trait RenderOrEmpty {
    fn render_or_empty(&self) -> String;
}
impl<T: askama::Template> RenderOrEmpty for T {
    fn render_or_empty(&self) -> String {
        self.render().unwrap_or_default()
    }
}

fn csrf_ok(req: &HttpRequest, form_token: &str, insecure_dev: bool) -> bool {
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    csrf::parse_cookie(cookie_header, insecure_dev)
        .as_deref()
        .is_some_and(|c| csrf::matches(form_token, c))
}

/// Resolve the signed-in user from the `__Host-zsidp_session` cookie, or
/// `None`. Mirrors `me::resolve_user` (kept local — that one is private).
#[allow(clippy::future_not_send)]
async fn resolve_user(
    req: &HttpRequest,
    db: &compio_postgres::Client,
    insecure_dev: bool,
) -> Option<UserRow> {
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let session_id = session_cookie::parse_cookie(cookie_header, insecure_dev)?;
    let session = sessions::validate(db, session_id).await.ok().flatten()?;
    users::find_by_id(db, &session.user_id.to_string())
        .await
        .ok()
        .flatten()
}

fn redirect_to_login() -> HttpResponse {
    let mut r = HttpResponse::Found();
    r.header(LOCATION, HeaderValue::from_static("/login"));
    r.finish()
}

fn redirect_to_me() -> HttpResponse {
    let mut r = HttpResponse::Found();
    r.header(LOCATION, HeaderValue::from_static("/me"));
    r.finish()
}
