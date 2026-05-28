//! `/link` GET + POST handlers — confirm an account-link via password.
//!
//! Hit only via a `PendingLink` token issued by [`crate::identity::linker`]
//! when a federation callback finds the upstream email collides with a
//! locally-credentialed account. Defends against the `failedstartup.com`
//! domain-re-registration attack (proposal §8.2): someone who acquires a
//! previously owned email at a federation provider cannot quietly take
//! over a zeroship account that still has its original password.
//!
//! Flow:
//!
//!   1. GET `/link?token=<pending>` — decode + verify the token, render
//!      [`LinkPage`] with a fresh CSRF cookie, prefilled with `existing_email`
//!      and `provider`.
//!   2. POST `/link` — verify CSRF, re-decode the token, run the
//!      Argon2id verify in `spawn_blocking`. On match: insert the
//!      `auth.identities` row, create an `auth.sessions` row, and call
//!      hydra's `accept_login(login_challenge)` (the SAME challenge the
//!      original `/oauth/<provider>/start` stashed — hydra has been waiting
//!      all along). On mismatch: re-render the form with an error banner
//!      and a fresh CSRF cookie.
//!
//! The handler never tries to look up the user by something else — the
//! HMAC on the pending token IS the binding to `user_id`. A forged or
//! expired token is rejected up-front; we don't even hit PG until the
//! token survives MAC + expiry checks.

use std::sync::Arc;

use askama::Template;
use ntex::http::header::{HeaderValue, COOKIE, LOCATION, SET_COOKIE};
use ntex::web::{HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;

use crate::audit::{self, AuditEvent};
use crate::config::AuthConfig;
use crate::csrf;
use crate::hydra_client::types::AcceptLoginRequest;
use crate::hydra_client::HydraAdmin;
use crate::identity::{eligibility, email as email_validation};
use crate::identity::linker::PendingLink;
use crate::identity::password;
use crate::ratelimit::{self, Bucket, RateLimitDecision};
use crate::sessions::login as session_cookie;
use crate::store::{identities, sessions, users};
use crate::ui::{ErrorPage, LinkPage, PublicErrorMessage};

/// ACR + AMR tags for the post-link `IdP` session.
///
/// The user authenticated via two factors in sequence — federation
/// (OAuth) then a local password — so the session carries both `pwd` and
/// `oauth` in `amr`. We pick the federation provider's ACR as the
/// dominant tag because that's what the relying party will see in the
/// ID token's `acr` claim. (Same shape as the post-callback federation
/// session.)
fn acr_for(provider: &str) -> &'static str {
    match provider {
        "github" => "urn:zeroship:github",
        // Default to google's ACR for now; new providers add an arm.
        _ => "urn:zeroship:google",
    }
}

#[derive(Debug, Deserialize)]
pub struct LinkQuery {
    pub token: String,
}

// ─── /link GET ───────────────────────────────────────────────────────────

/// Render the `/link` form. Rejects invalid or expired tokens with a
/// generic error page (the token has already failed verification, so the
/// user must restart the federation dance).
///
/// `!Send` for the same structural reason every other ntex handler in
/// this crate is: ntex's per-thread service futures hold `Rc`-backed
/// state.
#[allow(clippy::future_not_send)]
pub async fn get(
    query: ntex::web::types::Query<LinkQuery>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
) -> HttpResponse {
    let Some(pending) = PendingLink::decode(&query.token, cfg.stash_signing_key.as_bytes()) else {
        return render_error_page(PublicErrorMessage::SessionExpired);
    };
    if email_validation::validate_email(&pending.email).is_err() {
        return render_error_page(PublicErrorMessage::SessionExpired);
    }

    let csrf_token = csrf::generate_token();
    let page = LinkPage {
        token: &query.token,
        csrf: &csrf_token,
        existing_email: &pending.email,
        provider: &pending.provider,
        error: None,
    };
    let body = match page.render() {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "render link.html failed");
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
    };

    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token, cfg.insecure_dev));
    resp.body(body)
}

// ─── /link POST ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct LinkForm {
    pub csrf: String,
    pub token: String,
    pub password: String,
}

/// Confirm the link.
///
/// Algorithm:
///
///   1. CSRF check (cookie vs form, constant-time).
///   2. Decode the pending token — rejected if MAC fails or expired.
///   3. Look up the user by `pending.user_id`. If absent we still run
///      Argon2 against the dummy hash so the wall-clock matches a real
///      verify (account-enumeration defense, mirroring `/login`).
///   4. Argon2id verify in `spawn_blocking` (~100ms).
///   5. On success: insert `auth.identities`, create an `auth.sessions`
///      row, accept the (still-pending) hydra `login_challenge`, audit
///      `oauth_link_success`, 302 to hydra's `redirect_to` with the
///      session cookie.
///   6. On failure: audit `oauth_link_failed`, re-render the page with
///      an error banner.
#[allow(clippy::too_many_lines, clippy::future_not_send)]
pub async fn post(
    req: HttpRequest,
    form: ntex::web::types::Form<LinkForm>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    admin: ntex::web::types::State<HydraAdmin>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    // 1. CSRF — cheapest check first.
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let cookie_token = csrf::parse_cookie(cookie_header, cfg.insecure_dev);
    if cookie_token
        .as_deref()
        .is_none_or(|c| !csrf::matches(&form.csrf, c))
    {
        return render_error_page(PublicErrorMessage::InvalidRequest);
    }

    // 2. Decode + verify the pending token.
    let Some(pending) = PendingLink::decode(&form.token, cfg.stash_signing_key.as_bytes()) else {
        return render_error_page(PublicErrorMessage::SessionExpired);
    };
    if email_validation::validate_email(&pending.email).is_err() {
        return render_error_page(PublicErrorMessage::SessionExpired);
    }

    if let Err(e) = admin.get_login(&pending.login_challenge).await {
        tracing::warn!(error = %e, challenge = %pending.login_challenge, "link hydra challenge validation failed");
        audit::emit(
            db.as_ref(),
            &AuditEvent {
                event_type: "oauth_link_failed",
                outcome: "failure",
                user_id: Some(&pending.user_id),
                auth_method: Some(&pending.provider),
                detail: json!({ "reason": "login_challenge_invalid" }),
                ..Default::default()
            },
        )
        .await;
        return render_error_page(PublicErrorMessage::InvalidRequest);
    }

    let ip = req
        .connection_info()
        .remote()
        .unwrap_or("0.0.0.0")
        .to_string();
    let link_attempt_key = format!("link_attempt:{}:{ip}", pending.user_id);
    match ratelimit::consume(db.as_ref(), &link_attempt_key, Bucket::LINK_ATTEMPT).await {
        Ok(RateLimitDecision::Allowed) => {}
        Ok(RateLimitDecision::Throttled(_)) => {
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "oauth_link_failed",
                    outcome: "failure",
                    user_id: Some(&pending.user_id),
                    auth_method: Some(&pending.provider),
                    detail: json!({
                        "reason": "rate_limited",
                        "bucket": link_attempt_key,
                    }),
                    ..Default::default()
                },
            )
            .await;
            return render_link_error_with_status(
                &form.token,
                &pending,
                &cfg,
                "too many attempts, try again later",
                ntex::http::StatusCode::TOO_MANY_REQUESTS,
            );
        }
        Err(e) => {
            tracing::error!(error = %e, bucket = %link_attempt_key, "link rate-limit consume failed");
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
    }

    // 3. Look up the user. The pending token's HMAC guarantees the
    // `user_id` came from us — but the row might have been deleted between
    // token issuance and confirmation. Run dummy-hash on the missing arm so
    // the wall-clock matches.
    let user = match users::find_by_email(db.as_ref(), &pending.email).await {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(error = %e, "users::find_by_email failed");
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
    };

    let now = chrono::Utc::now();
    let phc = user
        .as_ref()
        .and_then(|u| {
            let locked = u.locked_until.is_some_and(|t| t > now);
            let disabled = u.disabled_at.is_some();
            if locked || disabled || u.password_hash.is_none() || u.id != pending.user_id {
                None
            } else {
                u.password_hash.clone()
            }
        })
        .unwrap_or_else(|| password::dummy_hash().to_string());

    // 4. Argon2 verify — CPU-bound, must not park the event loop.
    let password_clone = form.password.clone();
    let valid = compio::runtime::spawn_blocking(move || {
        password::verify(&password_clone, &phc).unwrap_or(false)
    })
    .await
    .unwrap_or(false);

    // Re-evaluate the "real user" predicate (mirror the dummy-hash arm).
    let ineligible_user = user.as_ref().filter(|u| {
        u.id == pending.user_id
            && (u.locked_until.is_some_and(|t| t > now) || u.disabled_at.is_some())
    });
    if let Some(u) = ineligible_user {
        audit::emit(
            db.as_ref(),
            &AuditEvent {
                event_type: "oauth_link_failed",
                outcome: "failure",
                user_id: Some(&u.id),
                auth_method: Some(&pending.provider),
                detail: json!({
                    "reason": "account_ineligible",
                    "email": pending.email,
                }),
                ..Default::default()
            },
        )
        .await;
        return render_link_error(
            &form.token,
            &pending,
            &cfg,
            PublicErrorMessage::AccountTemporarilyLocked.as_str(),
        );
    }

    let real_user = user.as_ref().filter(|u| {
        u.locked_until.is_none_or(|t| t <= now)
            && u.disabled_at.is_none()
            && u.password_hash.is_some()
            && u.id == pending.user_id
    });

    let Some(u) = real_user.filter(|_| valid) else {
        audit::emit(
            db.as_ref(),
            &AuditEvent {
                event_type: "oauth_link_failed",
                outcome: "failure",
                user_id: Some(&pending.user_id),
                auth_method: Some(&pending.provider),
                detail: json!({
                    "reason": "invalid_password",
                    "email": pending.email,
                }),
                ..Default::default()
            },
        )
        .await;
        return render_link_error(
            &form.token,
            &pending,
            &cfg,
            "invalid password",
        );
    };

    if let Err(e) = eligibility::check_user_eligible(db.as_ref(), u.id).await {
        if !e.is_account_state() {
            tracing::error!(error = %e, user_id = %u.id, "link eligibility check failed");
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
        audit::emit(
            db.as_ref(),
            &AuditEvent {
                event_type: "oauth_link_failed",
                outcome: "failure",
                user_id: Some(&u.id),
                auth_method: Some(&pending.provider),
                detail: json!({
                    "reason": "account_ineligible",
                    "email": pending.email,
                }),
                ..Default::default()
            },
        )
        .await;
        return render_link_error(
            &form.token,
            &pending,
            &cfg,
            PublicErrorMessage::AccountTemporarilyLocked.as_str(),
        );
    }

    // 5a. Create the identity row.
    if let Err(e) = identities::link(
        db.as_ref(),
        u.id,
        &pending.provider,
        &pending.subject,
        Some(&pending.email),
        None,
    )
    .await
    {
        tracing::error!(error = %e, "identities::link failed");
        return render_error_page(PublicErrorMessage::ContactSupport);
    }

    // 5b. Create the IdP session row.
    let acr_static = acr_for(&pending.provider);
    let session = match sessions::create(
        db.as_ref(),
        &sessions::CreateSession {
            user_id: u.id,
            auth_method: &pending.provider,
            // The user provided BOTH a federation assertion (oauth) AND a
            // local password to confirm the link. Both factors land in amr.
            amr: vec!["oauth".into(), "pwd".into()],
            acr: Some(acr_static),
            idle_minutes: session_cookie::IDLE_MINUTES,
            absolute_hours: session_cookie::ABSOLUTE_HOURS,
        },
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "sessions::create failed");
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
    };

    // 5c. Bump last_login_at (non-fatal).
    if let Err(e) = users::touch_last_login(db.as_ref(), u.id).await {
        tracing::warn!(error = %e, user_id = %u.id, "touch_last_login failed");
    }

    // 5d. Accept the hydra login challenge — the SAME challenge the
    // original `/oauth/<provider>/start` stashed. Hydra has been pending
    // all this time.
    let accept = AcceptLoginRequest {
        subject: u.id.to_string(),
        remember: Some(true),
        remember_for: Some(3600),
        acr: Some(acr_static.into()),
        amr: Some(vec!["oauth".into(), "pwd".into()]),
        ..Default::default()
    };
    let redirect_to = match admin.accept_login(&pending.login_challenge, &accept).await {
        Ok(r) => r.redirect_to,
        Err(e) => {
            tracing::error!(error = %e, "accept_login failed");
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
    };

    // 5e. Audit + redirect.
    audit::emit(
        db.as_ref(),
        &AuditEvent {
            event_type: "oauth_link_success",
            outcome: "success",
            user_id: Some(&u.id),
            auth_method: Some(&pending.provider),
            detail: json!({
                "subject": pending.subject,
                "email": pending.email,
            }),
            ..Default::default()
        },
    )
    .await;

    let mut resp = HttpResponse::Found();
    resp.header(
        LOCATION,
        HeaderValue::from_str(&redirect_to)
            .unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    resp.header(
        SET_COOKIE,
        session_cookie::set_cookie(&session.id, cfg.insecure_dev),
    );
    resp.finish()
}

/// Re-render the `/link` page with an error banner + fresh CSRF cookie.
/// Same shape as the success-path GET so timing/content don't leak which
/// arm rejected the request.
fn render_link_error(
    token: &str,
    pending: &PendingLink,
    cfg: &AuthConfig,
    err: &str,
) -> HttpResponse {
    render_link_error_with_status(
        token,
        pending,
        cfg,
        err,
        ntex::http::StatusCode::UNAUTHORIZED,
    )
}

fn render_link_error_with_status(
    token: &str,
    pending: &PendingLink,
    cfg: &AuthConfig,
    err: &str,
    status: ntex::http::StatusCode,
) -> HttpResponse {
    let csrf_token = csrf::generate_token();
    let page = LinkPage {
        token,
        csrf: &csrf_token,
        existing_email: &pending.email,
        provider: &pending.provider,
        error: Some(err),
    };
    let body = page
        .render()
        .unwrap_or_else(|_| format!("<h1>{err}</h1>"));
    let mut resp = HttpResponse::build(status);
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token, cfg.insecure_dev));
    resp.body(body)
}

fn render_error_page(message: PublicErrorMessage) -> HttpResponse {
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
