//! Magic-link UI handlers.
//!
//! Routes (registered in `server::configure`):
//!
//! - **POST `/magic/start`** — accept an email + `login_challenge`, issue a
//!   magic-link token, send the email, set `__Host-zsidp_magic_csrf` at
//!   the requesting device, render the "check your email" page (with a
//!   hidden code-entry form for cross-device completion). Always returns
//!   200, regardless of whether the address exists or is rate-limited
//!   (enumeration defense).
//!
//! - **GET `/magic/await`** — alternate landing for the cross-device
//!   code-entry form. The check-email page already embeds the same form
//!   inside a `<details>` toggle; this route exists for direct entry.
//!
//! - **GET `/magic/verify?token=<token>&login_challenge=<…>`** — render a
//!   POST interstitial so the token leaves the URL before it is redeemed.
//!
//! - **POST `/magic/verify/redeem`** — redeem the magic-link token. If the
//!   redeeming browser presents the matching `__Host-zsidp_magic_csrf`
//!   cookie (same-device path) → mint session, `accept_login`, 302 to hydra.
//!   If the cookie is missing or different (cross-device) → generate a
//!   6-digit code, persist it under the magic-link's CSRF nonce, render the
//!   code on the redeeming device.
//!
//! - **POST `/magic/complete`** — the cross-device completion form. The
//!   requesting device posts the 6-digit code it saw on the redeeming
//!   device; we atomically consume `zeroship.magic_completions`, look up the
//!   user, mint a session, `accept_login`, redirect.
//!
//! User creation: a successful redeem on an unknown email creates a new
//! `zeroship.users` row with `email_verified_at = NOW()` — clicking the link
//! is itself proof of email ownership. Reuses `find_or_create_magic_user`
//! for both same-device and cross-device.

use askama::Template;
use ntex::http::header::{HeaderValue, COOKIE, LOCATION, SET_COOKIE, USER_AGENT};
use ntex::http::StatusCode;
use ntex::web::{HttpRequest, HttpResponse};
use rand::Rng;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use url::form_urlencoded;
use uuid::Uuid;

use crate::audit::{self, AuditEvent};
use crate::config::AuthConfig;
use crate::csrf;
use crate::error::{AuthError, Result};
use crate::hydra_client::types::AcceptLoginRequest;
use crate::hydra_client::HydraAdmin;
use crate::identity::email as email_validation;
use crate::identity::eligibility;
use crate::identity::magic_link;
use crate::ratelimit::{self, Bucket, RateLimitDecision};
use crate::sessions::login as session_cookie;
use crate::store::{sessions, users};
use crate::ui::{
    render_token_interstitial, ErrorPage, MagicAwaitCodePage, MagicCheckEmailPage,
    MagicShowCodePage, PublicErrorMessage, TokenRedeemInterstitial,
};
use zeroship_mailer::templates::{build_email, MagicLinkHtml, MagicLinkText};
use zeroship_mailer::{Address, Mailer};

// ─── Cookie helpers ──────────────────────────────────────────────────

/// Production cookie name (`__Host-` → Secure required) for the
/// per-device magic-link CSRF nonce.
pub const MAGIC_CSRF_COOKIE_PROD: &str = "__Host-zsidp_magic_csrf";
/// Dev cookie name (no `__Host-` prefix). RFC 6265bis §4.1.3.2 — the
/// `__Host-` prefix mandates Secure; dev runs over plain HTTP.
pub const MAGIC_CSRF_COOKIE_DEV: &str = "zsidp_magic_csrf";

/// Resolve the magic-link CSRF cookie name for the current environment.
pub(crate) fn magic_csrf_cookie_name(insecure_dev: bool) -> &'static str {
    if insecure_dev { MAGIC_CSRF_COOKIE_DEV } else { MAGIC_CSRF_COOKIE_PROD }
}

/// Build the `Set-Cookie` header value for the magic-link CSRF cookie.
///
/// 15-minute Max-Age matches the magic-link token's TTL. `SameSite=Lax`
/// so the email-link click (which is a cross-site GET back to
/// `auth.zeroship.ai`) still presents the cookie. `HttpOnly` because no
/// JS needs to read it — only the server consults it on
/// `/magic/verify`. `Path=/` because the cookie must be present on the
/// `/magic/verify` and `/magic/complete` paths alike.
pub(crate) fn magic_csrf_set_cookie(nonce: &str, insecure_dev: bool) -> String {
    let name = magic_csrf_cookie_name(insecure_dev);
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!(
        "{name}={nonce}; Path=/; HttpOnly; SameSite=Lax{secure}; Max-Age=900"
    )
}

/// Build the `Set-Cookie` header value for clearing the magic-link
/// CSRF cookie. Used after a successful redeem so the nonce can't be
/// reused.
fn magic_csrf_clear_cookie(insecure_dev: bool) -> String {
    let name = magic_csrf_cookie_name(insecure_dev);
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!("{name}=; Path=/; HttpOnly; SameSite=Lax{secure}; Max-Age=0")
}

/// Parse the magic-link CSRF nonce from a request's `Cookie` header
/// value.
///
/// Used by `/magic/verify` to decide same-device vs cross-device on
/// redeem.
pub(crate) fn parse_magic_csrf_cookie(cookie_header: &str, insecure_dev: bool) -> Option<String> {
    let name = magic_csrf_cookie_name(insecure_dev);
    let prefix = format!("{name}=");
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(&prefix) {
            return Some(rest.to_string());
        }
    }
    None
}

fn safe_csrf_value(value: &str) -> bool {
    (16..=128).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

// ─── POST /magic/start ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct MagicStartForm {
    pub csrf: String,
    pub email: String,
    pub login_challenge: String,
}

/// Issue a magic-link token + send the email + render "check your email".
///
/// Enumeration-resistant: rate-limit failures and missing-user paths
/// still return the same 200 + page so an attacker can't probe which
/// addresses have accounts.
///
/// CSRF: form `csrf` value must match the `__Host-zsidp_csrf` cookie
/// (the existing double-submit cookie shared with `/login`/`/signup`).
#[allow(clippy::future_not_send, clippy::too_many_lines)]
pub async fn start(
    req: HttpRequest,
    form: ntex::web::types::Form<MagicStartForm>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
    mailer: ntex::web::types::State<Arc<dyn Mailer>>,
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

    let email_norm = form.email.trim().to_ascii_lowercase();
    if email_validation::validate_email(&email_norm).is_err() {
        return render_error_page_with_status(
            PublicErrorMessage::InvalidRequest,
            StatusCode::BAD_REQUEST,
        );
    }
    let login_challenge = form.login_challenge.clone();

    // 2. Rate-limit per-email + per-IP. On throttle, render the
    //    check-email page anyway so the attacker can't distinguish.
    //    Trusted, gateway-authored client IP (SEC-3) — not the spoofable
    //    leftmost X-Forwarded-For token.
    let ip = crate::headers::client_ip(&req);
    let buckets = [
        (format!("magic:email:{email_norm}"), Bucket::LOGIN_EMAIL),
        (format!("magic:ip:{ip}"), Bucket::LOGIN_IP),
    ];
    let mut throttled = false;
    for (key, bucket) in &buckets {
        match ratelimit::consume(db.as_ref(), key, *bucket).await {
            Ok(RateLimitDecision::Allowed) => {}
            Ok(RateLimitDecision::Throttled(_)) => {
                throttled = true;
                audit::emit(
                    db.as_ref(),
                    &AuditEvent {
                        event_type: "magic_issued",
                        outcome: "failure",
                        auth_method: Some("magic"),
                        detail: json!({ "reason": "rate_limited", "bucket": key }),
                        ..AuditEvent::from_request(&req)
                    },
                )
                .await;
                break;
            }
            Err(e) => {
                tracing::error!(error = %e, bucket = %key, "magic rate-limit consume failed");
                return render_error_page(PublicErrorMessage::ContactSupport);
            }
        }
    }

    // 3. Issue the token (skipped on throttle — but still render the
    //    same page so the attacker can't tell).
    let issued = if throttled {
        None
    } else {
        match magic_link::issue(db.as_ref(), &email_norm, "login").await {
            Ok(i) => Some(i),
            Err(e) => {
                tracing::error!(error = %e, "magic_link::issue failed");
                return render_error_page(PublicErrorMessage::ContactSupport);
            }
        }
    };

    // 4. Send the email. Errors are logged but never surfaced (enumeration
    //    defense — a suppressed/bouncing address must look identical to a
    //    deliverable one).
    if let Some(ref issued) = issued {
        let lc_escaped: String =
            form_urlencoded::byte_serialize(login_challenge.as_bytes()).collect();
        let link = format!(
            "{}/magic/verify?token={}&login_challenge={}",
            cfg.public_url(),
            issued.raw,
            lc_escaped,
        );
        let name_hint = email_norm.split('@').next().unwrap_or("there").to_string();
        let device_str = user_agent_str(req.headers().get(USER_AGENT));

        let html = MagicLinkHtml {
            name: &name_hint,
            link: &link,
            expires_in: "15 minutes",
            requesting_device: &device_str,
            requesting_location: "Unknown",
        }
        .render()
        .unwrap_or_default();
        let text = MagicLinkText {
            name: &name_hint,
            link: &link,
            expires_in: "15 minutes",
            requesting_device: &device_str,
            requesting_location: "Unknown",
        }
        .render()
        .unwrap_or_default();

        let email_msg = build_email(
            Address {
                email: email_norm.clone(),
                name: None,
            },
            Address {
                email: cfg.mail_from_email.clone(),
                name: Some(cfg.mail_from_name.clone()),
            },
            "Sign in to zeroship".into(),
            text,
            html,
            vec!["magic-link".into(), "login".into()],
        );
        if let Err(e) = mailer.send(db.as_ref(), email_msg).await {
            tracing::warn!(
                error = %e,
                email_domain = %email_domain(&email_norm),
                "magic_link email send failed"
            );
        }

        audit::emit(
            db.as_ref(),
            &AuditEvent {
                event_type: "magic_issued",
                outcome: "success",
                auth_method: Some("magic"),
                detail: json!({
                    "email_domain": email_norm.split('@').nth(1).unwrap_or("")
                }),
                ..AuditEvent::from_request(&req)
            },
        )
        .await;
    }

    // 5. Render the check-email page. Fresh CSRF cookie for the
    //    cross-device `/magic/complete` POST.
    let new_csrf = csrf::generate_token();
    // `csrf_nonce` rendered into the hidden form field MUST be the
    // magic-link's nonce, because `/magic/complete` will look the row up
    // by it (U4.3). When throttled we use a random placeholder — the
    // form will not match anything, which is the intended behaviour (we
    // silently dropped the request).
    let nonce = issued
        .as_ref()
        .map_or_else(|| "throttled-no-token".into(), |i| i.csrf_nonce.clone());

    let page = MagicCheckEmailPage {
        csrf: &new_csrf,
        login_challenge: &login_challenge,
        csrf_nonce: &nonce,
        email: &email_norm,
    };
    let body = page
        .render()
        .unwrap_or_else(|_| "<h1>Check your email</h1>".to_string());

    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&new_csrf, cfg.insecure_dev));
    // The requesting-device cookie: only set if we actually issued.
    if let Some(ref i) = issued {
        resp.header(
            SET_COOKIE,
            magic_csrf_set_cookie(&i.csrf_nonce, cfg.insecure_dev),
        );
    }
    resp.body(body)
}

// ─── GET /magic/await ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct MagicAwaitQuery {
    pub login_challenge: String,
    pub csrf_nonce: String,
    pub email: String,
}

/// `/magic/await` GET — surfaces just the code-entry form.
///
/// (The "I opened the link on another device" variant of the
/// check-email page.) The check-email page already embeds the same
/// form inside a `<details>`; this route is for explicit deep-link
/// entry.
#[allow(clippy::future_not_send, clippy::unused_async)]
pub async fn await_code(
    query: ntex::web::types::Query<MagicAwaitQuery>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
) -> HttpResponse {
    let new_csrf = csrf::generate_token();
    let page = MagicAwaitCodePage {
        csrf: &new_csrf,
        login_challenge: &query.login_challenge,
        csrf_nonce: &query.csrf_nonce,
        email: &query.email,
    };
    let body = page
        .render()
        .unwrap_or_else(|_| "<h1>Enter your code</h1>".to_string());
    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&new_csrf, cfg.insecure_dev));
    resp.body(body)
}

// ─── GET /magic/verify ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct MagicVerifyQuery {
    pub token: String,
    pub login_challenge: String,
}

#[derive(Debug, Deserialize)]
pub struct MagicRedeemForm {
    pub csrf: Option<String>,
    pub token: String,
    pub login_challenge: String,
}

/// `/magic/verify` — the link the user clicks in the email. Renders a
/// one-shot interstitial that immediately POSTs to `/magic/verify/redeem`.
#[allow(clippy::future_not_send)]
pub async fn verify(
    req: HttpRequest,
    query: ntex::web::types::Query<MagicVerifyQuery>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
) -> HttpResponse {
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let csrf_token = parse_magic_csrf_cookie(cookie_header, cfg.insecure_dev)
        .filter(|value| safe_csrf_value(value))
        .unwrap_or_else(csrf::generate_token);
    let page = TokenRedeemInterstitial {
        title: "Sign in",
        action: "/magic/verify/redeem",
        token: &query.token,
        csrf: &csrf_token,
        // Overwritten with an independent per-response nonce inside
        // `render_token_interstitial`; callers must not set it.
        script_nonce: "",
        extra_fields: vec![("login_challenge", query.login_challenge.as_str())],
    };
    let csrf_set_cookie = magic_csrf_set_cookie(&csrf_token, cfg.insecure_dev);
    render_token_interstitial(&page, &csrf_set_cookie)
}

/// `/magic/verify/redeem` — atomically redeem the token, then branch on
/// whether the redeeming browser
/// presents the matching `__Host-zsidp_magic_csrf` cookie:
///
/// - **Same-device**: mint a session, `accept_login`, 302 to hydra.
/// - **Cross-device**: generate a 6-digit code, persist it in
///   `zeroship.magic_completions`, render the code on this device for the
///   user to type back on the requesting device.
#[allow(clippy::future_not_send)]
pub async fn verify_redeem(
    req: HttpRequest,
    form: ntex::web::types::Form<MagicRedeemForm>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
    admin: ntex::web::types::State<HydraAdmin>,
) -> HttpResponse {
    if !valid_magic_csrf(&req, form.csrf.as_deref(), &cfg) {
        return render_error_page_with_status(
            PublicErrorMessage::InvalidRequest,
            StatusCode::FORBIDDEN,
        );
    }

    // 1. Redeem.
    let redeemed = match magic_link::redeem_pending(db.as_ref(), &form.token).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "magic_redeem",
                    outcome: "failure",
                    auth_method: Some("magic"),
                    detail: json!({ "reason": "invalid_or_expired" }),
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
            return render_error_page(PublicErrorMessage::SessionExpired);
        }
        Err(magic_link::RedeemError::InFlight) => {
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "magic_redeem",
                    outcome: "failure",
                    auth_method: Some("magic"),
                    detail: json!({ "reason": "consume_in_flight" }),
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
            return render_error_page(PublicErrorMessage::PleaseTryAgain);
        }
        Err(magic_link::RedeemError::AlreadyConsumed) => {
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "magic_redeem",
                    outcome: "failure",
                    auth_method: Some("magic"),
                    detail: json!({ "reason": "already_consumed" }),
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
            return render_error_page(PublicErrorMessage::SessionExpired);
        }
        Err(magic_link::RedeemError::Store(e)) => {
            tracing::error!(error = %e, "magic_link::redeem_pending failed");
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
    };
    if redeemed.purpose != magic_link::LOGIN_PURPOSE {
        tracing::error!(
            purpose = %redeemed.purpose,
            "magic_link::redeem_pending returned non-login purpose"
        );
        if let Err(e) = magic_link::clear_consume_pending(db.as_ref(), &redeemed.token_hash, None).await {
            tracing::warn!(error = %e, "magic_link clear pending after unexpected purpose failed");
        }
        audit::emit(
            db.as_ref(),
            &AuditEvent {
                event_type: "magic_redeem",
                outcome: "failure",
                auth_method: Some("magic"),
                detail: json!({ "reason": "unexpected_purpose" }),
                ..AuditEvent::from_request(&req)
            },
        )
        .await;
        return render_error_page(PublicErrorMessage::SessionExpired);
    }

    // 2. Same-device predicate.
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let cookie_nonce = parse_magic_csrf_cookie(cookie_header, cfg.insecure_dev);
    let same_device = cookie_nonce.as_deref() == Some(redeemed.csrf_nonce.as_str());

    // 3. Find-or-create the user.
    let user_id = match find_or_create_magic_user(db.as_ref(), &redeemed.email).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, "magic_link find-or-create failed");
            if let Err(e) = magic_link::clear_consume_pending(db.as_ref(), &redeemed.token_hash, None).await {
                tracing::warn!(error = %e, "magic_link clear pending after user failure failed");
            }
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
    };

    // F2 lockout recovery: the magic link was just cryptographically redeemed
    // (strong owner-present evidence), so clear any soft password-guessing
    // lockout BEFORE the eligibility gate — otherwise a victim locked by
    // password-guessing could never recover via magic-link. Best-effort; the
    // gate below still enforces hard `disabled_at`. Must precede the gate so the
    // cleared `locked_until` is what the gate reads.
    if let Err(e) = users::reset_login_failures(db.as_ref(), user_id).await {
        tracing::warn!(error = %e, user_id = %user_id, "magic clear lockout failed");
    }
    if let Err(e) = eligibility::check_user_eligible(db.as_ref(), user_id).await {
        if let Err(clear_err) = magic_link::clear_consume_pending(db.as_ref(), &redeemed.token_hash, None).await {
            tracing::warn!(error = %clear_err, "magic_link clear pending after eligibility failure failed");
        }
        if !e.is_account_state() {
            tracing::error!(error = %e, user_id = %user_id, "magic verify eligibility check failed");
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
        audit::emit(
            db.as_ref(),
            &AuditEvent {
                event_type: "magic_redeem",
                outcome: "failure",
                user_id: Some(&user_id),
                auth_method: Some("magic"),
                detail: json!({ "reason": "account_ineligible" }),
                ..AuditEvent::from_request(&req)
            },
        )
        .await;
        return render_error_page(PublicErrorMessage::AccountTemporarilyLocked);
    }

    if same_device {
        same_device_finish(
            db.as_ref(),
            &admin,
            &cfg,
            &redeemed.token_hash,
            &redeemed.reserved_at,
            user_id,
            &form.login_challenge,
            &req,
        )
        .await
    } else {
        cross_device_show_code(
            db.as_ref(),
            &redeemed.token_hash,
            &redeemed.reserved_at,
            user_id,
            &redeemed.email,
            &redeemed.csrf_nonce,
            &form.login_challenge,
            &req,
        )
        .await
    }
}

fn valid_magic_csrf(req: &HttpRequest, form_csrf: Option<&str>, cfg: &AuthConfig) -> bool {
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let cookie_token = parse_magic_csrf_cookie(cookie_header, cfg.insecure_dev);
    cookie_token
        .as_deref()
        .zip(form_csrf)
        .is_some_and(|(cookie, form)| csrf::matches(form, cookie))
}

/// Same-device path: mint session, `accept_login`, 302 to hydra.
#[allow(clippy::future_not_send)]
async fn same_device_finish(
    db: &compio_postgres::Client,
    admin: &HydraAdmin,
    cfg: &AuthConfig,
    token_hash: &[u8],
    reserved_at: &chrono::DateTime<chrono::Utc>,
    user_id: Uuid,
    login_challenge: &str,
    req: &HttpRequest,
) -> HttpResponse {
    let session = match sessions::create(
        db,
        &sessions::CreateSession {
            user_id,
            auth_method: "magic",
            amr: vec!["magic".into()],
            acr: Some("urn:zeroship:magic"),
            expected_credential_version: None,
            idle_minutes: session_cookie::IDLE_MINUTES,
            absolute_hours: session_cookie::ABSOLUTE_HOURS,
        },
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "magic sessions::create failed");
            if let Err(e) =
                magic_link::clear_consume_pending(db, token_hash, Some(reserved_at)).await
            {
                tracing::warn!(error = %e, "magic_link clear pending after session failure failed");
            }
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
    };

    if let Err(e) = users::touch_last_login(db, user_id).await {
        tracing::warn!(error = %e, user_id = %user_id, "magic touch_last_login failed");
    }

    let accept = AcceptLoginRequest {
        subject: user_id.to_string(),
        remember: Some(true),
        remember_for: Some(3600),
        acr: Some("urn:zeroship:magic".into()),
        amr: Some(vec!["magic".into()]),
        ..Default::default()
    };
    let redirect_to = match admin.accept_login(login_challenge, &accept).await {
        Ok(r) => r.redirect_to,
        Err(e) => {
            tracing::error!(error = %e, "magic accept_login failed");
            if let Err(e) =
                magic_link::clear_consume_pending(db, token_hash, Some(reserved_at)).await
            {
                tracing::warn!(error = %e, "magic_link clear pending after accept_login failure failed");
            }
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
    };

    match magic_link::finalize_consume(db, token_hash, reserved_at).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::error!("magic_link finalize consume updated no rows");
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
        Err(e) => {
            tracing::error!(error = %e, "magic_link finalize consume failed");
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
    }

    audit::emit(
        db,
        &AuditEvent {
            event_type: "magic_redeemed_same_device",
            outcome: "success",
            user_id: Some(&user_id),
            auth_method: Some("magic"),
            ..AuditEvent::from_request(&req)
        },
    )
    .await;

    let mut resp = HttpResponse::Found();
    resp.header(
        LOCATION,
        HeaderValue::from_str(&redirect_to).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    resp.header(
        SET_COOKIE,
        session_cookie::set_cookie(&session.id, cfg.insecure_dev),
    );
    // Clear the requesting-device cookie so a future stray click can't
    // be replayed in a same-device check.
    resp.header(SET_COOKIE, magic_csrf_clear_cookie(cfg.insecure_dev));
    resp.finish()
}

/// Cross-device path: stash a fresh 6-digit code in
/// `zeroship.magic_completions`, render the code on this (redeeming) device.
#[allow(clippy::future_not_send)]
async fn cross_device_show_code(
    db: &compio_postgres::Client,
    token_hash: &[u8],
    reserved_at: &chrono::DateTime<chrono::Utc>,
    user_id: Uuid,
    email: &str,
    csrf_nonce: &str,
    login_challenge: &str,
    req: &HttpRequest,
) -> HttpResponse {
    let code = format!("{:06}", rand::thread_rng().gen_range(0..1_000_000u32));
    // 5-minute window — short, since the user is actively typing.
    if let Err(e) =
        completions_store::create(db, csrf_nonce, &code, email, login_challenge, 300).await
    {
        tracing::error!(error = %e, "magic_completions insert failed");
        if let Err(e) = magic_link::clear_consume_pending(db, token_hash, Some(reserved_at)).await {
            tracing::warn!(error = %e, "magic_link clear pending after completion insert failure failed");
        }
        return render_error_page(PublicErrorMessage::ContactSupport);
    }

    match magic_link::finalize_consume(db, token_hash, reserved_at).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::error!("magic_link finalize consume after cross-device code updated no rows");
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
        Err(e) => {
            tracing::error!(error = %e, "magic_link finalize consume after cross-device code failed");
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
    }

    audit::emit(
        db,
        &AuditEvent {
            event_type: "magic_redeemed_cross_device",
            outcome: "success",
            user_id: Some(&user_id),
            auth_method: Some("magic"),
            ..AuditEvent::from_request(&req)
        },
    )
    .await;

    let page = MagicShowCodePage { code: &code, email };
    let body = page
        .render()
        .unwrap_or_else(|_| format!("<h1>Code: {code}</h1>"));
    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.body(body)
}

// ─── POST /magic/complete ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct MagicCompleteForm {
    pub csrf: String,
    pub csrf_nonce: String,
    pub login_challenge: String,
    pub code: String,
}

/// `/magic/complete` POST — cross-device completion. The requesting
/// device posts the 6-digit code shown on the redeeming device; we
/// atomically reserve the row, mint a session, `accept_login`, then
/// finalize the row consume.
#[allow(clippy::future_not_send, clippy::too_many_lines)]
pub async fn complete(
    req: HttpRequest,
    form: ntex::web::types::Form<MagicCompleteForm>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
    admin: ntex::web::types::State<HydraAdmin>,
) -> HttpResponse {
    // 1. CSRF.
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

    // 2. Per-IP throttle before touching the completion row. This limits
    //    online guessing even across many CSRF nonces from one source.
    //    Trusted, gateway-authored client IP (SEC-3).
    let ip = crate::headers::client_ip(&req);
    let rate_key = format!("magic_complete:{ip}");
    match ratelimit::consume_or_throttle(db.as_ref(), &rate_key, Bucket::MAGIC_COMPLETE).await {
        Ok(RateLimitDecision::Allowed) => {}
        Ok(RateLimitDecision::Throttled(_)) => {
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "magic_complete",
                    outcome: "failure",
                    auth_method: Some("magic"),
                    detail: json!({ "reason": "rate_limited", "bucket": rate_key }),
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
            return render_error_page_with_status(
                PublicErrorMessage::PleaseTryAgain,
                StatusCode::TOO_MANY_REQUESTS,
            );
        }
        Err(e) => {
            tracing::error!(error = %e, bucket = %rate_key, "magic complete rate-limit consume failed");
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
    }

    // 3. Reserve the completion row atomically. It is finalized only
    //    after hydra accepts the login.
    let completion =
        match completions_store::consume_pending(db.as_ref(), &form.csrf_nonce, &form.code).await {
            Ok(c) => c,
            Err(completions_store::ConsumeError::WrongCode) => {
                audit::emit(
                    db.as_ref(),
                    &AuditEvent {
                        event_type: "magic_complete",
                        outcome: "failure",
                        auth_method: Some("magic"),
                        detail: json!({ "reason": "code_invalid_or_expired" }),
                        ..AuditEvent::from_request(&req)
                    },
                )
                .await;
                return render_error_page(PublicErrorMessage::SessionExpired);
            }
            Err(completions_store::ConsumeError::Store(e)) => {
                tracing::error!(error = %e, "magic_completions consume failed");
                return render_error_page(PublicErrorMessage::ContactSupport);
            }
            Err(completions_store::ConsumeError::InFlight) => {
                audit::emit(
                    db.as_ref(),
                    &AuditEvent {
                        event_type: "magic_complete",
                        outcome: "failure",
                        auth_method: Some("magic"),
                        detail: json!({ "reason": "consume_in_flight" }),
                        ..AuditEvent::from_request(&req)
                    },
                )
                .await;
                return render_error_page(PublicErrorMessage::PleaseTryAgain);
            }
        };

    // 4. Defence: form `login_challenge` must match the one stashed at
    //    issue time. Defeats a forged challenge swap on the requesting
    //    device.
    if form.login_challenge != completion.login_challenge {
        audit::emit(
            db.as_ref(),
            &AuditEvent {
                event_type: "magic_complete",
                outcome: "failure",
                auth_method: Some("magic"),
                detail: json!({ "reason": "login_challenge_mismatch" }),
                ..AuditEvent::from_request(&req)
            },
        )
        .await;
        if let Err(e) = completions_store::clear_consume_pending(
            db.as_ref(),
            &form.csrf_nonce,
            Some(&completion.reserved_at),
        )
        .await
        {
            tracing::warn!(error = %e, "magic_completions clear pending after challenge mismatch failed");
        }
        return render_error_page(PublicErrorMessage::SessionExpired);
    }

    // 5. Find-or-create the user (must succeed — the redeem path
    //    already found-or-created, so this is effectively a lookup).
    let user_id = match find_or_create_magic_user(db.as_ref(), &completion.email).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, "magic complete find-or-create failed");
            if let Err(e) = completions_store::clear_consume_pending(
                db.as_ref(),
                &form.csrf_nonce,
                Some(&completion.reserved_at),
            )
            .await
            {
                tracing::warn!(error = %e, "magic_completions clear pending after user failure failed");
            }
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
    };

    // F2 lockout recovery (see the same-device path): the magic completion is
    // strong owner-present evidence, so clear any soft password-guessing lockout
    // before the eligibility gate. Best-effort; the gate still enforces hard
    // `disabled_at`.
    if let Err(e) = users::reset_login_failures(db.as_ref(), user_id).await {
        tracing::warn!(error = %e, user_id = %user_id, "magic complete clear lockout failed");
    }
    if let Err(e) = eligibility::check_user_eligible(db.as_ref(), user_id).await {
        if let Err(clear_err) =
            completions_store::clear_consume_pending(db.as_ref(), &form.csrf_nonce, None).await
        {
            tracing::warn!(error = %clear_err, "magic_completions clear pending after eligibility failure failed");
        }
        if !e.is_account_state() {
            tracing::error!(error = %e, user_id = %user_id, "magic complete eligibility check failed");
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
        audit::emit(
            db.as_ref(),
            &AuditEvent {
                event_type: "magic_complete",
                outcome: "failure",
                user_id: Some(&user_id),
                auth_method: Some("magic"),
                detail: json!({ "reason": "account_ineligible" }),
                ..AuditEvent::from_request(&req)
            },
        )
        .await;
        return render_error_page(PublicErrorMessage::AccountTemporarilyLocked);
    }

    // 6. Mint session + accept_login + 302 — mirrors same-device path.
    let session = match sessions::create(
        db.as_ref(),
        &sessions::CreateSession {
            user_id,
            auth_method: "magic",
            amr: vec!["magic".into()],
            acr: Some("urn:zeroship:magic"),
            expected_credential_version: None,
            idle_minutes: session_cookie::IDLE_MINUTES,
            absolute_hours: session_cookie::ABSOLUTE_HOURS,
        },
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "magic complete sessions::create failed");
            if let Err(e) = completions_store::clear_consume_pending(
                db.as_ref(),
                &form.csrf_nonce,
                Some(&completion.reserved_at),
            )
            .await
            {
                tracing::warn!(error = %e, "magic_completions clear pending after session failure failed");
            }
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
    };

    if let Err(e) = users::touch_last_login(db.as_ref(), user_id).await {
        tracing::warn!(error = %e, user_id = %user_id, "magic touch_last_login failed");
    }

    let accept = AcceptLoginRequest {
        subject: user_id.to_string(),
        remember: Some(true),
        remember_for: Some(3600),
        acr: Some("urn:zeroship:magic".into()),
        amr: Some(vec!["magic".into()]),
        ..Default::default()
    };
    let redirect_to = match admin.accept_login(&form.login_challenge, &accept).await {
        Ok(r) => r.redirect_to,
        Err(e) => {
            tracing::error!(error = %e, "magic complete accept_login failed");
            if let Err(e) = completions_store::clear_consume_pending(
                db.as_ref(),
                &form.csrf_nonce,
                Some(&completion.reserved_at),
            )
            .await
            {
                tracing::warn!(error = %e, "magic_completions clear pending after accept_login failure failed");
            }
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
    };

    match completions_store::finalize_consume(
        db.as_ref(),
        &form.csrf_nonce,
        &completion.reserved_at,
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => {
            tracing::error!("magic_completions finalize consume updated no rows");
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
        Err(e) => {
            tracing::error!(error = %e, "magic_completions finalize consume failed");
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
    }

    audit::emit(
        db.as_ref(),
        &AuditEvent {
            event_type: "magic_complete",
            outcome: "success",
            user_id: Some(&user_id),
            auth_method: Some("magic"),
            ..AuditEvent::from_request(&req)
        },
    )
    .await;

    let mut resp = HttpResponse::Found();
    resp.header(
        LOCATION,
        HeaderValue::from_str(&redirect_to).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    resp.header(
        SET_COOKIE,
        session_cookie::set_cookie(&session.id, cfg.insecure_dev),
    );
    resp.header(SET_COOKIE, magic_csrf_clear_cookie(cfg.insecure_dev));
    resp.finish()
}

// ─── Helpers ─────────────────────────────────────────────────────────

fn render_error_page(message: PublicErrorMessage) -> HttpResponse {
    render_error_page_with_status(message, StatusCode::OK)
}

fn render_error_page_with_status(
    message: PublicErrorMessage,
    status: StatusCode,
) -> HttpResponse {
    let page = ErrorPage {
        message,
        error_code: message.error_code(),
    };
    let body = page
        .render()
        .unwrap_or_else(|_| format!("<h1>{}</h1>", message.as_str()));
    let mut resp = HttpResponse::build(status);
    resp.content_type("text/html; charset=utf-8");
    resp.body(body)
}

/// Find a user by `email`, creating them with `email_verified_at = NOW()`
/// if absent (clicking the magic link is itself proof of email
/// ownership).
async fn find_or_create_magic_user(db: &compio_postgres::Client, email: &str) -> Result<Uuid> {
    email_validation::validate_email(email)
        .map_err(|_| AuthError::Internal("invalid email".into()))?;

    if let Some(user) = users::find_by_email(db, email).await? {
        if user.email_verified_at.is_none() {
            // Magic-link click counts as email verification — make
            // sure the row reflects that (no-op if already verified).
            db.execute(
                "UPDATE zeroship.users SET email_verified_at = NOW() \
                 WHERE id = $1 AND email_verified_at IS NULL",
                &[&user.id],
            )
            .await
            .map_err(|e| AuthError::Db(format!("set email_verified_at: {e}")))?;
        }
        return Ok(user.id);
    }
    let name = email.split('@').next().unwrap_or("user");
    let user = users::create(db, email, name, None).await?;
    db.execute(
        "UPDATE zeroship.users SET email_verified_at = NOW() WHERE id = $1",
        &[&user.id],
    )
    .await
    .map_err(|e| AuthError::Db(format!("set email_verified_at: {e}")))?;
    Ok(user.id)
}

// ─── zeroship.magic_completions store ────────────────────────────────────

pub mod completions_store {
    use compio_postgres::Client;

    use crate::error::AuthError;

    #[derive(Debug, Clone)]
    pub struct Completion {
        pub email: String,
        pub login_challenge: String,
        pub reserved_at: chrono::DateTime<chrono::Utc>,
    }

    #[derive(Debug)]
    pub enum ConsumeError {
        InFlight,
        WrongCode,
        Store(AuthError),
    }

    /// Insert a fresh completion row. `expires_secs` is the lifetime
    /// from now until the code expires (5 minutes at the caller).
    ///
    /// `csrf_nonce` is the PRIMARY KEY — at most one outstanding
    /// completion per magic-link issue. The single-use redeem in
    /// `identity::magic_link::redeem_pending` makes a second insert impossible
    /// in practice; `ON CONFLICT … DO UPDATE` is defence-in-depth so
    /// re-running the test suite (which short-circuits the single-use
    /// invariant by tweaking the row directly) still works.
    pub async fn create(
        db: &Client,
        csrf_nonce: &str,
        code: &str,
        email: &str,
        login_challenge: &str,
        expires_secs: i64,
    ) -> crate::error::Result<()> {
        super::email_validation::validate_email(email)
            .map_err(|_| AuthError::Internal("invalid email".into()))?;

        db.execute(
            "INSERT INTO zeroship.magic_completions \
                (csrf_nonce, code, email, login_challenge, expires_at) \
             VALUES ($1, $2, $3::citext, $4, NOW() + ($5::text || ' seconds')::interval) \
             ON CONFLICT (csrf_nonce) DO UPDATE SET \
                code = EXCLUDED.code, \
                email = EXCLUDED.email, \
                login_challenge = EXCLUDED.login_challenge, \
                expires_at = EXCLUDED.expires_at, \
                attempts = 0, \
                consumed_pending_at = NULL, \
                consumed_at = NULL",
            &[
                &csrf_nonce,
                &code,
                &email,
                &login_challenge,
                &expires_secs.to_string(),
            ],
        )
        .await
        .map_err(|e| AuthError::Db(format!("magic_completions insert: {e}")))?;
        Ok(())
    }

    /// Atomically reserve a completion row, invalidating it after five
    /// failed code attempts. A second correct-code consume within 60
    /// seconds of an existing reservation reports `InFlight`; stale
    /// reservations can be retried.
    pub async fn consume_pending(
        db: &Client,
        csrf_nonce: &str,
        code: &str,
    ) -> std::result::Result<Completion, ConsumeError> {
        let rows = db
            .query(
                "UPDATE zeroship.magic_completions \
                 SET attempts = (attempts + 1)::SMALLINT, \
                     consumed_pending_at = NOW() \
                 WHERE csrf_nonce = $1 \
                   AND code = $2 \
                   AND attempts < 5 \
                   AND consumed_at IS NULL \
                   AND (consumed_pending_at IS NULL \
                        OR consumed_pending_at <= NOW() - INTERVAL '60 seconds') \
                   AND expires_at > NOW() \
                 RETURNING email::text, login_challenge, consumed_pending_at",
                &[&csrf_nonce, &code],
            )
            .await
            .map_err(|e| {
                ConsumeError::Store(AuthError::Db(format!("magic_completions consume: {e}")))
            })?;

        if let Some(row) = rows.first() {
            return Ok(Completion {
                email: row.get("email"),
                login_challenge: row.get("login_challenge"),
                reserved_at: row.get("consumed_pending_at"),
            });
        }

        let in_flight = db
            .query(
                "SELECT TRUE AS in_flight \
                 FROM zeroship.magic_completions \
                 WHERE csrf_nonce = $1 \
                   AND consumed_at IS NULL \
                   AND consumed_pending_at > NOW() - INTERVAL '60 seconds' \
                   AND expires_at > NOW()",
                &[&csrf_nonce],
            )
            .await
            .map_err(|e| {
                ConsumeError::Store(AuthError::Db(format!(
                    "magic_completions consume in-flight: {e}"
                )))
            })?;
        if in_flight.first().is_some() {
            return Err(ConsumeError::InFlight);
        }

        let wrong_rows = db
            .query(
                "UPDATE zeroship.magic_completions \
                 SET attempts = (attempts + 1)::SMALLINT, \
                     consumed_at = CASE \
                         WHEN attempts + 1 >= 5 THEN NOW() \
                         ELSE consumed_at \
                     END \
                 WHERE csrf_nonce = $1 \
                   AND code <> $2 \
                   AND attempts < 5 \
                   AND consumed_at IS NULL \
                   AND consumed_pending_at IS NULL \
                   AND expires_at > NOW() \
                 RETURNING attempts",
                &[&csrf_nonce, &code],
            )
            .await
            .map_err(|e| {
                ConsumeError::Store(AuthError::Db(format!(
                    "magic_completions wrong-code consume: {e}"
                )))
            })?;

        if wrong_rows.first().is_none() {
            return Err(ConsumeError::WrongCode);
        }

        Err(ConsumeError::WrongCode)
    }

    pub async fn finalize_consume(
        db: &Client,
        csrf_nonce: &str,
        reserved_at: &chrono::DateTime<chrono::Utc>,
    ) -> crate::error::Result<bool> {
        let updated = db
            .execute(
                "UPDATE zeroship.magic_completions \
                 SET consumed_at = NOW() \
                 WHERE csrf_nonce = $1 \
                   AND consumed_pending_at = $2 \
                   AND consumed_at IS NULL",
                &[&csrf_nonce, reserved_at],
            )
            .await
            .map_err(|e| AuthError::Db(format!("magic_completions finalize consume: {e}")))?;
        Ok(updated > 0)
    }

    pub async fn clear_consume_pending(
        db: &Client,
        csrf_nonce: &str,
        reserved_at: Option<&chrono::DateTime<chrono::Utc>>,
    ) -> crate::error::Result<bool> {
        let result = if let Some(ts) = reserved_at {
            db.execute(
                "UPDATE zeroship.magic_completions \
                 SET consumed_pending_at = NULL \
                 WHERE csrf_nonce = $1 \
                   AND consumed_pending_at = $2 \
                   AND consumed_at IS NULL",
                &[&csrf_nonce, ts],
            )
            .await
        } else {
            db.execute(
                "UPDATE zeroship.magic_completions \
                 SET consumed_pending_at = NULL \
                 WHERE csrf_nonce = $1 \
                   AND consumed_at IS NULL",
                &[&csrf_nonce],
            )
            .await
        };
        let updated = result
            .map_err(|e| AuthError::Db(format!("magic_completions clear consume pending: {e}")))?;
        Ok(updated > 0)
    }
}

// ─── Email-related helpers (start-only) ──────────────────────────────

/// Best-effort device label extracted from a `User-Agent` header. We
/// don't ship a full UA parser; the value is purely for the email body
/// ("Chrome on macOS just requested a sign-in link …"). Falls back to
/// "Unknown device" when the header is missing or malformed.
fn user_agent_str(h: Option<&HeaderValue>) -> String {
    h.and_then(|v| v.to_str().ok())
        .map(|s| {
            let trimmed: String = s
                .chars()
                .take(120)
                .filter(|ch| !ch.is_control())
                .collect();
            if trimmed.is_empty() {
                "Unknown device".to_string()
            } else {
                trimmed
            }
        })
        .unwrap_or_else(|| "Unknown device".to_string())
}

fn email_domain(email: &str) -> &str {
    email.split_once('@').map(|(_, domain)| domain).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_str_caps_long_values() {
        let raw = "A".repeat(121);
        let header = HeaderValue::from_str(&raw).expect("header value");
        let got = user_agent_str(Some(&header));
        assert_eq!(got.len(), 120);
        assert!(got.chars().all(|ch| ch == 'A'));
    }

    #[test]
    fn user_agent_str_uses_unknown_for_empty_values() {
        let header = HeaderValue::from_static("");
        assert_eq!(user_agent_str(Some(&header)), "Unknown device");
    }

    #[test]
    fn email_domain_omits_local_part() {
        assert_eq!(email_domain("victim@example.com"), "example.com");
        assert_eq!(email_domain("not-an-email"), "");
    }
}
