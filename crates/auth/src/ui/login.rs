//! `/login` GET handler.
//!
//! Algorithm (P2 §8.1 GET path):
//!
//! 1. Read `login_challenge` from the query.
//! 2. Fetch challenge metadata from hydra via `admin.get_login(challenge)`.
//! 3. If `info.skip == true`, hydra already has a session for this subject —
//!    immediately call `accept_login` and redirect back to hydra.
//! 4. Otherwise render the form with a fresh CSRF token cookie.
//!
//! Route wiring happens in P2-U6 (`server::configure`); the handler here is
//! a plain `pub async fn` with no `#[ntex::web::*]` attribute.

use askama::Template;
use ntex::http::header::{HeaderValue, COOKIE, LOCATION, SET_COOKIE};
use ntex::web::{HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use crate::audit::{self, AuditEvent};
use crate::config::AuthConfig;
use crate::csrf;
use crate::hydra_client::types::{AcceptLoginRequest, RejectRequest};
use crate::hydra_client::HydraAdmin;
use crate::identity::credentials::{verify_password_credentials, CredentialError};
use crate::identity::eligibility::{self, LoginIneligible};
use crate::identity::totp;
use crate::oidc::auth_request::AuthRequest;
use crate::ratelimit::{self, Bucket, RateLimitDecision};
use crate::return_to;
use crate::sessions::login as session_cookie;
use crate::sessions::totp_challenge::{self, TotpChallenge};
use crate::store::{sessions, totp as totp_store, users};
use crate::ui::{LoginPage, PublicErrorMessage, TotpChallengePage};

#[derive(Debug, Deserialize)]
pub struct LoginQuery {
    pub login_challenge: Option<String>,
    pub return_to: Option<String>,
}

// ntex's per-thread service futures are intentionally `!Send` (Rc-based
// internal state). Marking each handler `#[allow(clippy::future_not_send)]`
// is the canonical workaround; the lint is structural, not actionable.
#[allow(clippy::future_not_send)]
pub async fn get(
    req: HttpRequest,
    query: ntex::web::types::Query<LoginQuery>,
    admin: ntex::web::types::State<HydraAdmin>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    let query = query.into_inner();
    let Some(challenge) = query.login_challenge.as_deref() else {
        return get_native(req, query.return_to.as_deref(), cfg.as_ref(), db.as_ref()).await;
    };

    // P5: delete (Hydra arm).
    // Fetch challenge details from hydra.
    let info = match admin.get_login(challenge).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, challenge = %challenge, "login challenge fetch failed");
            return render_error(PublicErrorMessage::InvalidRequest);
        }
    };

    // Skip path: hydra already knows the subject.
    if info.skip {
        let subject_uuid = uuid::Uuid::parse_str(&info.subject).ok();
        let eligible = match subject_uuid {
            Some(subject_id) => eligibility::check_user_eligible(db.as_ref(), subject_id).await,
            None => Err(LoginIneligible::NotFound),
        };
        if let Err(e) = eligible {
            if !e.is_account_state() {
                tracing::error!(error = %e, subject = %info.subject, "skip-login eligibility check failed");
                return render_error(PublicErrorMessage::ContactSupport);
            }
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "login_failure",
                    outcome: "failure",
                    user_id: subject_uuid.as_ref(),
                    client_id: Some(&info.client.client_id),
                    auth_method: Some("hydra_skip"),
                    detail: json!({ "reason": "account_ineligible" }),
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
            let reject = RejectRequest {
                error: "access_denied".into(),
                error_description: Some("account temporarily locked".into()),
                status_code: Some(403),
            };
            match admin.reject_login(challenge, &reject).await {
                Ok(resp) => return redirect(&resp.redirect_to),
                Err(e) => {
                    tracing::error!(error = %e, subject = %info.subject, "reject_login (skip path) failed");
                    return render_error(PublicErrorMessage::ContactSupport);
                }
            }
        }
        let accept = AcceptLoginRequest {
            subject: info.subject.clone(),
            remember: Some(true),
            remember_for: Some(3600),
            ..Default::default()
        };
        match admin.accept_login(challenge, &accept).await {
            Ok(resp) => return redirect(&resp.redirect_to),
            Err(e) => {
                tracing::error!(error = %e, "accept_login (skip path) failed");
                return render_error(PublicErrorMessage::ContactSupport);
            }
        }
    }

    // Render the form with a fresh CSRF token cookie.
    let csrf_token = csrf::generate_token();
    let page = LoginPage {
        challenge,
        return_to: "",
        csrf: &csrf_token,
        error: None,
        client_name: info.client.client_name.as_deref().unwrap_or(&info.client.client_id),
        google_enabled: cfg.google_client_id.is_some(),
        github_enabled: cfg.github_client_id.is_some(),
        is_hydra: true,
    };
    let body = match page.render() {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "render login.html failed");
            return render_error(PublicErrorMessage::ContactSupport);
        }
    };

    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token, cfg.insecure_dev));
    resp.body(body)
}

#[allow(clippy::future_not_send)]
async fn get_native(
    req: HttpRequest,
    raw_return_to: Option<&str>,
    cfg: &AuthConfig,
    db: &compio_postgres::Client,
) -> HttpResponse {
    let return_to = return_to::sanitize(raw_return_to, return_to::SAFE_DEFAULT);
    let auth_request = AuthRequest::parse_return_to(&return_to).ok();
    match resolve_native_session(&req, cfg, db).await {
        Ok(Some(_)) => {
            return return_to::see_other(&return_to)
                .header("cache-control", "no-store")
                .finish();
        }
        Ok(None) => {}
        Err(resp) => return resp,
    }

    if let Some(location) = auth_request.as_ref().and_then(|request| {
        request.provider_start_location(
            cfg.google_client_id.is_some(),
            cfg.github_client_id.is_some(),
        )
    }) {
        return return_to::see_other(&location)
            .header("cache-control", "no-store")
            .finish();
    }

    render_login_form_native(
        &return_to,
        &native_client_name(auth_request.as_ref()),
        cfg,
        None,
        200,
    )
}

fn redirect(to: &str) -> HttpResponse {
    let mut resp = HttpResponse::Found();
    resp.header(
        LOCATION,
        HeaderValue::from_str(to).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    resp.finish()
}

fn render_error(message: PublicErrorMessage) -> HttpResponse {
    use crate::ui::ErrorPage;
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

// ─── POST /login ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct LoginForm {
    pub csrf: String,
    pub email: String,
    pub password: String,
    pub return_to: Option<String>,
    pub login_challenge: Option<String>,
}

/// `/login` POST — credential flow per proposal §8.1.
///
/// 1. CSRF (form vs `__Host-zsidp_csrf` cookie).
/// 2. Rate-limit (3 buckets in order: email+ip, email, ip).
/// 3. User lookup by email; dummy-hash fallback when user absent / locked /
///    has no `password_hash` (account-enumeration defense).
/// 4. Argon2 verify wrapped in `compio::runtime::spawn_blocking` (~100 ms,
///    must not block the ntex event loop).
/// 5. On success: insert `zeroship.idp_sessions`, set `__Host-zsidp_session` cookie,
///    `accept_login` to hydra, 302 to hydra's `redirect_to`.
/// 6. On failure: re-render the form with a status code matching the
///    failure mode (400 / 401 / 429 / 500). Cookies refreshed so the form
///    stays usable for a retry.
#[allow(clippy::too_many_lines)]
// ntex's per-thread service futures are intentionally `!Send`. See note on
// `get`.
#[allow(clippy::future_not_send)]
pub async fn post(
    req: HttpRequest,
    query: ntex::web::types::Query<LoginQuery>,
    form: ntex::web::types::Form<LoginForm>,
    admin: ntex::web::types::State<HydraAdmin>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    let query = query.into_inner();
    let form = form.into_inner();
    if let Some(challenge) = query
        .login_challenge
        .as_deref()
        .or(form.login_challenge.as_deref())
    {
        // P5: delete (Hydra arm).
        return post_hydra(
            req,
            challenge,
            &form,
            &admin,
            cfg.as_ref(),
            db.as_ref(),
        )
        .await;
    }

    post_native(req, query.return_to.as_deref(), &form, cfg.as_ref(), db.as_ref()).await
}

#[allow(clippy::future_not_send, clippy::too_many_lines)]
async fn post_hydra(
    req: HttpRequest,
    challenge: &str,
    form: &LoginForm,
    admin: &HydraAdmin,
    cfg: &AuthConfig,
    db: &compio_postgres::Client,
) -> HttpResponse {
    // Best-effort fetch of client_name so the re-rendered LoginPage on
    // failure still shows the relying-party label. If hydra is unreachable
    // we render an opaque error page; the login flow cannot proceed without
    // a challenge anyway.
    let info = match admin.get_login(&challenge).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, challenge = %challenge, "POST /login: get_login failed");
            return render_error(PublicErrorMessage::InvalidRequest);
        }
    };
    let client_id = info.client.client_id.clone();
    let client_name = info
        .client
        .client_name
        .clone()
        .unwrap_or_else(|| client_id.clone());

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
        return render_login_error(
            &challenge,
            "",
            true,
            &client_name,
            cfg,
            "invalid request",
            400,
        );
    }

    // 2. Resolve the remote IP (rate-limit bucket + audit). Use the trusted,
    //    gateway-authored client IP (SEC-3) — never the spoofable leftmost
    //    X-Forwarded-For token `connection_info().remote()` returns.
    let ip = crate::headers::client_ip(&req);

    // 3–4 + failure arms: the constant-time, fail-closed credential check
    // (rate-limit → lookup → dummy-hash defense → Argon2 verify → eligibility
    // → audit) lives in `identity::credentials` as the single shared
    // verification body. Map its failure classes back to the form-re-render /
    // opaque-error responses this HTML handler uses.
    let verified = match verify_password_credentials(
        db,
        &req,
        &client_id,
        &ip,
        &form.email,
        &form.password,
    )
    .await
    {
        Ok(v) => v,
        Err(CredentialError::RateLimited) => {
            return render_login_error(
                &challenge,
                "",
                true,
                &client_name,
                cfg,
                "too many attempts, try again later",
                429,
            );
        }
        Err(CredentialError::InvalidCredentials) => {
            return render_login_error(
                &challenge,
                "",
                true,
                &client_name,
                cfg,
                "invalid email or password",
                401,
            );
        }
        Err(CredentialError::Ineligible) => {
            return render_login_error(
                &challenge,
                "",
                true,
                &client_name,
                cfg,
                PublicErrorMessage::AccountTemporarilyLocked.as_str(),
                403,
            );
        }
        Err(CredentialError::Internal) => {
            return render_error(PublicErrorMessage::ContactSupport);
        }
    };

    // 5. Password verified. If the user has a CONFIRMED TOTP credential, do NOT
    // mint a session / accept_login yet — require a second factor. We attest
    // "factor 1 passed" in a short-lived, HMAC-signed `__Host-zsidp_2fa` cookie
    // (bound to user_id + credential_version + this hydra challenge) and render
    // the code-entry form. `/login/2fa` finishes the flow. A pending (un-
    // confirmed) enrollment does NOT gate login (`is_enabled` is confirmed-only).
    match totp_store::is_enabled(db, verified.id).await {
        Ok(true) => {
            let stash = TotpChallenge::new(
                verified.id,
                verified.credential_version,
                challenge.to_string(),
            );
            let cookie = stash.encode(cfg.stash_signing_key.as_bytes());
            let csrf_token = csrf::generate_token();
            let page = TotpChallengePage {
                challenge: &challenge,
                return_to: "",
                csrf: &csrf_token,
                error: None,
                is_hydra: true,
            };
            let body = match page.render() {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!(error = %e, "render totp_challenge.html failed");
                    return render_error(PublicErrorMessage::ContactSupport);
                }
            };
            let mut resp = HttpResponse::Ok();
            resp.content_type("text/html; charset=utf-8");
            resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token, cfg.insecure_dev));
            resp.header(
                SET_COOKIE,
                totp_challenge::set_cookie(&cookie, cfg.insecure_dev),
            );
            return resp.body(body);
        }
        Ok(false) => {}
        Err(e) => {
            // Fail CLOSED: if we cannot determine 2FA status we must not skip
            // the second factor for a user who may have it enabled.
            tracing::error!(error = %e, user_id = %verified.id, "totp is_enabled check failed");
            return render_error(PublicErrorMessage::ContactSupport);
        }
    }

    // 5'. No 2FA — finish the login (session + accept_login + 302).
    finish_login_hydra(admin, cfg, db, verified.id, verified.credential_version, challenge, &["pwd"], None).await
}

#[allow(clippy::future_not_send, clippy::too_many_lines)]
async fn post_native(
    req: HttpRequest,
    query_return_to: Option<&str>,
    form: &LoginForm,
    cfg: &AuthConfig,
    db: &compio_postgres::Client,
) -> HttpResponse {
    let return_to = return_to::sanitize(
        form.return_to.as_deref().or(query_return_to),
        return_to::SAFE_DEFAULT,
    );
    // MED-3: do NOT derive the audit `client_id` from `return_to`. Only /authorize
    // pre-validates that param; a direct /login hit lets an attacker forge it, and
    // login is client-agnostic anyway (the real client binding + its audit happen at
    // /authorize → /consent → /token). Stamp a fixed sentinel so a forged return_to
    // cannot poison the login_failure/login_success audit label.
    let client_id = "native".to_string();
    let auth_request = AuthRequest::parse_return_to(&return_to).ok();
    let client_name = native_client_name(auth_request.as_ref());

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
        return render_login_error("", &return_to, false, &client_name, cfg, "invalid request", 400);
    }

    let ip = crate::headers::client_ip(&req);
    let verified = match verify_password_credentials(
        db,
        &req,
        &client_id,
        &ip,
        &form.email,
        &form.password,
    )
    .await
    {
        Ok(v) => v,
        Err(CredentialError::RateLimited) => {
            return render_login_error(
                "",
                &return_to,
                false,
                &client_name,
                cfg,
                "too many attempts, try again later",
                429,
            );
        }
        Err(CredentialError::InvalidCredentials) => {
            return render_login_error(
                "",
                &return_to,
                false,
                &client_name,
                cfg,
                "invalid email or password",
                401,
            );
        }
        Err(CredentialError::Ineligible) => {
            return render_login_error(
                "",
                &return_to,
                false,
                &client_name,
                cfg,
                PublicErrorMessage::AccountTemporarilyLocked.as_str(),
                403,
            );
        }
        Err(CredentialError::Internal) => {
            return render_error(PublicErrorMessage::ContactSupport);
        }
    };

    match totp_store::is_enabled(db, verified.id).await {
        Ok(true) => {
            let stash = TotpChallenge::new(
                verified.id,
                verified.credential_version,
                return_to.clone(),
            );
            let cookie = stash.encode(cfg.stash_signing_key.as_bytes());
            let csrf_token = csrf::generate_token();
            let page = TotpChallengePage {
                challenge: "",
                return_to: &return_to,
                csrf: &csrf_token,
                error: None,
                is_hydra: false,
            };
            let body = match page.render() {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!(error = %e, "render totp_challenge.html failed");
                    return render_error(PublicErrorMessage::ContactSupport);
                }
            };
            let mut resp = HttpResponse::Ok();
            resp.content_type("text/html; charset=utf-8");
            resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token, cfg.insecure_dev));
            resp.header(
                SET_COOKIE,
                totp_challenge::set_cookie(&cookie, cfg.insecure_dev),
            );
            return resp.body(body);
        }
        Ok(false) => {}
        Err(e) => {
            tracing::error!(error = %e, user_id = %verified.id, "totp is_enabled check failed");
            return render_error(PublicErrorMessage::ContactSupport);
        }
    }

    finish_login_native(
        cfg,
        db,
        verified.id,
        verified.credential_version,
        &return_to,
        &["pwd"],
        None,
    )
    .await
}

/// Complete a verified login: create the IdP session row, bump `last_login_at`,
/// `accept_login` to hydra, and 302 with the session cookie. `amr` records the
/// methods used (`["pwd"]` or `["pwd", "otp"]`). `clear_challenge_cookie`, when
/// set, additionally clears the `__Host-zsidp_2fa` cookie (the 2FA path).
///
/// Shared by the no-2FA login tail and the `/login/2fa` second-factor handler so
/// there is ONE session-mint + accept_login body.
#[allow(clippy::future_not_send, clippy::too_many_arguments)]
async fn finish_login(
    db: &compio_postgres::Client,
    user_id: uuid::Uuid,
    credential_version: i64,
    amr: &[&str],
) -> Option<sessions::Session> {
    let session = match sessions::create(
        db,
        &sessions::CreateSession {
            user_id,
            auth_method: "pwd",
            amr: amr.iter().map(|s| (*s).to_string()).collect(),
            acr: Some("urn:zeroship:pwd"),
            expected_credential_version: Some(credential_version),
            idle_minutes: session_cookie::IDLE_MINUTES,
            absolute_hours: session_cookie::ABSOLUTE_HOURS,
        },
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "sessions::create failed");
            return None;
        }
    };

    if let Err(e) = users::touch_last_login(db, user_id).await {
        tracing::warn!(error = %e, user_id = %user_id, "touch_last_login failed");
    }

    Some(session)
}

#[allow(clippy::future_not_send, clippy::too_many_arguments)]
async fn finish_login_hydra(
    admin: &HydraAdmin,
    cfg: &AuthConfig,
    db: &compio_postgres::Client,
    user_id: uuid::Uuid,
    credential_version: i64,
    challenge: &str,
    amr: &[&str],
    clear_challenge_cookie: Option<()>,
) -> HttpResponse {
    let Some(session) = finish_login(db, user_id, credential_version, amr).await else {
        return render_error(PublicErrorMessage::ContactSupport);
    };

    let accept = AcceptLoginRequest {
        subject: user_id.to_string(),
        remember: Some(true),
        remember_for: Some(3600),
        acr: Some("urn:zeroship:pwd".into()),
        amr: Some(amr.iter().map(|s| (*s).to_string()).collect()),
        ..Default::default()
    };
    let redirect_to = match admin.accept_login(challenge, &accept).await {
        Ok(resp) => resp.redirect_to,
        Err(e) => {
            tracing::error!(error = %e, "accept_login failed");
            return render_error(PublicErrorMessage::ContactSupport);
        }
    };

    let mut resp = HttpResponse::Found();
    resp.header(
        LOCATION,
        HeaderValue::from_str(&redirect_to).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    resp.header(
        SET_COOKIE,
        session_cookie::set_cookie(&session.id, cfg.insecure_dev),
    );
    if clear_challenge_cookie.is_some() {
        resp.header(SET_COOKIE, totp_challenge::clear_cookie(cfg.insecure_dev));
    }
    resp.finish()
}

#[allow(clippy::future_not_send, clippy::too_many_arguments)]
async fn finish_login_native(
    cfg: &AuthConfig,
    db: &compio_postgres::Client,
    user_id: uuid::Uuid,
    credential_version: i64,
    return_to: &str,
    amr: &[&str],
    clear_challenge_cookie: Option<()>,
) -> HttpResponse {
    let Some(session) = finish_login(db, user_id, credential_version, amr).await else {
        return render_error(PublicErrorMessage::ContactSupport);
    };

    let mut resp = return_to::see_other(return_to);
    resp.header(
        SET_COOKIE,
        session_cookie::set_cookie(&session.id, cfg.insecure_dev),
    );
    resp.header("cache-control", "no-store");
    if clear_challenge_cookie.is_some() {
        resp.header(SET_COOKIE, totp_challenge::clear_cookie(cfg.insecure_dev));
    }
    resp.finish()
}

// ─── POST /login/2fa ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct TotpForm {
    pub csrf: String,
    pub code: String,
    pub return_to: Option<String>,
    pub login_challenge: Option<String>,
}

/// `/login/2fa` POST — the second factor (ISS-11).
///
/// Reached only after `/login` POST verified the password for a TOTP-enabled
/// user and set the signed `__Host-zsidp_2fa` cookie. Algorithm:
///
/// 1. CSRF (double-submit).
/// 2. Decode + verify the signed challenge cookie (factor-1 attestation). A
///    missing/forged/expired cookie → back to `/login`.
/// 3. Re-check the cookie's `credential_version` against the live user row — a
///    password change / forced logout since factor 1 invalidates the challenge.
/// 4. Rate-limit the verify (per-user) so the 6-digit code + backup codes can't
///    be brute-forced.
/// 5. Accept a valid TOTP code (±1 step skew) OR an unused backup code (marked
///    used on redeem). Only then `finish_login` (session + accept_login).
#[allow(clippy::future_not_send, clippy::too_many_lines)]
pub async fn post_2fa(
    req: HttpRequest,
    query: ntex::web::types::Query<LoginQuery>,
    form: ntex::web::types::Form<TotpForm>,
    admin: ntex::web::types::State<HydraAdmin>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    let query = query.into_inner();
    let form = form.into_inner();
    let hydra_challenge = query
        .login_challenge
        .as_deref()
        .or(form.login_challenge.as_deref())
        .map(str::to_string);
    let return_to = return_to::sanitize(
        form.return_to.as_deref().or(query.return_to.as_deref()),
        return_to::SAFE_DEFAULT,
    );

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
        return redirect_to_login(hydra_challenge.as_deref(), &return_to);
    }

    // 2. Decode + verify the factor-1 challenge cookie.
    let Some(stash) = totp_challenge::parse_cookie(cookie_header, cfg.insecure_dev)
        .and_then(|raw| TotpChallenge::decode(&raw, cfg.stash_signing_key.as_bytes()))
    else {
        return redirect_to_login(hydra_challenge.as_deref(), &return_to);
    };
    let stash_target = if let Some(challenge) = hydra_challenge.as_deref() {
        challenge
    } else {
        return_to.as_str()
    };
    if stash.return_to != stash_target {
        return redirect_to_login(hydra_challenge.as_deref(), &return_to);
    }

    // 3. Re-fetch the user; credential_version must still match (a password
    // change / forced logout since factor 1 invalidates this challenge).
    let user = match users::find_by_id(db.as_ref(), &stash.user_id.to_string()).await {
        Ok(Some(u)) => u,
        Ok(None) => return redirect_to_login(hydra_challenge.as_deref(), &return_to),
        Err(e) => {
            tracing::error!(error = %e, "post_2fa find_by_id failed");
            return render_error(PublicErrorMessage::ContactSupport);
        }
    };
    if user.credential_version != stash.credential_version {
        return render_2fa_error(
            hydra_challenge.as_deref().unwrap_or(""),
            &return_to,
            hydra_challenge.is_some(),
            cfg.as_ref(),
            "session expired, sign in again",
        );
    }

    // 4. Rate-limit the verify (per-user).
    let rl_key = format!("totp:verify:{}", user.id);
    match ratelimit::consume(db.as_ref(), &rl_key, Bucket::TOTP_VERIFY).await {
        Ok(RateLimitDecision::Allowed) => {}
        Ok(RateLimitDecision::Throttled(_)) => {
            return render_2fa_error(
                hydra_challenge.as_deref().unwrap_or(""),
                &return_to,
                hydra_challenge.is_some(),
                cfg.as_ref(),
                "too many attempts, try again later",
            );
        }
        Err(e) => {
            tracing::error!(error = %e, "post_2fa rate-limit consume failed");
            return render_error(PublicErrorMessage::ContactSupport);
        }
    }

    // 5. The credential must still be confirmed/enabled.
    let cred = match totp_store::find_confirmed(db.as_ref(), user.id).await {
        Ok(Some(c)) => c,
        Ok(None) => return redirect_to_login(hydra_challenge.as_deref(), &return_to),
        Err(e) => {
            tracing::error!(error = %e, "post_2fa find_confirmed failed");
            return render_error(PublicErrorMessage::ContactSupport);
        }
    };

    // 5a. Try the TOTP code first.
    let key = match totp::key_from_config(&cfg.totp_enc_key) {
        Ok(k) => k,
        Err(e) => {
            tracing::error!(error = %e, "totp enc key misconfigured");
            return render_error(PublicErrorMessage::ContactSupport);
        }
    };
    let mut second_factor_ok = false;
    if let Ok(secret) = totp::decrypt_secret(&key, user.id, &cred.encrypted_secret) {
        if totp::verify_code(&secret, &form.code) {
            second_factor_ok = true;
        }
    }

    // 5b. Otherwise try an unused backup code (constant-time per-code via Argon2).
    if !second_factor_ok {
        match totp_store::unused_backup_codes(db.as_ref(), user.id).await {
            Ok(codes) => {
                for c in &codes {
                    if totp::verify_backup_code(&form.code, &c.code_hash).unwrap_or(false) {
                        // Single-use: mark it; only count the factor if WE won
                        // the mark-used race.
                        match totp_store::mark_backup_code_used(db.as_ref(), c.id).await {
                            Ok(true) => second_factor_ok = true,
                            Ok(false) => {} // already used concurrently — reject
                            Err(e) => {
                                tracing::error!(error = %e, "mark_backup_code_used failed");
                            }
                        }
                        break;
                    }
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "unused_backup_codes failed");
                return render_error(PublicErrorMessage::ContactSupport);
            }
        }
    }

    if !second_factor_ok {
        audit::emit(
            db.as_ref(),
            &AuditEvent {
                event_type: "login_2fa_failure",
                outcome: "failure",
                user_id: Some(&user.id),
                auth_method: Some("otp"),
                detail: json!({ "reason": "invalid_second_factor" }),
                ..AuditEvent::from_request(&req)
            },
        )
        .await;
        return render_2fa_error(
            hydra_challenge.as_deref().unwrap_or(""),
            &return_to,
            hydra_challenge.is_some(),
            cfg.as_ref(),
            "invalid code",
        );
    }

    audit::emit(
        db.as_ref(),
        &AuditEvent {
            event_type: "login_2fa_success",
            outcome: "success",
            user_id: Some(&user.id),
            auth_method: Some("otp"),
            detail: json!({}),
            ..AuditEvent::from_request(&req)
        },
    )
    .await;

    if let Some(challenge) = hydra_challenge.as_deref() {
        // P5: delete (Hydra arm).
        finish_login_hydra(
            &admin,
            cfg.as_ref(),
            db.as_ref(),
            user.id,
            user.credential_version,
            challenge,
            &["pwd", "otp"],
            Some(()),
        )
        .await
    } else {
        finish_login_native(
            cfg.as_ref(),
            db.as_ref(),
            user.id,
            user.credential_version,
            &return_to,
            &["pwd", "otp"],
            Some(()),
        )
        .await
    }
}

/// Re-render the 2FA challenge page with an error banner + fresh CSRF cookie.
fn render_2fa_error(
    challenge: &str,
    return_to: &str,
    is_hydra: bool,
    cfg: &AuthConfig,
    err: &str,
) -> HttpResponse {
    let csrf_token = csrf::generate_token();
    let page = TotpChallengePage {
        challenge,
        return_to,
        csrf: &csrf_token,
        error: Some(err),
        is_hydra,
    };
    let body = page.render().unwrap_or_else(|_| format!("<h1>{err}</h1>"));
    let mut resp = HttpResponse::build(ntex::http::StatusCode::UNAUTHORIZED);
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token, cfg.insecure_dev));
    resp.body(body)
}

fn redirect_to_login(hydra_challenge: Option<&str>, return_to: &str) -> HttpResponse {
    let location = if let Some(challenge) = hydra_challenge {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("login_challenge", challenge)
            .finish();
        format!("/login?{query}")
    } else {
        return_to::login_location(return_to)
    };
    return_to::see_other(&location)
        .header("cache-control", "no-store")
        .finish()
}

/// Re-render the login page with an error banner + fresh CSRF cookie, at the
/// given HTTP status. Identical body shape for every failure mode (the only
/// thing that varies is the visible message + status) so timing and content
/// don't leak which arm rejected the request.
fn render_login_error(
    challenge: &str,
    return_to: &str,
    is_hydra: bool,
    client_name: &str,
    cfg: &AuthConfig,
    err: &str,
    status: u16,
) -> HttpResponse {
    let csrf_token = csrf::generate_token();
    let page = LoginPage {
        challenge,
        return_to,
        csrf: &csrf_token,
        error: Some(err),
        client_name,
        google_enabled: cfg.google_client_id.is_some(),
        github_enabled: cfg.github_client_id.is_some(),
        is_hydra,
    };
    let body = page
        .render()
        .unwrap_or_else(|_| format!("<h1>{err}</h1>"));
    let code = ntex::http::StatusCode::from_u16(status)
        .unwrap_or(ntex::http::StatusCode::BAD_REQUEST);
    let mut resp = HttpResponse::build(code);
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token, cfg.insecure_dev));
    resp.body(body)
}

fn render_login_form_native(
    return_to: &str,
    client_name: &str,
    cfg: &AuthConfig,
    err: Option<&str>,
    status: u16,
) -> HttpResponse {
    let csrf_token = csrf::generate_token();
    let page = LoginPage {
        challenge: "",
        return_to,
        csrf: &csrf_token,
        error: err,
        client_name,
        google_enabled: false,
        github_enabled: false,
        is_hydra: false,
    };
    let body = page
        .render()
        .unwrap_or_else(|_| format!("<h1>{}</h1>", err.unwrap_or("sign in")));
    let code = ntex::http::StatusCode::from_u16(status)
        .unwrap_or(ntex::http::StatusCode::BAD_REQUEST);
    let mut resp = HttpResponse::build(code);
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token, cfg.insecure_dev));
    resp.body(body)
}

#[allow(clippy::future_not_send)]
async fn resolve_native_session(
    req: &HttpRequest,
    cfg: &AuthConfig,
    db: &compio_postgres::Client,
) -> Result<Option<sessions::Session>, HttpResponse> {
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let Some(session_id) = session_cookie::parse_cookie(cookie_header, cfg.insecure_dev) else {
        return Ok(None);
    };
    sessions::validate(db, session_id).await.map_err(|err| {
        tracing::error!(error = %err, "native login session validation failed");
        render_error(PublicErrorMessage::ContactSupport)
    })
}

fn native_client_name(auth_request: Option<&AuthRequest>) -> String {
    auth_request
        .map(|request| request.client_id.clone())
        .unwrap_or_else(|| "zeroship".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use askama::Template;

    #[test]
    fn login_page_oauth_buttons_gated_by_config() {
        let page = LoginPage {
            challenge: "abc",
            return_to: "",
            csrf: "xyz",
            error: None,
            client_name: "Test",
            google_enabled: true,
            github_enabled: false,
            is_hydra: true,
        };
        let html = page.render().expect("render");
        assert!(
            html.contains("/oauth/google/start"),
            "google button should be present"
        );
        assert!(
            !html.contains("/oauth/github/start"),
            "github button should be absent"
        );
        assert!(html.contains("Sign in with Google"));
    }

    #[test]
    fn login_page_hides_section_if_no_oauth() {
        let page = LoginPage {
            challenge: "abc",
            return_to: "",
            csrf: "xyz",
            error: None,
            client_name: "Test",
            google_enabled: false,
            github_enabled: false,
            is_hydra: true,
        };
        let html = page.render().expect("render");
        assert!(
            !html.contains("oauth-buttons"),
            "OAuth section should be hidden"
        );
        assert!(!html.contains("or sign in with"));
    }
}
