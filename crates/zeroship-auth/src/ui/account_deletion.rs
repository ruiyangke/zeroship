//! `POST /me/delete` + `GET,POST /me/delete/cancel` - the account-deletion
//! request/undo surface (ISS-12 / GDPR Art. 17).
//!
//! `/me/delete` does NOT erase anything synchronously. It begins the lifecycle:
//! refuse if the control plane says this human is the last owner of a live
//! organization, then mark deletion requested + schedule + revoke sessions + mint
//! the undo token (one DB transaction via [`users::request_deletion`]), then send
//! the confirm/undo email and audit. The irreversible erasure happens later,
//! after the grace window, in `cron::account_reaper`.
//!
//! # The two refusals on the request, and why both are refusals
//!
//! A human who is the ONLY owner of a live organization cannot be erased:
//! `organization_members.user_id` is `ON DELETE CASCADE`, so their seat goes
//! with them and nobody can be seated again - and the delete would abort against
//! `projects_organization_id_fkey` anyway, naming a constraint to someone who
//! asked to be deleted. It is refused HERE, before the window opens, because
//! inside the window they can neither sign in nor transfer.
//!
//! A preflight that could not be COMPUTED is refused the same way. Proceeding
//! would mean deleting a person on the strength of a check that did not run.
//!
//! # The undo, and why it is a token
//!
//! `/me/delete/cancel` was session-authenticated and, MEASURED, unreachable:
//! `request_deletion` bumps `credential_version`, stamps `deletion_requested_at`
//! and revokes every `idp_sessions` row, and `sessions::validate` requires all
//! three to be otherwise. `check_user_eligible` refuses re-login a fourth time.
//! The handler could not resolve a caller, ever.
//!
//! So the undo credential is the single-use token mailed with the confirmation
//! (`identity::deletion_cancel`), and the route is a `GET` form plus a
//! CSRF-checked `POST`, exactly like `/reset`. Nothing about the revocation is
//! relaxed to make it work.

use std::sync::Arc;

use askama::Template;
use ntex::http::header::{HeaderValue, COOKIE, LOCATION, SET_COOKIE};
use ntex::http::StatusCode;
use ntex::web::{self, HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;

use crate::audit::{self, AuditEvent};
use crate::config::AuthConfig;
use crate::control_client::{self, PreflightError};
use crate::cron::account_reaper::GRACE_DAYS;
use crate::csrf;
use crate::headers;
use crate::identity::deletion_cancel;
use crate::oidc;
use crate::oidc::refresh::RefreshSessionPool;
use crate::sessions::login as session_cookie;
use crate::store::sessions;
use crate::store::users::{self, UserRow};
use crate::ui::{DeletionBlockedPage, DeletionBlocker, DeletionCancelPage, DeletionDebt};
use zeroship_mailer::templates::{
    build_email, AccountDeletionRequestedHtml, AccountDeletionRequestedText,
};
use zeroship_mailer::{Address, Mailer};

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    pub csrf: String,
}

/// `GET /me/delete/cancel?token=<token>` query. `token`, never `t`: the
/// token-redeem handlers were inconsistent about this once and the short name
/// is not coming back.
#[derive(Debug, Deserialize)]
pub struct CancelQuery {
    pub token: String,
}

#[derive(Debug, Deserialize)]
pub struct CancelForm {
    pub csrf: String,
    pub token: String,
}

/// `POST /me/delete` - begin an account-deletion request.
///
/// On success (or when nothing changed because a request was already in
/// flight), 302 back to `/me` - the page then shows the deactivated/scheduled
/// state. A sole-ownership blocker renders the refusal page with 409; an
/// unanswerable preflight renders it with 503. CSRF / session failures behave
/// like the rest of `/me`.
#[allow(clippy::future_not_send, clippy::too_many_arguments)]
pub async fn request(
    req: HttpRequest,
    form: web::types::Form<CsrfForm>,
    cfg: web::types::State<Arc<AuthConfig>>,
    db: web::types::State<Arc<compio_postgres::Client>>,
    refresh_pool: web::types::State<RefreshSessionPool>,
    mailer: web::types::State<Arc<dyn Mailer>>,
    issuer: web::types::State<Arc<oidc::Issuer>>,
    service_keyring: web::types::State<Arc<zeroship_core::service_peers::ServiceKeyring>>,
) -> HttpResponse {
    if !csrf_ok(&req, &form.csrf) {
        return redirect_to_login();
    }
    let Some(user) = resolve_user(&req, db.as_ref()).await else {
        return redirect_to_login();
    };

    // The ownership precondition, before anything is written. Every arm that is
    // not a clear answer is a refusal.
    match control_client::erasure_preflight(cfg.control_url(), &service_keyring, &user.id).await {
        Ok(report) if report.is_clear() => {}
        Ok(report) => {
            let blockers = report
                .blockers
                .iter()
                .map(|b| DeletionBlocker {
                    organization_name: b.organization_name.clone(),
                    organization_slug: b.organization_slug.clone(),
                    personal: b.personal,
                    instruction: b.remedy.instruction(),
                })
                .collect::<Vec<_>>();
            let owing = report
                .billing_blockers
                .iter()
                .map(|b| DeletionDebt {
                    organization_name: b.organization_name.clone(),
                    organization_slug: b.organization_slug.clone(),
                    personal: b.personal,
                    dissolved: b.dissolved,
                    summary: debt_summary(b),
                    instruction: b.remedy.instruction(),
                })
                .collect::<Vec<_>>();
            // The reason names WHICH rule refused, so the audit trail can be
            // read for one of them without re-deriving it from the id list.
            // Money wins when both apply: it is the refusal a succession does
            // not necessarily clear.
            let reason = if report.billing_blockers.is_empty() {
                "sole_owner"
            } else {
                "outstanding_billing"
            };
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "account_deletion_refused",
                    outcome: "failure",
                    user_id: Some(&user.id),
                    detail: json!({
                        "reason": reason,
                        "organizations": report
                            .blockers
                            .iter()
                            .map(|b| b.organization_id.as_str())
                            .collect::<Vec<_>>(),
                        "owing_organizations": report
                            .billing_blockers
                            .iter()
                            .map(|b| b.organization_id.as_str())
                            .collect::<Vec<_>>(),
                        "owed_cents": report
                            .billing_blockers
                            .iter()
                            .map(|b| b.owed_cents)
                            .sum::<i64>(),
                    }),
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
            return render_blocked(&DeletionBlockedPage {
                blockers,
                owing,
                unavailable: false,
            });
        }
        Err(e) => {
            // NoCredential is a signing fault on this process and the others are
            // transport; both mean the same thing to the person in front of the
            // form, and neither is a licence to proceed.
            tracing::error!(
                error = %e,
                user_id = user.id.as_str(),
                credentialed = !matches!(e, PreflightError::NoCredential(_)),
                "account deletion refused: erasure preflight unavailable"
            );
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "account_deletion_refused",
                    outcome: "failure",
                    user_id: Some(&user.id),
                    detail: json!({ "reason": "preflight_unavailable" }),
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
            return render_blocked(&DeletionBlockedPage {
                blockers: Vec::new(),
                owing: Vec::new(),
                unavailable: true,
            });
        }
    }

    let pool = match refresh_pool.checkout_pool("account deletion").await {
        Ok(pool) => pool,
        Err(e) => {
            tracing::error!(error = %e, user_id = user.id.as_str(), "account deletion pool unavailable");
            return redirect_to_me();
        }
    };
    let mut conn = match pool.acquire().await {
        Ok(conn) => conn,
        Err(e) => {
            tracing::error!(error = %e, user_id = user.id.as_str(), "account deletion session unavailable");
            return redirect_to_me();
        }
    };
    let request = match users::request_deletion(&mut conn, &user.id, GRACE_DAYS).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            // No such user. Land on /me.
            return redirect_to_me();
        }
        Err(e) => {
            tracing::error!(error = %e, user_id = user.id.as_str(), "request_deletion failed");
            return redirect_to_me();
        }
    };
    drop(conn);
    drop(pool);

    match oidc::backchannel_logout::emit_for_user(db.as_ref(), issuer.as_ref(), &user.id).await {
        Ok(report) => tracing::info!(
            user_id = user.id.as_str(),
            attempted = report.attempted,
            delivered = report.delivered,
            "account-deletion: emitted OIDC back-channel logout tokens"
        ),
        Err(e) => tracing::error!(
            error = %e,
            user_id = user.id.as_str(),
            "account-deletion: BCL emission failed"
        ),
    }

    // Confirm/undo email (best-effort: a send failure must not change the
    // outcome - the deletion is already scheduled and audited).
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

/// `GET /me/delete/cancel?token=<token>` - render the undo confirmation.
///
/// The token is NOT peeked at here, matching `/reset`: validity is decided by
/// the one statement that consumes it. Rendering a form against a dead token
/// costs a page the POST then refuses; checking first costs a round trip on
/// every render AND opens a check-then-use gap that the POST would have to
/// close anyway.
//
// ntex's per-thread service futures are intentionally `!Send`.
#[allow(clippy::unused_async, clippy::future_not_send)]
pub async fn cancel_form(query: web::types::Query<CancelQuery>) -> HttpResponse {
    render_cancel(&query.token, None, false, StatusCode::OK)
}

/// `POST /me/delete/cancel` - spend the mailed token and cancel the deletion.
///
/// There is NO session arm. The request this reverses revoked every session in
/// the transaction that scheduled it, so a session-authenticated cancel is a
/// branch that cannot be taken; keeping one would be a fallback that reads as
/// protection and never runs.
#[allow(clippy::future_not_send)]
pub async fn cancel(
    req: HttpRequest,
    form: web::types::Form<CancelForm>,
    db: web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    if !csrf_ok(&req, &form.csrf) {
        return render_cancel(
            &form.token,
            Some("invalid request"),
            false,
            StatusCode::BAD_REQUEST,
        );
    }

    match deletion_cancel::redeem(db.as_ref(), &form.token).await {
        Ok(Some(cancelled)) => {
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "account_deletion_cancelled",
                    outcome: "success",
                    user_id: Some(&cancelled.user_id),
                    detail: json!({ "via": "emailed_token" }),
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
            render_cancel("", None, true, StatusCode::OK)
        }
        Ok(None) => {
            // One answer for "no such token", "already spent" and "expired".
            // Splitting them would tell an unauthenticated caller holding a
            // guessed token whether that account is pending deletion.
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "account_deletion_cancelled",
                    outcome: "failure",
                    user_id: None,
                    detail: json!({ "via": "emailed_token", "reason": "token_not_live" }),
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
            render_cancel(
                &form.token,
                Some("this link is invalid or has expired"),
                false,
                StatusCode::BAD_REQUEST,
            )
        }
        Err(e) => {
            tracing::error!(error = %e, "deletion_cancel redeem failed");
            render_cancel(
                &form.token,
                Some("please try again"),
                false,
                StatusCode::INTERNAL_SERVER_ERROR,
            )
        }
    }
}

// ─── helpers ─────────────────────────────────────────────────

async fn send_confirmation(
    cfg: &AuthConfig,
    mailer: &dyn Mailer,
    db: &compio_postgres::Client,
    request: &users::DeletionRequest,
) {
    // The undo link, carrying the single-use token. This used to point at
    // `/me`, which the request had just made unreachable - so the email
    // advertised a thirty-day window whose only door was locked.
    let link = format!(
        "{}/me/delete/cancel?token={}",
        cfg.public_url(),
        request.cancel_token
    );
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
            email: cfg.settings.mail_from_email.get().clone(),
            name: Some(cfg.settings.mail_from_name.get().clone()),
        },
        "Your zeroship account is scheduled for deletion".into(),
        text,
        html,
        vec!["account-deletion".into()],
    );
    if let Err(e) = mailer.send(db, msg).await {
        tracing::warn!(error = %e, user_id = request.user_id.as_str(), "account-deletion confirmation email send failed");
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

/// Render the undo page. Sets a fresh CSRF cookie on every render so the GET
/// that arrives from an email link hands the POST a matching pair.
fn render_cancel(
    token: &str,
    error: Option<&str>,
    cancelled: bool,
    status: StatusCode,
) -> HttpResponse {
    let csrf_token = csrf::generate_token();
    // Independent per-response CSP script nonce - must NOT be the CSRF token
    // (which is also a non-HttpOnly cookie + plaintext form field). See L3.
    let script_nonce = csrf::generate_token();
    let page = DeletionCancelPage {
        token,
        csrf: &csrf_token,
        script_nonce: &script_nonce,
        error,
        cancelled,
    };
    let body = page
        .render()
        .unwrap_or_else(|_| "<h1>error</h1>".to_string());
    let mut resp = HttpResponse::build(status);
    resp.content_type("text/html; charset=utf-8");
    resp.header("Cache-Control", "no-store");
    resp.header("Pragma", "no-cache");
    resp.header(
        "Content-Security-Policy",
        headers::content_security_policy_with_script_nonce(&script_nonce),
    );
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token));
    resp.body(body)
}

/// Render the request refusal. `409` for a blocker the person can act on,
/// `503` for a preflight that did not answer - the two are different problems
/// and a retry only helps with one of them.
fn render_blocked(page: &DeletionBlockedPage) -> HttpResponse {
    let status = if page.unavailable {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::CONFLICT
    };
    let body = page
        .render()
        .unwrap_or_else(|_| "<h1>error</h1>".to_string());
    let mut resp = HttpResponse::build(status);
    resp.content_type("text/html; charset=utf-8");
    resp.header("Cache-Control", "no-store");
    resp.body(body)
}

/// What this organization owes, in one sentence a person can read.
///
/// Built here rather than in the template because the template has no business
/// dividing a minor unit by a hundred, and because the two arms are different
/// SENTENCES rather than different values: cash already claimed can be quoted
/// as an amount, while usage that was never invoiced has no price yet and must
/// not be quoted as one.
fn debt_summary(blocker: &control_client::BillingBlocker) -> String {
    let currency = blocker.currency.to_uppercase();
    let owed = format!(
        "{}.{:02} {currency}",
        blocker.owed_cents / 100,
        (blocker.owed_cents % 100).abs()
    );
    let invoices = if blocker.unpaid_invoice_count == 1 {
        "invoice".to_string()
    } else {
        format!("{} invoices", blocker.unpaid_invoice_count)
    };
    let periods = if blocker.unbilled_period_count == 1 {
        "one billing period".to_string()
    } else {
        format!("{} billing periods", blocker.unbilled_period_count)
    };
    match (
        blocker.unpaid_invoice_count > 0,
        blocker.unbilled_period_count > 0,
    ) {
        (true, true) => format!(
            "{owed} outstanding on {invoices}, and usage in {periods} that has not been \
             invoiced yet"
        ),
        (true, false) => format!("{owed} outstanding on {invoices}"),
        (false, true) => {
            format!("usage in {periods} that has not been invoiced yet")
        }
        // Unreachable from a blocker the control plane built - it only raises
        // one when at least one arm is non-empty. Rendered rather than
        // panicked because a refusal page must always say something.
        (false, false) => "outstanding billing".to_string(),
    }
}

fn csrf_ok(req: &HttpRequest, form_token: &str) -> bool {
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    csrf::parse_cookie(cookie_header)
        .as_deref()
        .is_some_and(|c| csrf::matches(form_token, c))
}

/// Resolve the signed-in user from the `__Host-zsidp_session` cookie, or
/// `None`. Mirrors `me::resolve_user` (kept local — that one is private).
#[allow(clippy::future_not_send)]
async fn resolve_user(req: &HttpRequest, db: &compio_postgres::Client) -> Option<UserRow> {
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let session_id = session_cookie::parse_cookie(cookie_header)?;
    let session = sessions::validate(db, session_id).await.ok().flatten()?;
    users::find_by_id(db, &session.user_id).await.ok().flatten()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The query struct must reject `?t=`. Every token-redeem handler here
    /// settled on `token`, and a silent alias is how they diverged the first
    /// time. Decoded the way ntex's `Query<T>` extractor does - pairs into a
    /// `MapDeserializer` - so this stays a unit test with no server.
    #[test]
    fn the_cancel_query_takes_token_and_not_t() {
        fn parse(q: &str) -> std::result::Result<CancelQuery, serde::de::value::Error> {
            use serde::Deserialize;
            let pairs: Vec<(String, String)> = url::form_urlencoded::parse(q.as_bytes())
                .into_owned()
                .collect();
            CancelQuery::deserialize(serde::de::value::MapDeserializer::new(pairs.into_iter()))
        }
        assert_eq!(parse("token=abc").expect("token= must parse").token, "abc");
        assert!(parse("t=abc").is_err(), "the short name is not an alias");
    }

    /// A blocker page is `409` (act on it) and an unanswerable preflight is
    /// `503` (retry). Collapsing them would tell a person to retry a refusal
    /// no retry can clear.
    #[test]
    fn the_two_refusals_do_not_share_a_status() {
        let blocked = render_blocked(&DeletionBlockedPage {
            blockers: vec![DeletionBlocker {
                organization_name: "Acme".into(),
                organization_slug: "acme".into(),
                personal: false,
                instruction: "transfer ownership to another member of this organization",
            }],
            owing: Vec::new(),
            unavailable: false,
        });
        assert_eq!(blocked.status(), StatusCode::CONFLICT);
        let unavailable = render_blocked(&DeletionBlockedPage {
            blockers: Vec::new(),
            owing: Vec::new(),
            unavailable: true,
        });
        assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    fn owing_blocker(
        owed_cents: i64,
        unpaid_invoice_count: i64,
        unbilled_period_count: i64,
    ) -> control_client::BillingBlocker {
        control_client::BillingBlocker {
            organization_id: "org_1".into(),
            organization_slug: "acme".into(),
            organization_name: "Acme".into(),
            personal: false,
            dissolved: true,
            owed_cents,
            currency: "usd".into(),
            unpaid_invoice_count,
            unbilled_period_count,
            remedy: control_client::BillingRemedy::SettleInvoices,
        }
    }

    /// A MONEY refusal is a 409 with no ownership blocker beside it, and the
    /// page has to say what is owed and what to do. Without the `owing` arm
    /// this page would render the heading and an empty list, which reads as
    /// "refused for no reason".
    #[test]
    fn the_money_refusal_renders_the_amount_and_the_remedy() {
        let page = DeletionBlockedPage {
            blockers: Vec::new(),
            owing: vec![DeletionDebt {
                organization_name: "Acme".into(),
                organization_slug: "acme".into(),
                personal: false,
                dissolved: true,
                summary: debt_summary(&owing_blocker(1250, 1, 0)),
                instruction: control_client::BillingRemedy::SettleInvoices.instruction(),
            }],
            unavailable: false,
        };
        assert_eq!(render_blocked(&page).status(), StatusCode::CONFLICT);
        let body = page.render().expect("the refusal page renders");
        assert!(
            body.contains("12.50 USD"),
            "the amount is on the page: {body}"
        );
        assert!(body.contains("Acme"), "the organization is named: {body}");
        assert!(
            body.contains("add a payment method"),
            "the remedy is on the page: {body}"
        );
        assert!(
            body.contains("already closed"),
            "a dissolved organization says so: {body}"
        );
    }

    /// The three shapes of debt read as three different sentences, and only
    /// the invoice arms quote money. Usage that was never invoiced has no
    /// price yet, so a summary that put an amount on it would be quoting a
    /// figure no invoice will match.
    #[test]
    fn the_summary_quotes_money_only_for_invoices() {
        let both = debt_summary(&owing_blocker(1250, 2, 1));
        assert!(
            both.contains("12.50 USD") && both.contains("2 invoices"),
            "{both}"
        );
        assert!(both.contains("one billing period"), "{both}");

        let invoices_only = debt_summary(&owing_blocker(500, 1, 0));
        assert!(invoices_only.contains("5.00 USD"), "{invoices_only}");
        assert!(!invoices_only.contains("billing period"), "{invoices_only}");

        let usage_only = debt_summary(&owing_blocker(0, 0, 3));
        assert!(usage_only.contains("3 billing periods"), "{usage_only}");
        assert!(
            !usage_only.contains("USD"),
            "unbilled usage has no price: {usage_only}"
        );
    }
}
