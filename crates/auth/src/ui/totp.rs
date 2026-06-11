//! `/me/2fa/*` — authenticated TOTP enrollment / confirm / disable (ISS-11).
//!
//! These are the self-service 2FA-management endpoints, gated by the same
//! `__Host-zsidp_session` cookie + double-submit CSRF as the rest of `/me`
//! (the caller is resolved from the session, never from a request param).
//!
//!   - `POST /me/2fa/enroll`  → generate a secret, store it ENCRYPTED + PENDING
//!     (`confirmed_at = NULL`), return the `otpauth://` provisioning URI + the
//!     base32 secret for manual entry. NOT yet active.
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

use std::sync::Arc;

use ntex::http::header::COOKIE;
use ntex::http::StatusCode;
use ntex::web::{self, HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;

use crate::audit::{self, AuditEvent};
use crate::config::AuthConfig;
use crate::csrf;
use crate::identity::password;
use crate::identity::totp;
use crate::ratelimit::{self, Bucket, RateLimitDecision};
use crate::sessions::login as session_cookie;
use crate::store::users::UserRow;
use crate::store::{sessions, totp as totp_store, users};

/// Issuer shown in the authenticator app's account label.
const TOTP_ISSUER: &str = "zeroship";

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    pub csrf: String,
}

#[derive(Debug, Deserialize)]
pub struct ConfirmForm {
    pub csrf: String,
    pub code: String,
}

/// Disable accepts EITHER a current TOTP code OR the account password as the
/// re-auth proof. At least one must be supplied (and valid).
#[derive(Debug, Deserialize)]
pub struct DisableForm {
    pub csrf: String,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

// ─── POST /me/2fa/enroll ───────────────────────────────────────────────────

/// Begin enrollment: mint a fresh secret, store it encrypted + PENDING, and
/// return the provisioning material. Re-enrolling overwrites any pending (or
/// confirmed) credential and resets it to pending (login is no longer gated
/// until a fresh confirm).
#[allow(clippy::future_not_send)]
pub async fn enroll(
    req: HttpRequest,
    form: web::types::Form<CsrfForm>,
    cfg: web::types::State<Arc<AuthConfig>>,
    db: web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    if !csrf_ok(&req, &form.csrf, cfg.insecure_dev) {
        return json_status(StatusCode::FORBIDDEN, &json!({ "error": "invalid_request" }));
    }
    let Some(user) = resolve_user(&req, db.as_ref(), cfg.insecure_dev).await else {
        return json_status(StatusCode::UNAUTHORIZED, &json!({ "error": "unauthenticated" }));
    };
    let key = match totp::key_from_config(&cfg.totp_enc_key) {
        Ok(k) => k,
        Err(e) => {
            tracing::error!(error = %e, "totp enc key misconfigured");
            return json_status(StatusCode::INTERNAL_SERVER_ERROR, &json!({ "error": "server_error" }));
        }
    };

    let secret = totp::generate_secret();
    let provisioning = match totp::provisioning(&secret, TOTP_ISSUER, &user.email) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "totp provisioning failed");
            return json_status(StatusCode::INTERNAL_SERVER_ERROR, &json!({ "error": "server_error" }));
        }
    };
    let ciphertext = match totp::encrypt_secret(&key, user.id, &secret) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "totp secret encrypt failed");
            return json_status(StatusCode::INTERNAL_SERVER_ERROR, &json!({ "error": "server_error" }));
        }
    };
    if let Err(e) = totp_store::enroll(db.as_ref(), user.id, &ciphertext).await {
        tracing::error!(error = %e, user_id = %user.id, "totp enroll store failed");
        return json_status(StatusCode::INTERNAL_SERVER_ERROR, &json!({ "error": "server_error" }));
    }

    audit::emit(
        db.as_ref(),
        &AuditEvent {
            event_type: "totp_enroll_started",
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
    if !csrf_ok(&req, &form.csrf, cfg.insecure_dev) {
        return json_status(StatusCode::FORBIDDEN, &json!({ "error": "invalid_request" }));
    }
    let Some(user) = resolve_user(&req, db.as_ref(), cfg.insecure_dev).await else {
        return json_status(StatusCode::UNAUTHORIZED, &json!({ "error": "unauthenticated" }));
    };
    // Bound brute-force of the 6-digit code against the pending secret.
    if rate_limited(db.as_ref(), user.id).await {
        return json_status(StatusCode::TOO_MANY_REQUESTS, &json!({ "error": "rate_limited" }));
    }

    let key = match totp::key_from_config(&cfg.totp_enc_key) {
        Ok(k) => k,
        Err(e) => {
            tracing::error!(error = %e, "totp enc key misconfigured");
            return json_status(StatusCode::INTERNAL_SERVER_ERROR, &json!({ "error": "server_error" }));
        }
    };
    let Some(cred) = (match totp_store::find(db.as_ref(), user.id).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "totp find failed");
            return json_status(StatusCode::INTERNAL_SERVER_ERROR, &json!({ "error": "server_error" }));
        }
    }) else {
        return json_status(StatusCode::BAD_REQUEST, &json!({ "error": "no_pending_enrollment" }));
    };
    let secret = match totp::decrypt_secret(&key, user.id, &cred.encrypted_secret) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "totp secret decrypt failed");
            return json_status(StatusCode::INTERNAL_SERVER_ERROR, &json!({ "error": "server_error" }));
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
        return json_status(StatusCode::UNAUTHORIZED, &json!({ "error": "invalid_code" }));
    }

    // Code valid → mint backup codes and confirm atomically.
    let (plain, hashes) = match totp::generate_backup_codes() {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "backup code mint failed");
            return json_status(StatusCode::INTERNAL_SERVER_ERROR, &json!({ "error": "server_error" }));
        }
    };
    match totp_store::confirm(db.as_ref(), user.id, &hashes).await {
        Ok(true) => {}
        Ok(false) => {
            return json_status(StatusCode::BAD_REQUEST, &json!({ "error": "no_pending_enrollment" }));
        }
        Err(e) => {
            tracing::error!(error = %e, "totp confirm store failed");
            return json_status(StatusCode::INTERNAL_SERVER_ERROR, &json!({ "error": "server_error" }));
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
    form: web::types::Form<DisableForm>,
    cfg: web::types::State<Arc<AuthConfig>>,
    db: web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    if !csrf_ok(&req, &form.csrf, cfg.insecure_dev) {
        return json_status(StatusCode::FORBIDDEN, &json!({ "error": "invalid_request" }));
    }
    let Some(user) = resolve_user(&req, db.as_ref(), cfg.insecure_dev).await else {
        return json_status(StatusCode::UNAUTHORIZED, &json!({ "error": "unauthenticated" }));
    };
    if rate_limited(db.as_ref(), user.id).await {
        return json_status(StatusCode::TOO_MANY_REQUESTS, &json!({ "error": "rate_limited" }));
    }

    let Some(cred) = (match totp_store::find_confirmed(db.as_ref(), user.id).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "totp find_confirmed failed");
            return json_status(StatusCode::INTERNAL_SERVER_ERROR, &json!({ "error": "server_error" }));
        }
    }) else {
        // No active 2FA — treat as already-disabled (idempotent).
        return json_status(StatusCode::OK, &json!({ "disabled": true }));
    };

    // Re-auth: a valid current TOTP code OR the account password.
    let reauthed = verify_reauth(&cfg, &user, &cred, &form).await;
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
        return json_status(StatusCode::UNAUTHORIZED, &json!({ "error": "reauth_required" }));
    }

    if let Err(e) = totp_store::disable(db.as_ref(), user.id).await {
        tracing::error!(error = %e, "totp disable store failed");
        return json_status(StatusCode::INTERNAL_SERVER_ERROR, &json!({ "error": "server_error" }));
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

    json_status(StatusCode::OK, &json!({ "disabled": true }))
}

// ─── helpers ────────────────────────────────────────────────────────────────

/// Verify the disable re-auth proof: a valid current TOTP code (decrypt the
/// stored secret and check) OR the correct account password (Argon2, on
/// `spawn_blocking`). Returns `true` if EITHER supplied proof validates.
#[allow(clippy::future_not_send)]
async fn verify_reauth(
    cfg: &AuthConfig,
    user: &UserRow,
    cred: &totp_store::TotpCredential,
    form: &DisableForm,
) -> bool {
    // TOTP-code proof.
    if let Some(code) = form.code.as_deref().filter(|c| !c.trim().is_empty()) {
        if let Ok(key) = totp::key_from_config(&cfg.totp_enc_key) {
            if let Ok(secret) = totp::decrypt_secret(&key, user.id, &cred.encrypted_secret) {
                if totp::verify_code(&secret, code) {
                    return true;
                }
            }
        }
    }
    // Password proof (only meaningful for accounts that have a password).
    if let (Some(pw), Some(phc)) = (
        form.password.as_deref().filter(|p| !p.is_empty()),
        user.password_hash.clone(),
    ) {
        let pw = pw.to_string();
        let ok = compio::runtime::spawn_blocking(move || password::verify(&pw, &phc).unwrap_or(false))
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
async fn rate_limited(db: &compio_postgres::Client, user_id: uuid::Uuid) -> bool {
    let key = format!("totp:verify:{user_id}");
    match ratelimit::consume(db, &key, Bucket::TOTP_VERIFY).await {
        Ok(RateLimitDecision::Allowed) => false,
        Ok(RateLimitDecision::Throttled(_)) => true,
        Err(e) => {
            tracing::error!(error = %e, "totp verify rate-limit consume failed");
            false
        }
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

/// Resolve the signed-in user from the `__Host-zsidp_session` cookie (mirrors
/// `me::resolve_user` / `account_deletion::resolve_user`).
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

fn json_status(status: StatusCode, body: &serde_json::Value) -> HttpResponse {
    HttpResponse::build(status).json(body)
}
