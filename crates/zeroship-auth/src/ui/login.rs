//! `/login` GET/POST handlers.

use askama::Template;
use ntex::http::header::{COOKIE, SET_COOKIE};
use ntex::web::{HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use crate::audit::{self, AuditEvent};
use crate::config::AuthConfig;
use crate::csrf;
use crate::identity::credentials::{verify_password_credentials, CredentialError};
use crate::identity::totp;
use crate::oidc::auth_request::AuthRequest;
use crate::oidc::authorization_code::{
    prompt_requests_login, return_to_after_prompt_interaction,
};
use crate::ratelimit::{self, Bucket, RateLimitDecision};
use crate::return_to;
use crate::sessions::login as session_cookie;
use crate::sessions::totp_challenge::{self, FirstFactor, TotpChallenge};
use crate::store::{sessions, totp as totp_store, users};
use crate::ui::{LoginPage, PublicErrorMessage, TotpChallengePage};

#[derive(Debug, Deserialize)]
pub struct LoginQuery {
    pub return_to: Option<String>,
}

// ntex's per-thread service futures are intentionally `!Send` (Rc-based
// internal state). Marking each handler `#[allow(clippy::future_not_send)]`
// is the canonical workaround; the lint is structural, not actionable.
#[allow(clippy::future_not_send)]
pub async fn get(
    req: HttpRequest,
    query: ntex::web::types::Query<LoginQuery>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    let query = query.into_inner();
    get_native(req, query.return_to.as_deref(), cfg.as_ref(), db.as_ref()).await
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
    let force_login = auth_request
        .as_ref()
        .is_some_and(|request| prompt_requests_login(request.prompt.as_deref()));
    match resolve_native_session(&req, cfg, db).await {
        Ok(Some(_)) if !force_login => {
            return return_to::see_other(&return_to)
                .header("cache-control", "no-store")
                .finish();
        }
        Ok(Some(_)) => {}
        Ok(None) => {}
        Err(resp) => return resp,
    }

    if let Some(location) = auth_request.as_ref().and_then(|request| {
        request.provider_start_location(
            cfg.google_client_id().is_some(),
            cfg.github_client_id().is_some(),
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
}

/// `/login` POST — credential flow per proposal §8.1.
///
/// 1. CSRF (form vs `__Host-zsidp_csrf` cookie).
/// 2. Rate-limit (3 buckets in order: email+ip, email, ip).
/// 3. User lookup by email; dummy-hash fallback when user absent / locked /
///    has no `password_hash` (account-enumeration defense).
/// 4. Argon2 verify wrapped in `compio::runtime::spawn_blocking` (~100 ms,
///    must not block the ntex event loop).
/// 5. On success: insert `zeroship.idp_sessions`, set `__Host-zsidp_session`
///    cookie, and redirect back to the native authorize request.
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
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    let query = query.into_inner();
    let form = form.into_inner();
    post_native(req, query.return_to.as_deref(), &form, cfg.as_ref(), db.as_ref()).await
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
    let cookie_token = csrf::parse_cookie(cookie_header);
    if cookie_token
        .as_deref()
        .is_none_or(|c| !csrf::matches(&form.csrf, c))
    {
        return render_login_error(&return_to, &client_name, cfg, "invalid request", 400);
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
                &return_to,
                &client_name,
                cfg,
                "too many attempts, try again later",
                429,
            );
        }
        Err(CredentialError::InvalidCredentials) => {
            return render_login_error(
                &return_to,
                &client_name,
                cfg,
                "invalid email or password",
                401,
            );
        }
        Err(CredentialError::Ineligible) => {
            return render_login_error(
                &return_to,
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
            return render_challenge(
                cfg,
                &TotpChallenge::new(
                    verified.id,
                    verified.credential_version,
                    return_to.clone(),
                    FirstFactor::Password,
                ),
            );
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

/// Render the TOTP challenge page for `stash`: the signed factor-1 cookie plus
/// a fresh CSRF cookie, and the form that POSTs both back to `/login/2fa`.
///
/// Every mint path that must demand a second factor renders through here, so
/// there is ONE challenge body and one cookie pair to reason about. The stash
/// carries the `return_to` the form echoes back, which `post_2fa` re-compares
/// against the signed copy.
#[must_use]
pub(crate) fn render_challenge(cfg: &AuthConfig, stash: &TotpChallenge) -> HttpResponse {
    let cookie = stash.encode(cfg.settings.stash_signing_key.expose_str().as_bytes());
    let csrf_token = csrf::generate_token();
    let page = TotpChallengePage {
        return_to: &stash.return_to,
        csrf: &csrf_token,
        error: None,
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
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token));
    resp.header(
        SET_COOKIE,
        totp_challenge::set_cookie(&cookie),
    );
    resp.body(body)
}

/// Complete a verified login: create the IdP session row, bump `last_login_at`,
/// and let the caller redirect with the session cookie. `amr` records the
/// methods used (`["pwd"]` or `["pwd", "otp"]`).
///
/// Shared by the no-2FA login tail and the `/login/2fa` second-factor handler so
/// there is ONE session-mint body.
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
async fn finish_login_native(
    _cfg: &AuthConfig,
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

    let return_to = return_to_after_prompt_interaction(return_to, &["login", "select_account"]);
    let mut resp = return_to::see_other(&return_to);
    resp.header(
        SET_COOKIE,
        session_cookie::set_cookie(&session.id),
    );
    resp.header("cache-control", "no-store");
    if clear_challenge_cookie.is_some() {
        resp.header(SET_COOKIE, totp_challenge::clear_cookie());
    }
    resp.finish()
}

// ─── POST /login/2fa ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct TotpForm {
    pub csrf: String,
    pub code: String,
    pub return_to: Option<String>,
}

/// `/login/2fa` POST - the second factor (ISS-11).
///
/// Reached after any mint path cleared its first factor for a TOTP-enabled user
/// and set the signed `__Host-zsidp_2fa` cookie: `/login` (password), the
/// magic-link redeem and completion, and the `/link` account-link confirm.
/// Algorithm:
///
/// 1. CSRF (double-submit).
/// 2. Decode + verify the signed challenge cookie (factor-1 attestation). A
///    missing/forged/expired cookie → back to `/login`.
/// 3. Re-check the cookie's `credential_version` against the live user row — a
///    password change / forced logout since factor 1 invalidates the challenge.
/// 4. Rate-limit the verify (per-user) so the 6-digit code + backup codes can't
///    be brute-forced.
/// 5. Accept a valid TOTP code (+/-1 step skew) OR an unused backup code
///    (marked used on redeem).
/// 6. Only then complete, in the flow named by the stash's `first_factor`, so
///    the session's `amr`/`acr` describe the factors actually presented.
#[allow(clippy::future_not_send, clippy::too_many_lines)]
pub async fn post_2fa(
    req: HttpRequest,
    query: ntex::web::types::Query<LoginQuery>,
    form: ntex::web::types::Form<TotpForm>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    let query = query.into_inner();
    let form = form.into_inner();
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
    let cookie_token = csrf::parse_cookie(cookie_header);
    if cookie_token
        .as_deref()
        .is_none_or(|c| !csrf::matches(&form.csrf, c))
    {
        return redirect_to_login(&return_to);
    }

    // 2. Decode + verify the factor-1 challenge cookie.
    let Some(stash) = totp_challenge::parse_cookie(cookie_header)
        .and_then(|raw| TotpChallenge::decode(&raw, cfg.settings.stash_signing_key.expose_str().as_bytes()))
    else {
        return redirect_to_login(&return_to);
    };
    if stash.return_to != return_to {
        return redirect_to_login(&return_to);
    }

    // 3. Re-fetch the user; credential_version must still match (a password
    // change / forced logout since factor 1 invalidates this challenge).
    let user = match users::find_by_id(db.as_ref(), &stash.user_id.to_string()).await {
        Ok(Some(u)) => u,
        Ok(None) => return redirect_to_login(&return_to),
        Err(e) => {
            tracing::error!(error = %e, "post_2fa find_by_id failed");
            return render_error(PublicErrorMessage::ContactSupport);
        }
    };
    if user.credential_version != stash.credential_version {
        return render_2fa_error(
            &return_to,
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
                &return_to,
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
        Ok(None) => return redirect_to_login(&return_to),
        Err(e) => {
            tracing::error!(error = %e, "post_2fa find_confirmed failed");
            return render_error(PublicErrorMessage::ContactSupport);
        }
    };

    // 5a. Try the TOTP code first.
    let key = match totp::key_from_config(cfg.settings.totp_enc_key.expose_str()) {
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
            &return_to,
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

    // 6. Complete in the flow that raised the challenge. Each arm owns the
    // session shape its own factors justify, so `amr`/`acr` describe what the
    // user actually did rather than what `/login` would have done.
    match &stash.first_factor {
        FirstFactor::Password => {
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
        FirstFactor::Magic => {
            crate::ui::magic::finish_after_second_factor(
                cfg.as_ref(),
                db.as_ref(),
                &user,
                &return_to,
                &req,
            )
            .await
        }
        FirstFactor::OauthLink {
            provider,
            subject,
            email,
        } => {
            crate::ui::link::finish_after_second_factor(
                cfg.as_ref(),
                db.as_ref(),
                &user,
                provider,
                subject,
                email,
                &return_to,
                &req,
            )
            .await
        }
    }
}

/// Re-render the 2FA challenge page with an error banner + fresh CSRF cookie.
fn render_2fa_error(
    return_to: &str,
    _cfg: &AuthConfig,
    err: &str,
) -> HttpResponse {
    let csrf_token = csrf::generate_token();
    let page = TotpChallengePage {
        return_to,
        csrf: &csrf_token,
        error: Some(err),
    };
    let body = page.render().unwrap_or_else(|_| format!("<h1>{err}</h1>"));
    let mut resp = HttpResponse::build(ntex::http::StatusCode::UNAUTHORIZED);
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token));
    resp.body(body)
}

fn redirect_to_login(return_to: &str) -> HttpResponse {
    let location = return_to::login_location(return_to);
    return_to::see_other(&location)
        .header("cache-control", "no-store")
        .finish()
}

fn oauth_start_href(path: &str, return_to: &str) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("return_to", return_to);
    format!("{path}?{}", query.finish())
}

/// Re-render the login page with an error banner + fresh CSRF cookie, at the
/// given HTTP status. Identical body shape for every failure mode (the only
/// thing that varies is the visible message + status) so timing and content
/// don't leak which arm rejected the request.
fn render_login_error(
    return_to: &str,
    client_name: &str,
    cfg: &AuthConfig,
    err: &str,
    status: u16,
) -> HttpResponse {
    let csrf_token = csrf::generate_token();
    let page = LoginPage {
        return_to,
        csrf: &csrf_token,
        error: Some(err),
        client_name,
        google_enabled: cfg.google_client_id().is_some(),
        github_enabled: cfg.github_client_id().is_some(),
        google_start_href: oauth_start_href("/oauth/google/start", return_to),
        github_start_href: oauth_start_href("/oauth/github/start", return_to),
    };
    let body = page
        .render()
        .unwrap_or_else(|_| format!("<h1>{err}</h1>"));
    let code = ntex::http::StatusCode::from_u16(status)
        .unwrap_or(ntex::http::StatusCode::BAD_REQUEST);
    let mut resp = HttpResponse::build(code);
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token));
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
        return_to,
        csrf: &csrf_token,
        error: err,
        client_name,
        google_enabled: cfg.google_client_id().is_some(),
        github_enabled: cfg.github_client_id().is_some(),
        google_start_href: oauth_start_href("/oauth/google/start", return_to),
        github_start_href: oauth_start_href("/oauth/github/start", return_to),
    };
    let body = page
        .render()
        .unwrap_or_else(|_| format!("<h1>{}</h1>", err.unwrap_or("sign in")));
    let code = ntex::http::StatusCode::from_u16(status)
        .unwrap_or(ntex::http::StatusCode::BAD_REQUEST);
    let mut resp = HttpResponse::build(code);
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token));
    resp.body(body)
}

#[allow(clippy::future_not_send)]
async fn resolve_native_session(
    req: &HttpRequest,
    _cfg: &AuthConfig,
    db: &compio_postgres::Client,
) -> Result<Option<sessions::Session>, HttpResponse> {
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let Some(session_id) = session_cookie::parse_cookie(cookie_header) else {
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
        let return_to = "/oauth2/authorize?client_id=oac_123&redirect_uri=https%3A%2F%2Fapp.test%2Fcb";
        let page = LoginPage {
            return_to,
            csrf: "xyz",
            error: None,
            client_name: "Test",
            google_enabled: true,
            github_enabled: false,
            google_start_href: oauth_start_href("/oauth/google/start", return_to),
            github_start_href: oauth_start_href("/oauth/github/start", return_to),
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

    /// A failed sign-in must be ANNOUNCED, and the fields must say they are the
    /// ones it is about.
    ///
    /// Measured in real Chromium against a live auth server before this landed:
    /// the accessibility tree rendered the banner as `generic: invalid email or
    /// password` with both inputs plain `textbox`. A screen-reader user who
    /// submitted a wrong password got no signal that anything had failed - the
    /// page simply changed.
    ///
    /// Both fields point at the ONE banner on purpose: the message deliberately
    /// does not say which of email or password was wrong (enumeration defense),
    /// so there is no per-field message to point at.
    ///
    /// WHAT THIS DOES NOT COVER: whether a given screen reader actually voices
    /// a `role="alert"` that is already in the document at first paint, which
    /// varies by AT and browser and cannot be asserted from a rendered string.
    /// This pins the markup contract only. It also says nothing about the other
    /// ten templates carrying the same banner; `template_a11y_error_banner`
    /// below is what covers those.
    #[test]
    fn login_page_error_is_announced_and_describes_the_fields() {
        let render = |error| {
            let return_to = "/me";
            LoginPage {
                return_to,
                csrf: "xyz",
                error,
                client_name: "Test",
                google_enabled: false,
                github_enabled: false,
                google_start_href: oauth_start_href("/oauth/google/start", return_to),
                github_start_href: oauth_start_href("/oauth/github/start", return_to),
            }
            .render()
            .expect("render")
        };

        let failed = render(Some("invalid email or password"));
        assert!(
            failed.contains(r#"<div class="error" role="alert" id="form-error">"#),
            "error banner must be an alert anchored by id: {failed}"
        );
        assert_eq!(
            failed.matches(r#"aria-describedby="form-error""#).count(),
            2,
            "both email and password must point at the banner: {failed}"
        );
        assert_eq!(
            failed.matches(r#"aria-invalid="true""#).count(),
            2,
            "both fields must report invalid: {failed}"
        );

        // The control that makes the assertions above mean something: on the
        // CLEAN render the attributes must be ABSENT. Without this, hardcoding
        // aria-invalid unconditionally would pass every check above while
        // telling every first-time visitor their empty form is already wrong.
        let clean = render(None);
        assert!(!clean.contains("aria-invalid"), "clean render: {clean}");
        assert!(!clean.contains("aria-describedby"), "clean render: {clean}");
        assert!(!clean.contains(r#"role="alert""#), "clean render: {clean}");
    }

    #[test]
    fn login_page_hides_section_if_no_oauth() {
        let return_to = "/oauth2/authorize?client_id=oac_123&redirect_uri=https%3A%2F%2Fapp.test%2Fcb";
        let page = LoginPage {
            return_to,
            csrf: "xyz",
            error: None,
            client_name: "Test",
            google_enabled: false,
            github_enabled: false,
            google_start_href: oauth_start_href("/oauth/google/start", return_to),
            github_start_href: oauth_start_href("/oauth/github/start", return_to),
        };
        let html = page.render().expect("render");
        assert!(
            !html.contains("oauth-buttons"),
            "OAuth section should be hidden"
        );
        assert!(!html.contains("or sign in with"));
    }

    #[test]
    fn native_login_page_oauth_buttons_use_return_to() {
        let return_to = "/oauth2/authorize?client_id=oac_123&redirect_uri=https%3A%2F%2Fapp.test%2Fcb";
        let page = LoginPage {
            return_to,
            csrf: "xyz",
            error: None,
            client_name: "Test",
            google_enabled: true,
            github_enabled: true,
            google_start_href: oauth_start_href("/oauth/google/start", return_to),
            github_start_href: oauth_start_href("/oauth/github/start", return_to),
        };
        let html = page.render().expect("render");
        assert!(html.contains("/oauth/google/start?return_to=%2Foauth2%2Fauthorize"));
        assert!(html.contains("/oauth/github/start?return_to=%2Foauth2%2Fauthorize"));
        assert!(
            html.contains(
                "/signup?return_to=%2Foauth2%2Fauthorize%3Fclient_id%3Doac_123%26redirect_uri%3Dhttps%253A%252F%252Fapp.test%252Fcb"
            ),
            "native signup link should preserve return_to"
        );
        assert!(!html.contains("login_challenge="));
    }
}
