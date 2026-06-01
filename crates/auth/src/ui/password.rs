//! `POST /password` — headless in-page password login (credential→code oracle).
//!
//! The interactive `/login` UI renders a form, validates a CSRF cookie, and on
//! success drives the OAuth dance via 302 redirects through the browser. The
//! in-page variant lets an app collect credentials inside its OWN page (no
//! cross-site popup) and have the gateway exchange them for an authorization
//! `code` here, which the browser then redeems at Hydra's `/oauth2/token` (it
//! holds the PKCE verifier).
//!
//! ## This endpoint is a credential→code ORACLE
//!
//! A bug here is a full auth bypass: anyone who can POST credentials and get a
//! `code` back can brute-force passwords and impersonate users. The handler is
//! ordered so the cheapest, hardest gate runs first and NOTHING reaches the
//! code mint without a successful constant-time password verify:
//!
//! 1. **SHARED-SECRET GATE (invariant I).** `Authorization: Bearer <key>` must
//!    constant-time-match the gateway↔auth `auth_internal_key`. Rejected
//!    (401/403) BEFORE any body parse, DB hit, or hashing. Unlike `/login`,
//!    this endpoint has NO `same_origin_guard` (that lives on the gateway) and
//!    is otherwise dial-able by anyone, so it authenticates its only legitimate
//!    caller — the gateway — service-to-service. Mirrors the worker's
//!    `worker_key` / control's `control_key`.
//! 2. **FAIL-CLOSED VERIFY (invariant II).** [`verify_password_credentials`]
//!    runs the full constant-time path (rate-limit → dummy-hash enumeration
//!    defense → Argon2 verify → eligibility → audit). EVERY failure arm returns
//!    here, before the dance.
//! 3. **FIRST-PARTY DANCE (invariant III).** [`mint_code_for_subject`] silently
//!    self-grants identity scopes for first-party `skip_consent` clients ONLY.
//! 4. `200 { code, state? }`.

use std::sync::Arc;

use ntex::http::header::AUTHORIZATION;
use ntex::util::Bytes;
use ntex::web::types::State;
use ntex::web::{HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;

use zeroship_core::auth::{extract_bearer, validate_control_key};

use crate::config::AuthConfig;
use crate::hydra_client::HydraAdmin;
use crate::identity::credentials::{verify_password_credentials, CredentialError};
use crate::oauth::headless::{mint_code_for_subject, HeadlessError, MintCodeParams};

/// `POST /password` request body (JSON). All fields browser-supplied except as
/// gated by the shared secret on the wire.
#[derive(Debug, Deserialize)]
pub struct PasswordLoginRequest {
    pub email: String,
    pub password: String,
    pub client_id: String,
    pub redirect_uri: String,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub nonce: Option<String>,
    pub code_challenge: String,
    #[serde(default)]
    pub code_challenge_method: Option<String>,
}

/// JSON error envelope mirroring the OAuth error shape the gateway/SDK expect.
fn error_json(status: u16, code: &str, description: &str) -> HttpResponse {
    let sc = ntex::http::StatusCode::from_u16(status)
        .unwrap_or(ntex::http::StatusCode::BAD_REQUEST);
    HttpResponse::build(sc).json(&json!({
        "error": code,
        "error_description": description,
    }))
}

/// Map a credential-verify failure class to its public JSON error. The message
/// is deliberately opaque for the credential arm (never distinguishes "no such
/// user" from "wrong password").
fn credential_error_json(e: CredentialError) -> HttpResponse {
    match e {
        CredentialError::RateLimited => {
            error_json(429, "too_many_requests", "too many attempts, try again later")
        }
        CredentialError::InvalidCredentials => {
            error_json(401, "invalid_credentials", "invalid email or password")
        }
        CredentialError::Ineligible => {
            error_json(403, "account_ineligible", "account temporarily locked")
        }
        CredentialError::Internal => {
            error_json(503, "temporarily_unavailable", "please try again")
        }
    }
}

/// SHARED-SECRET GATE (invariant I). Validate the gateway↔auth `Authorization:
/// Bearer <key>` against the configured `expected` key, constant-time.
///
/// Returns `None` when the request is authorised (continue), or `Some(reject)`
/// — a 401 (no bearer presented) or 403 (wrong bearer) — when it is not.
///
/// An EMPTY `expected` DISABLES the gate (returns `None`): dev-only loopback,
/// mirroring `worker_key`. Production boot rejects an empty key (see main.rs
/// `require_unless_dev`), so the disabled arm is unreachable in production.
fn check_internal_secret(presented: Option<&str>, expected: &str) -> Option<HttpResponse> {
    if expected.is_empty() {
        return None;
    }
    let token = presented.and_then(extract_bearer);
    match token {
        Some(t) if validate_control_key(t, expected) => None,
        Some(_) => Some(error_json(403, "forbidden", "invalid internal credential")),
        None => Some(error_json(401, "unauthorized", "internal credential required")),
    }
}

/// `POST /password` handler. See module docs for the security ordering.
//
// ntex's per-thread service futures are intentionally `!Send`.
#[allow(clippy::future_not_send)]
pub async fn post(
    req: HttpRequest,
    body: Bytes,
    admin: State<HydraAdmin>,
    cfg: State<Arc<AuthConfig>>,
    db: State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    // ── (a) SHARED-SECRET GATE (invariant I) ──────────────────────────────
    // FIRST. Before body parse, DB, or any hashing.
    let presented = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if let Some(reject) = check_internal_secret(presented, cfg.auth_internal_key.as_str()) {
        return reject;
    }

    // Parse the JSON body only AFTER the gate (don't burn CPU on attacker JSON).
    let payload: PasswordLoginRequest = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "POST /password: body parse failed");
            return error_json(400, "invalid_request", "malformed request body");
        }
    };

    // Minimal shape validation. These are required for the dance to succeed; a
    // missing one is a client bug, not a credential decision.
    if payload.email.trim().is_empty()
        || payload.password.is_empty()
        || payload.client_id.is_empty()
        || payload.redirect_uri.is_empty()
        || payload.code_challenge.is_empty()
    {
        return error_json(
            400,
            "invalid_request",
            "email, password, client_id, redirect_uri, and code_challenge are required",
        );
    }

    let ip = req
        .connection_info()
        .remote()
        .unwrap_or("0.0.0.0")
        .to_string();

    // ── (b) FAIL-CLOSED VERIFY (invariant II) ─────────────────────────────
    // Every failure arm returns BEFORE the dance. The success arm guarantees a
    // constant-time Argon2 verify against the real user happened.
    let verified = match verify_password_credentials(
        db.as_ref(),
        &req,
        &payload.client_id,
        &ip,
        &payload.email,
        &payload.password,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return credential_error_json(e),
    };

    // ── (c) FIRST-PARTY HEADLESS DANCE (invariant III) ────────────────────
    // mint_code_for_subject self-grants identity scopes for first-party
    // skip_consent clients only; a third-party client yields ConsentRequired.
    let subject = verified.id.to_string();
    let http = cyper::Client::new();
    let code = match mint_code_for_subject(
        &admin,
        db.as_ref(),
        cfg.hydra_public_url(),
        &http,
        &MintCodeParams {
            client_id: &payload.client_id,
            subject: &subject,
            redirect_uri: &payload.redirect_uri,
            scope: payload.scope.as_deref().unwrap_or(""),
            state: payload.state.as_deref().unwrap_or(""),
            nonce: payload.nonce.as_deref().unwrap_or(""),
            code_challenge: &payload.code_challenge,
            code_challenge_method: payload.code_challenge_method.as_deref().unwrap_or(""),
        },
    )
    .await
    {
        Ok(c) => c,
        Err(HeadlessError::ConsentRequired) => {
            // A non-first-party client (or non-identity scope) cannot use the
            // silent path; it must go through the interactive consent UI. This
            // is NOT a credential failure (the password was correct).
            tracing::warn!(
                client_id = %payload.client_id,
                "POST /password: client not eligible for headless silent consent"
            );
            return error_json(
                403,
                "consent_required",
                "this client must use the interactive login flow",
            );
        }
        Err(e) => {
            tracing::error!(error = %e, client_id = %payload.client_id, "POST /password: headless code mint failed");
            return error_json(503, "temporarily_unavailable", "please try again");
        }
    };

    // ── (d) 200 { code, state? } ──────────────────────────────────────────
    let mut out = serde_json::Map::new();
    out.insert("code".into(), json!(code));
    if let Some(state) = payload.state.as_deref().filter(|s| !s.is_empty()) {
        out.insert("state".into(), json!(state));
    }
    let mut resp = HttpResponse::Ok();
    resp.header("Cache-Control", "no-store");
    resp.header("Pragma", "no-cache");
    resp.json(&serde_json::Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_error_maps_to_expected_status() {
        assert_eq!(
            credential_error_json(CredentialError::RateLimited).status().as_u16(),
            429
        );
        assert_eq!(
            credential_error_json(CredentialError::InvalidCredentials)
                .status()
                .as_u16(),
            401
        );
        assert_eq!(
            credential_error_json(CredentialError::Ineligible).status().as_u16(),
            403
        );
        assert_eq!(
            credential_error_json(CredentialError::Internal).status().as_u16(),
            503
        );
    }

    #[test]
    fn error_json_sets_status_and_shape() {
        let resp = error_json(401, "unauthorized", "internal credential required");
        assert_eq!(resp.status().as_u16(), 401);
    }

    // ── SHARED-SECRET GATE (invariant I) — offline negatives ──────────────

    #[test]
    fn secret_gate_missing_bearer_is_401() {
        // No Authorization header at all.
        let reject = check_internal_secret(None, "the-internal-key");
        assert_eq!(
            reject.expect("missing bearer must reject").status().as_u16(),
            401
        );
    }

    #[test]
    fn secret_gate_non_bearer_header_is_401() {
        // Present but not a Bearer token (no "Bearer " prefix ⇒ extract_bearer None).
        let reject = check_internal_secret(Some("Basic abc"), "the-internal-key");
        assert_eq!(
            reject.expect("non-bearer must reject").status().as_u16(),
            401
        );
    }

    #[test]
    fn secret_gate_wrong_bearer_is_403() {
        let reject = check_internal_secret(Some("Bearer wrong-key"), "the-internal-key");
        assert_eq!(
            reject.expect("wrong bearer must reject").status().as_u16(),
            403
        );
    }

    #[test]
    fn secret_gate_correct_bearer_passes() {
        assert!(
            check_internal_secret(Some("Bearer the-internal-key"), "the-internal-key").is_none(),
            "correct bearer must pass the gate"
        );
    }

    #[test]
    fn secret_gate_length_mismatch_rejected_constant_time() {
        // A prefix of the real key must NOT pass (validate_control_key is
        // length-checked + constant-time).
        let reject = check_internal_secret(Some("Bearer the-internal"), "the-internal-key");
        assert_eq!(
            reject.expect("prefix must reject").status().as_u16(),
            403
        );
    }

    #[test]
    fn secret_gate_empty_expected_disables_gate() {
        // Dev-only: an empty configured key disables the gate. (Production boot
        // rejects an empty key via require_unless_dev, so this is unreachable
        // outside --dev-insecure.)
        assert!(
            check_internal_secret(None, "").is_none(),
            "empty expected key disables the gate"
        );
        assert!(
            check_internal_secret(Some("Bearer anything"), "").is_none(),
            "empty expected key disables the gate even with a presented token"
        );
    }
}
