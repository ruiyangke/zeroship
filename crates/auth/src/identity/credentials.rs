//! Shared constant-time password-credential verification.
//!
//! This is the security-critical core lifted verbatim out of
//! [`crate::ui::login`]'s POST handler so the headless in-page login endpoint
//! ([`crate::ui::password`]) reuses the *exact same* verification path rather
//! than a parallel copy. A second copy is the classic way a credential→code
//! oracle drifts out of constant-time / fail-closed discipline; there is one
//! body, here.
//!
//! Algorithm (proposal §8.1 credential path), in order:
//!
//! 1. Rate-limit — 3 buckets (email+ip, email, ip), deepest scope first.
//! 2. User lookup by normalised email.
//! 3. Dummy-hash enumeration defense — when the user is absent / locked /
//!    disabled / has no `password_hash`, verify against the dummy hash so the
//!    wall time matches a real verify (defeats email enumeration by timing).
//! 4. Argon2 verify on `spawn_blocking` (~100 ms; never parks the event loop).
//! 5. Locked / disabled (ineligible) arm → fail.
//! 6. Absent / no-credential / wrong-password arm → fail (`invalid_credentials`).
//! 7. `eligibility::check_user_eligible` → fail on account-state.
//! 8. Audit (`login_success` | `login_failure`, `auth_method: "pwd"`).
//!
//! Every failure arm returns BEFORE any caller may proceed to a session/code
//! mint. The function emits its own audit events so callers cannot forget to.

use serde_json::json;

use crate::audit::{self, AuditEvent};
use crate::identity::eligibility;
use crate::identity::password;
use crate::ratelimit::{self, Bucket, RateLimitDecision};
use crate::store::users;

/// A successfully-verified local user. Carrying the whole row lets callers mint
/// a session (login.rs) or a hydra login acceptance (password.rs) without a
/// second DB round-trip.
#[derive(Debug, Clone, Copy)]
pub struct VerifiedUser {
    pub id: uuid::Uuid,
    pub credential_version: i64,
}

/// Why a credential verification was rejected.
///
/// Each variant maps to a public HTTP status + message; callers translate to
/// HTML (login.rs) or JSON (password.rs). Audit emission for the failure has
/// ALREADY happened inside [`verify_password_credentials`] before the error is
/// returned (except `Internal`, which is an infrastructure fault, not a
/// credential decision).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialError {
    /// Rate-limit bucket exhausted. HTTP 429.
    RateLimited,
    /// No user / wrong password / OAuth-only account. HTTP 401. Opaque on
    /// purpose — never distinguishes "no such user" from "wrong password".
    InvalidCredentials,
    /// Account locked or disabled. HTTP 403.
    Ineligible,
    /// Infrastructure failure (DB / rate-limit store). HTTP 503. NOT a
    /// credential decision; no `login_failure` audit row is emitted for it.
    Internal,
}

impl CredentialError {
    /// HTTP status code for this failure class.
    #[must_use]
    pub const fn status(self) -> u16 {
        match self {
            Self::RateLimited => 429,
            Self::InvalidCredentials => 401,
            Self::Ineligible => 403,
            Self::Internal => 503,
        }
    }
}

/// Verify password credentials, constant-time and fail-closed.
///
/// `email` and `password` are the raw submitted values; `email` is normalised
/// (trim + lowercase) inside. `ip` is the caller-resolved remote address (used
/// for the per-IP rate-limit bucket + audit). `client_id` is the relying-party
/// id, recorded in audit rows.
///
/// On `Ok(VerifiedUser)` the password matched a real, eligible user and a
/// `login_success` audit row was emitted. On `Err` a `login_failure` row was
/// emitted for every credential-decision arm (not for `Internal`).
///
/// SECURITY (invariant II): every rejection arm returns BEFORE the caller can
/// reach any code/session mint, and no arm reaches the success return without a
/// successful constant-time Argon2 verify. The dummy-hash verify runs on the
/// absent/locked/disabled/no-hash paths so the wall time is uniform.
///
/// # Errors
///
/// Returns a [`CredentialError`] on any rejection arm: `RateLimited` (429),
/// `InvalidCredentials` (401), `Ineligible` (403), or `Internal` (503) on a
/// DB / rate-limit-store fault.
// ntex's per-thread service futures are intentionally `!Send`; the line-count
// lint trips on the (deliberately) linear, audited failure arms.
#[allow(clippy::future_not_send, clippy::too_many_lines)]
pub async fn verify_password_credentials(
    db: &compio_postgres::Client,
    req: &ntex::web::HttpRequest,
    client_id: &str,
    ip: &str,
    email: &str,
    password: &str,
) -> Result<VerifiedUser, CredentialError> {
    // 1. Rate limit (3 buckets, deepest scope first).
    let email_norm = email.trim().to_ascii_lowercase();
    let buckets = [
        (format!("login:eip:{email_norm}:{ip}"), Bucket::LOGIN_EIP),
        (format!("login:email:{email_norm}"), Bucket::LOGIN_EMAIL),
        (format!("login:ip:{ip}"), Bucket::LOGIN_IP),
    ];
    for (key, bucket) in &buckets {
        match ratelimit::consume(db, key, *bucket).await {
            Ok(RateLimitDecision::Allowed) => {}
            Ok(RateLimitDecision::Throttled(_)) => {
                audit::emit(
                    db,
                    &AuditEvent {
                        event_type: "login_failure",
                        outcome: "failure",
                        client_id: Some(client_id),
                        auth_method: Some("pwd"),
                        detail: json!({ "reason": "rate_limited", "bucket": key }),
                        ..AuditEvent::from_request(req)
                    },
                )
                .await;
                return Err(CredentialError::RateLimited);
            }
            Err(e) => {
                tracing::error!(error = %e, bucket = %key, "rate-limit consume failed");
                return Err(CredentialError::Internal);
            }
        }
    }

    // 2. Look up user.
    let user = match users::find_by_email(db, &email_norm).await {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(error = %e, "users::find_by_email failed");
            return Err(CredentialError::Internal);
        }
    };

    // 3. Constant-time enumeration defense: if user is None, locked, disabled,
    // or has no password hash (OAuth-only), verify against the dummy hash so
    // the wall time matches a real verify.
    let now = chrono::Utc::now();
    let phc = user
        .as_ref()
        .and_then(|u| {
            let locked = u.locked_until.is_some_and(|t| t > now);
            let disabled = u.disabled_at.is_some();
            if locked || disabled || u.password_hash.is_none() {
                None
            } else {
                u.password_hash.clone()
            }
        })
        .unwrap_or_else(|| password::dummy_hash().to_string());

    // 4. Argon2 verify — CPU-bound, run on spawn_blocking so the event loop is
    // not parked.
    let password_clone = password.to_string();
    let valid = compio::runtime::spawn_blocking(move || {
        password::verify(&password_clone, &phc).unwrap_or(false)
    })
    .await
    .unwrap_or(false);

    // 5. Locked / disabled (ineligible) arm — fail BEFORE any success path. The
    // dummy-hash verify above already spent the wall time for these users.
    let ineligible_user = user
        .as_ref()
        .filter(|u| u.locked_until.is_some_and(|t| t > now) || u.disabled_at.is_some());
    if let Some(u) = ineligible_user {
        audit::emit(
            db,
            &AuditEvent {
                event_type: "login_failure",
                outcome: "failure",
                user_id: Some(&u.id),
                client_id: Some(client_id),
                auth_method: Some("pwd"),
                detail: json!({ "reason": "account_ineligible" }),
                ..AuditEvent::from_request(req)
            },
        )
        .await;
        return Err(CredentialError::Ineligible);
    }

    // 6a. Absent user / OAuth-only account — fail (`invalid_credentials`).
    let real_user = user.as_ref().filter(|u| {
        u.locked_until.is_none_or(|t| t <= now)
            && u.disabled_at.is_none()
            && u.password_hash.is_some()
    });
    let Some(u) = real_user else {
        audit::emit(
            db,
            &AuditEvent {
                event_type: "login_failure",
                outcome: "failure",
                client_id: Some(client_id),
                auth_method: Some("pwd"),
                detail: json!({ "reason": "invalid_credentials" }),
                ..AuditEvent::from_request(req)
            },
        )
        .await;
        return Err(CredentialError::InvalidCredentials);
    };

    // 6b. Wrong password — fail (`invalid_credentials`). This is the ONLY arm
    // reachable past here, and only when `valid == true`.
    if !valid {
        audit::emit(
            db,
            &AuditEvent {
                event_type: "login_failure",
                outcome: "failure",
                user_id: Some(&u.id),
                client_id: Some(client_id),
                auth_method: Some("pwd"),
                detail: json!({ "reason": "invalid_credentials" }),
                ..AuditEvent::from_request(req)
            },
        )
        .await;
        return Err(CredentialError::InvalidCredentials);
    }

    // 7. Eligibility (re-check via the shared account-state gate). A non-state
    // error is infrastructure (Internal); a state error is Ineligible.
    if let Err(e) = eligibility::check_user_eligible(db, u.id).await {
        if !e.is_account_state() {
            tracing::error!(error = %e, user_id = %u.id, "password login eligibility check failed");
            return Err(CredentialError::Internal);
        }
        audit::emit(
            db,
            &AuditEvent {
                event_type: "login_failure",
                outcome: "failure",
                user_id: Some(&u.id),
                client_id: Some(client_id),
                auth_method: Some("pwd"),
                detail: json!({ "reason": "account_ineligible" }),
                ..AuditEvent::from_request(req)
            },
        )
        .await;
        return Err(CredentialError::Ineligible);
    }

    // 8. Success — emit the audit row and hand the verified user back.
    audit::emit(
        db,
        &AuditEvent {
            event_type: "login_success",
            outcome: "success",
            user_id: Some(&u.id),
            client_id: Some(client_id),
            auth_method: Some("pwd"),
            detail: json!({}),
            ..AuditEvent::from_request(req)
        },
    )
    .await;

    Ok(VerifiedUser {
        id: u.id,
        credential_version: u.credential_version,
    })
}
