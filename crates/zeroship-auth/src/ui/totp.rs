//! `/me/2fa/*` — authenticated TOTP enrollment / confirm / disable.
//!
//! These are the self-service 2FA-management endpoints, gated by the same
//! `__Host-zsidp_session` cookie + double-submit CSRF as the rest of `/me`
//! (the caller is resolved from the session, never from a request param).
//!
//!   - `POST /me/2fa/enroll`  → generate a secret, store it ENCRYPTED + PENDING
//!     (`confirmed_at = NULL`), return the `otpauth://` provisioning URI + the
//!     base32 secret for manual entry. NOT yet active. Replacing an already
//!     CONFIRMED credential resets it to pending, which turns 2FA off, so that
//!     case requires the same re-auth proof as `disable`.
//!   - `POST /me/2fa/confirm` → verify a code against the pending secret; on
//!     success flip to confirmed (2FA now gates login) and return one-time
//!     backup codes (only their hashes are stored).
//!   - `POST /me/2fa/disable` → require a current code OR password re-auth, then
//!     delete the credential + backup codes.
//!
//! Responses are JSON: enrollment naturally returns structured provisioning
//! data (URI + secret + backup codes) that an account-settings UI renders into
//! a QR code and a one-time backup-code list — there is no HTML template to fit
//! this into, and JSON keeps the surface scriptable.
//!
//! # Why turning 2FA off revokes NOTHING, and notifies instead
//!
//! Every other teardown-shaped route here revokes: `/reset` writes the
//! `(client_id, sub)` family markers, revokes the `app_session_anchors` rows and
//! bumps `credential_version`; `/logout` and `/me/sessions/{id}/revoke` emit
//! back-channel logout. Neither route in THIS file revokes anything, and that is
//! a decision, not an omission.
//!
//! A password reset must kill everything because the credential an intruder may
//! hold just became stale. Turning 2FA off is the opposite shape: nothing the
//! actor knows was invalidated, and the proof this file already demands is
//! exactly the material that makes a teardown useless.
//!
//!   - Whoever reached this state supplied a current TOTP code or the account
//!     password (`verify_reauth`). CSRF is not a second barrier: `csrf::matches`
//!     is a bare double-submit equality, so anyone holding the session cookie
//!     sets both halves themselves.
//!   - After a successful call 2FA is off, so that same proof, the password,
//!     is now sufficient to sign in from scratch. Revoking every session would
//!     evict the account holder and leave the actor one `/login` away. It buys
//!     no capability back.
//!   - A teardown would also have to cover `enroll`, since it reaches the same
//!     state, and `enroll` over a confirmed credential is the ROUTINE
//!     move-my-authenticator-to-a-new-phone flow. Signing every device out on
//!     every phone rotation is a recurring cost paid against a benefit that an
//!     attacker sidesteps by simply not disabling.
//!
//! What the escalation actually needs is for the account holder to FIND OUT, so
//! both routes send [`notify_second_factor_removed`] when a confirmed credential
//! stops gating login, and its call to action is a password reset - the one
//! action that does revoke everything. Both also write an audit event
//! (`totp_disabled` / `totp_enroll_started`), and neither is in any
//! `cron::audit_retention` sweep bucket, so the trail is kept indefinitely.
//!
//! These are the two controls the field converges on, and the teardown is not
//! one of them: of ten systems surveyed (GitLab, Ory Kratos, Supabase `GoTrue`,
//! Auth0, Keycloak, Okta, `django-allauth`, Entra ID, AWS IAM, GitHub) none
//! revokes other sessions on factor removal, and ASVS 5.0 7.4.3 asks only that
//! the application OFFER a global sign-out, which `/me/sessions/{id}/revoke`
//! already is. The full survey, the standards citations and the conditions that
//! would reopen this live in
//! `docs/decisions/2026-08-21-second-factor-removal-notifies-not-revokes.md`.

use std::sync::Arc;

use askama::Template;
use ntex::http::header::COOKIE;
use ntex::http::StatusCode;
use ntex::web::{self, HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;

use zeroship_mailer::templates::{build_email, SecondFactorRemovedHtml, SecondFactorRemovedText};
use zeroship_mailer::{Address, Mailer};

use crate::audit::{self, AuditEvent};
use crate::config::AuthConfig;
use crate::csrf;
use crate::identity::password;
use crate::identity::totp;
use crate::sessions::login as session_cookie;
use crate::store::users::UserRow;
use crate::store::{sessions, totp as totp_store, users};
use zeroship_authn::rate_limit::{self, Quota, RateLimitDecision};

/// Issuer shown in the authenticator app's account label.
const TOTP_ISSUER: &str = "zeroship";

#[derive(Debug, Deserialize)]
pub struct ConfirmForm {
    pub csrf: String,
    pub code: String,
}

/// The re-auth proof `enroll` and `disable` share: EITHER a current TOTP code OR
/// the account password. Both routes can end with the account holding no working
/// second factor, so both take the same shape. Neither field is required at the
/// parse layer - `enroll` only demands a proof when there is a confirmed
/// credential to protect - but the handler rejects the request when the proof it
/// does require is absent or wrong.
#[derive(Debug, Deserialize)]
pub struct ReauthForm {
    pub csrf: String,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

// ─── POST /me/2fa/enroll ───────────────────────────────────────────────────

/// Begin enrollment: mint a fresh secret, store it encrypted + PENDING, and
/// return the provisioning material.
///
/// Re-enrolling resets the credential to pending, so for a CONFIRMED credential
/// this is a way to turn 2FA off: `is_enabled` goes false and `/login` stops
/// challenging. That is the same end state `disable` produces, so it demands the
/// same proof - a current TOTP code or the account password. Without it a stolen
/// session cookie would be enough to disarm the second factor and then re-arm it
/// against the thief's own authenticator.
///
/// A first enrollment, or one replacing a still-PENDING credential, protects
/// nothing yet (neither gates login) and needs no re-auth - requiring one there
/// would make 2FA impossible to turn on.
#[allow(clippy::future_not_send)]
pub async fn enroll(
    req: HttpRequest,
    form: web::types::Form<ReauthForm>,
    cfg: web::types::State<Arc<AuthConfig>>,
    db: web::types::State<Arc<compio_postgres::Client>>,
    mailer: web::types::State<Arc<dyn Mailer>>,
) -> HttpResponse {
    if !csrf_ok(&req, &form.csrf) {
        return json_status(
            StatusCode::FORBIDDEN,
            &json!({ "error": "invalid_request" }),
        );
    }
    let Some(user) = resolve_user(&req, db.as_ref()).await else {
        return json_status(
            StatusCode::UNAUTHORIZED,
            &json!({ "error": "unauthenticated" }),
        );
    };

    let active = match totp_store::find_confirmed(db.as_ref(), &user.id).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "totp find_confirmed failed");
            return json_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                &json!({ "error": "server_error" }),
            );
        }
    };
    if let Some(cred) = active.as_ref() {
        // Same bucket as confirm/disable: this arm verifies a code, so it is
        // brute-forceable and belongs behind the verify throttle. The
        // no-credential and pending arms below never touch it, keeping first
        // enrollment free of throttle state.
        if rate_limited(db.as_ref(), &user.id).await {
            return json_status(
                StatusCode::TOO_MANY_REQUESTS,
                &json!({ "error": "rate_limited" }),
            );
        }
        if !verify_reauth(
            &cfg,
            &user,
            cred,
            form.code.as_deref(),
            form.password.as_deref(),
        )
        .await
        {
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "totp_enroll_refused",
                    outcome: "failure",
                    user_id: Some(&user.id),
                    auth_method: Some("totp"),
                    detail: json!({ "reason": "reauth_failed" }),
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
            return json_status(
                StatusCode::UNAUTHORIZED,
                &json!({ "error": "reauth_required" }),
            );
        }
    }

    let Some((provisioning, ciphertext)) = mint_enrollment(&cfg, &user) else {
        return json_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            &json!({ "error": "server_error" }),
        );
    };
    // `active.is_some()` here means the re-auth above passed. The store re-checks
    // under the write, so a confirm that landed since then loses the race and the
    // enrollment is refused rather than disarming a credential nobody proved
    // ownership of.
    match totp_store::enroll(db.as_ref(), &user.id, &ciphertext, active.is_some()).await {
        Ok(true) => {}
        Ok(false) => {
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "totp_enroll_refused",
                    outcome: "failure",
                    user_id: Some(&user.id),
                    auth_method: Some("totp"),
                    detail: json!({ "reason": "confirmed_credential_exists" }),
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
            return json_status(
                StatusCode::UNAUTHORIZED,
                &json!({ "error": "reauth_required" }),
            );
        }
        Err(e) => {
            tracing::error!(error = %e, user_id = user.id.as_str(), "totp enroll store failed");
            return json_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                &json!({ "error": "server_error" }),
            );
        }
    }

    audit::emit(
        db.as_ref(),
        &AuditEvent {
            event_type: "totp_enroll_started",
            outcome: "success",
            user_id: Some(&user.id),
            auth_method: Some("totp"),
            detail: json!({ "replaced_confirmed": active.is_some() }),
            ..AuditEvent::from_request(&req)
        },
    )
    .await;

    // Only the replace-a-confirmed arm turned the login challenge off.
    if active.is_some() {
        notify_second_factor_removed(db.as_ref(), &cfg, &**mailer, &user).await;
    }

    json_status(
        StatusCode::OK,
        &json!({
            "otpauth_uri": provisioning.otpauth_uri,
            "secret": provisioning.secret_base32,
            "confirmed": false,
        }),
    )
}

// ─── POST /me/2fa/confirm ──────────────────────────────────────────────────

/// Confirm a pending enrollment with a TOTP code. On success activate the
/// credential and return one-time backup codes (shown once; only hashes stored).
#[allow(clippy::future_not_send)]
pub async fn confirm(
    req: HttpRequest,
    form: web::types::Form<ConfirmForm>,
    cfg: web::types::State<Arc<AuthConfig>>,
    db: web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    if !csrf_ok(&req, &form.csrf) {
        return json_status(
            StatusCode::FORBIDDEN,
            &json!({ "error": "invalid_request" }),
        );
    }
    let Some(user) = resolve_user(&req, db.as_ref()).await else {
        return json_status(
            StatusCode::UNAUTHORIZED,
            &json!({ "error": "unauthenticated" }),
        );
    };
    // Bound brute-force of the 6-digit code against the pending secret.
    if rate_limited(db.as_ref(), &user.id).await {
        return json_status(
            StatusCode::TOO_MANY_REQUESTS,
            &json!({ "error": "rate_limited" }),
        );
    }

    let key = match totp::key_from_config(cfg.settings.totp_enc_key.expose_str()) {
        Ok(k) => k,
        Err(e) => {
            tracing::error!(error = %e, "totp enc key misconfigured");
            return json_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                &json!({ "error": "server_error" }),
            );
        }
    };
    let Some(cred) = (match totp_store::find(db.as_ref(), &user.id).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "totp find failed");
            return json_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                &json!({ "error": "server_error" }),
            );
        }
    }) else {
        return json_status(
            StatusCode::BAD_REQUEST,
            &json!({ "error": "no_pending_enrollment" }),
        );
    };
    let secret = match totp::decrypt_secret(&key, &user.id, &cred.encrypted_secret) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "totp secret decrypt failed");
            return json_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                &json!({ "error": "server_error" }),
            );
        }
    };

    if !totp::verify_code(&secret, &form.code) {
        audit::emit(
            db.as_ref(),
            &AuditEvent {
                event_type: "totp_confirm_failed",
                outcome: "failure",
                user_id: Some(&user.id),
                auth_method: Some("totp"),
                detail: json!({ "reason": "invalid_code" }),
                ..AuditEvent::from_request(&req)
            },
        )
        .await;
        return json_status(
            StatusCode::UNAUTHORIZED,
            &json!({ "error": "invalid_code" }),
        );
    }

    // Code valid → mint backup codes and confirm atomically.
    let (plain, hashes) = match totp::generate_backup_codes() {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "backup code mint failed");
            return json_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                &json!({ "error": "server_error" }),
            );
        }
    };
    match totp_store::confirm(db.as_ref(), &user.id, &hashes).await {
        Ok(true) => {}
        Ok(false) => {
            return json_status(
                StatusCode::BAD_REQUEST,
                &json!({ "error": "no_pending_enrollment" }),
            );
        }
        Err(e) => {
            tracing::error!(error = %e, "totp confirm store failed");
            return json_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                &json!({ "error": "server_error" }),
            );
        }
    }

    audit::emit(
        db.as_ref(),
        &AuditEvent {
            event_type: "totp_enabled",
            outcome: "success",
            user_id: Some(&user.id),
            auth_method: Some("totp"),
            detail: json!({}),
            ..AuditEvent::from_request(&req)
        },
    )
    .await;

    json_status(
        StatusCode::OK,
        &json!({ "confirmed": true, "backup_codes": plain }),
    )
}

// ─── POST /me/2fa/disable ──────────────────────────────────────────────────

/// Disable 2FA after a current-code OR password re-auth. Deletes the credential
/// and all backup codes.
#[allow(clippy::future_not_send)]
pub async fn disable(
    req: HttpRequest,
    form: web::types::Form<ReauthForm>,
    cfg: web::types::State<Arc<AuthConfig>>,
    db: web::types::State<Arc<compio_postgres::Client>>,
    mailer: web::types::State<Arc<dyn Mailer>>,
) -> HttpResponse {
    if !csrf_ok(&req, &form.csrf) {
        return json_status(
            StatusCode::FORBIDDEN,
            &json!({ "error": "invalid_request" }),
        );
    }
    let Some(user) = resolve_user(&req, db.as_ref()).await else {
        return json_status(
            StatusCode::UNAUTHORIZED,
            &json!({ "error": "unauthenticated" }),
        );
    };
    if rate_limited(db.as_ref(), &user.id).await {
        return json_status(
            StatusCode::TOO_MANY_REQUESTS,
            &json!({ "error": "rate_limited" }),
        );
    }

    let Some(cred) = (match totp_store::find_confirmed(db.as_ref(), &user.id).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "totp find_confirmed failed");
            return json_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                &json!({ "error": "server_error" }),
            );
        }
    }) else {
        // No active 2FA — treat as already-disabled (idempotent).
        return json_status(StatusCode::OK, &json!({ "disabled": true }));
    };

    // Re-auth: a valid current TOTP code OR the account password.
    let reauthed = verify_reauth(
        &cfg,
        &user,
        &cred,
        form.code.as_deref(),
        form.password.as_deref(),
    )
    .await;
    if !reauthed {
        audit::emit(
            db.as_ref(),
            &AuditEvent {
                event_type: "totp_disable_failed",
                outcome: "failure",
                user_id: Some(&user.id),
                auth_method: Some("totp"),
                detail: json!({ "reason": "reauth_failed" }),
                ..AuditEvent::from_request(&req)
            },
        )
        .await;
        return json_status(
            StatusCode::UNAUTHORIZED,
            &json!({ "error": "reauth_required" }),
        );
    }

    if let Err(e) = totp_store::disable(db.as_ref(), &user.id).await {
        tracing::error!(error = %e, "totp disable store failed");
        return json_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            &json!({ "error": "server_error" }),
        );
    }

    audit::emit(
        db.as_ref(),
        &AuditEvent {
            event_type: "totp_disabled",
            outcome: "success",
            user_id: Some(&user.id),
            auth_method: Some("totp"),
            detail: json!({}),
            ..AuditEvent::from_request(&req)
        },
    )
    .await;

    notify_second_factor_removed(db.as_ref(), &cfg, &**mailer, &user).await;

    json_status(StatusCode::OK, &json!({ "disabled": true }))
}

// ─── helpers ────────────────────────────────────────────────────────────────

/// Mint a fresh secret and everything `enroll` stores or hands back: the
/// `otpauth://` provisioning material and the secret encrypted at rest.
///
/// `None` means one of the three fallible steps failed; each logs its own cause
/// and the caller turns any of them into the same `server_error`, so nothing
/// downstream has to distinguish a misconfigured key from a failed encrypt.
fn mint_enrollment(cfg: &AuthConfig, user: &UserRow) -> Option<(totp::Provisioning, Vec<u8>)> {
    let key = totp::key_from_config(cfg.settings.totp_enc_key.expose_str())
        .inspect_err(|e| tracing::error!(error = %e, "totp enc key misconfigured"))
        .ok()?;
    let secret = totp::generate_secret();
    let provisioning = totp::provisioning(&secret, TOTP_ISSUER, &user.email)
        .inspect_err(|e| tracing::error!(error = %e, "totp provisioning failed"))
        .ok()?;
    let ciphertext = totp::encrypt_secret(&key, &user.id, &secret)
        .inspect_err(|e| tracing::error!(error = %e, "totp secret encrypt failed"))
        .ok()?;
    Some((provisioning, ciphertext))
}

/// Tell the account holder their second factor stopped gating login.
///
/// This is what stands in for the session teardown the module header declines
/// to do, and it is the ONLY signal that reaches the account holder rather than
/// the operator: a stolen-session actor who also holds the password satisfies
/// `verify_reauth`, and every session -- theirs and the victim's -- survives on
/// purpose, because revoking them would not cost that actor anything (2FA is
/// off, so `/login` now takes the password alone). Detection is the defence, and
/// the mail's call to action is a password reset, which DOES revoke everything.
///
/// Best-effort, exactly like `/forgot`'s reset mail: a mailer fault is logged
/// and swallowed. It must never turn a completed credential change into a 5xx
/// that tells the caller to retry a write that already landed.
#[allow(clippy::future_not_send)]
async fn notify_second_factor_removed(
    db: &compio_postgres::Client,
    cfg: &AuthConfig,
    mailer: &dyn Mailer,
    user: &UserRow,
) {
    let name_hint = user.name.split_whitespace().next().unwrap_or("there");
    let reset_link = format!("{}/forgot", cfg.public_url());
    let time = chrono::Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();

    let html = SecondFactorRemovedHtml {
        name: name_hint,
        time: &time,
        reset_link: &reset_link,
    }
    .render()
    .unwrap_or_default();
    let text = SecondFactorRemovedText {
        name: name_hint,
        time: &time,
        reset_link: &reset_link,
    }
    .render()
    .unwrap_or_default();

    let msg = build_email(
        Address {
            email: user.email.clone(),
            name: Some(user.name.clone()),
        },
        Address {
            email: cfg.settings.mail_from_email.get().clone(),
            name: Some(cfg.settings.mail_from_name.get().clone()),
        },
        "Two-factor authentication was turned off".into(),
        text,
        html,
        vec!["second-factor-removed".into()],
    );
    if let Err(e) = mailer.send(db, msg).await {
        tracing::warn!(error = %e, user_id = user.id.as_str(), "second-factor-removed notice send failed");
    }
}

/// Verify a re-auth proof: a valid current TOTP code (decrypt the stored secret
/// and check) OR the correct account password (Argon2, on `spawn_blocking`).
/// Returns `true` if EITHER supplied proof validates. Shared by `enroll` and
/// `disable` - both can leave the account without a working second factor, so
/// both accept exactly these two proofs.
#[allow(clippy::future_not_send)]
async fn verify_reauth(
    cfg: &AuthConfig,
    user: &UserRow,
    cred: &totp_store::TotpCredential,
    code: Option<&str>,
    submitted_password: Option<&str>,
) -> bool {
    // TOTP-code proof.
    if let Some(code) = code.filter(|c| !c.trim().is_empty()) {
        if let Ok(key) = totp::key_from_config(cfg.settings.totp_enc_key.expose_str()) {
            if let Ok(secret) = totp::decrypt_secret(&key, &user.id, &cred.encrypted_secret) {
                if totp::verify_code(&secret, code) {
                    return true;
                }
            }
        }
    }
    // Password proof (only meaningful for accounts that have a password).
    if let (Some(pw), Some(phc)) = (
        submitted_password.filter(|p| !p.is_empty()),
        user.password_hash.clone(),
    ) {
        let pw = pw.to_string();
        let ok =
            compio::runtime::spawn_blocking(move || password::verify(&pw, &phc).unwrap_or(false))
                .await
                .unwrap_or(false);
        if ok {
            return true;
        }
    }
    false
}

/// Per-user TOTP verify rate-limit. Returns `true` when throttled (caller 429s).
/// Best-effort: a store fault is treated as NOT throttled (fail-open on the
/// throttle only — the credential decision itself is still fail-closed).
#[allow(clippy::future_not_send)]
async fn rate_limited(db: &compio_postgres::Client, user_id: &zeroship_core::UserId) -> bool {
    let key = format!("totp:verify:{}", user_id.as_str());
    match rate_limit::consume(db, &key, Quota::TOTP_VERIFY).await {
        Ok(RateLimitDecision::Allowed) => false,
        Ok(RateLimitDecision::Throttled(_)) => true,
        Err(e) => {
            tracing::error!(error = %e, "totp verify rate-limit consume failed");
            false
        }
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

/// Resolve the signed-in user from the `__Host-zsidp_session` cookie (mirrors
/// `me::resolve_user` / `account_deletion::resolve_user`).
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

fn json_status(status: StatusCode, body: &serde_json::Value) -> HttpResponse {
    HttpResponse::build(status).json(body)
}
