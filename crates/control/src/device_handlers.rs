//! Platform-mediated OAuth device flow for GoTrue/Supabase.
//!
//! This is control acting as the RFC 8628 device-authorization server for an
//! IdP that lacks a native device grant. The durable credential bound at
//! approval is still a real GoTrue refresh token; control only stores it
//! encrypted until the CLI redeems the one-time grant.

use std::sync::Arc;

use base64::Engine as _;
use chrono::{DateTime, Duration, Utc};
use ntex::http::StatusCode;
use ntex::web;
use ntex::web::types::{Json, State};
use rand::{Rng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use zeroship_core::auth::extract_bearer;
use zeroship_core::auth_provider::{ProviderAuthz, VerifyTokenError};
use zeroship_core::crypto;

use crate::{identity_bridge, AppState};

const DEVICE_CODE_BYTES: usize = 32;
const DEVICE_TTL_SECS: i64 = 600;
const POLL_INTERVAL_SECS: i64 = 5;
const USER_CODE_ALPHABET: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ";
const USER_CODE_ATTEMPTS: usize = 8;
const DEVICE_REFRESH_AAD_PREFIX: &[u8] =
    b"zs:control:device_grant:gotrue_refresh:v1\0";

#[derive(Debug, Deserialize)]
pub struct DeviceAuthRequest {
    client_id: String,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DeviceAuthResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Debug, Deserialize)]
pub struct DeviceApproveRequest {
    user_code: String,
    refresh_token: String,
}

#[derive(Debug, Deserialize)]
pub struct DeviceTokenRequest {
    device_code: String,
    grant_type: String,
}

#[derive(Debug, Serialize)]
pub struct DeviceTokenResponse {
    refresh_token: String,
    token_type: &'static str,
    provider: &'static str,
    auth_url: String,
    token_endpoint: String,
    anon_key: String,
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(web::resource("/api/device/auth").route(web::post().to(device_auth)))
        .service(web::resource("/api/device/approve").route(web::post().to(device_approve)))
        .service(web::resource("/api/device/token").route(web::post().to(device_token)));
}

pub async fn device_auth(
    state: State<Arc<AppState>>,
    body: Json<DeviceAuthRequest>,
) -> web::HttpResponse {
    if let Err(resp) = ensure_supabase_provider(&state) {
        return resp;
    }
    let client_id = body.client_id.trim();
    if client_id.is_empty() || client_id.len() > 128 {
        return bad_request("invalid_client_id");
    }

    let scope = body.scope.as_deref().map(str::trim).filter(|s| !s.is_empty());
    for _ in 0..USER_CODE_ATTEMPTS {
        let device_code = generate_device_code();
        let device_code_hash = sha256_hex(&device_code);
        let user_code = generate_user_code();
        let inserted = match state
            .control_pg
            .query_opt(
                "INSERT INTO zeroship.device_grants \
                    (device_code_hash, user_code, provider, scope, expires_at) \
                 VALUES ($1, $2, 'supabase', $3, NOW() + ($4::TEXT)::INTERVAL) \
                 ON CONFLICT DO NOTHING \
                 RETURNING user_code",
                &[
                    &device_code_hash,
                    &user_code,
                    &scope,
                    &format!("{DEVICE_TTL_SECS} seconds"),
                ],
            )
            .await
        {
            Ok(row) => row,
            Err(err) => {
                tracing::error!(error = %err, "control: device grant insert failed");
                return internal_error();
            }
        };

        if inserted.is_some() {
            let verification_uri = verification_uri(&state);
            let verification_uri_complete =
                verification_uri_complete(&verification_uri, &user_code);
            return web::HttpResponse::Ok().json(&DeviceAuthResponse {
                device_code,
                user_code,
                verification_uri,
                verification_uri_complete,
                expires_in: DEVICE_TTL_SECS as u64,
                interval: POLL_INTERVAL_SECS as u64,
            });
        }
    }

    tracing::error!("control: exhausted device user_code collision retries");
    internal_error()
}

pub async fn device_approve(
    state: State<Arc<AppState>>,
    req: web::HttpRequest,
    body: Json<DeviceApproveRequest>,
) -> web::HttpResponse {
    // P-S2c owns the browser form, including CSRF/origin hardening. This API
    // slice models that browser as an authenticated GoTrue bearer plus the
    // browser-held GoTrue refresh token.
    let verified = match verified_gotrue_bearer(&state, &req).await {
        Ok(verified) => verified,
        Err(resp) => return resp,
    };

    let user_code = normalize_user_code(&body.user_code);
    if user_code.is_empty() || body.refresh_token.trim().is_empty() {
        return bad_request("invalid_user_code");
    }

    let row = match state
        .control_pg
        .query_opt(
            "SELECT device_code_hash \
             FROM zeroship.device_grants \
             WHERE user_code = $1 \
               AND status = 'pending' \
               AND expires_at > NOW()",
            &[&user_code],
        )
        .await
    {
        Ok(row) => row,
        Err(err) => {
            tracing::error!(error = %err, "control: device grant approve lookup failed");
            return internal_error();
        }
    };
    let Some(row) = row else {
        return bad_request("invalid_user_code");
    };
    let device_code_hash: String = row.get("device_code_hash");

    let Some(supabase_url) = state.auth_provider.supabase_url() else {
        return unsupported_provider();
    };
    let service_role_key = state.auth_provider.supabase_service_role_key().unwrap_or("");
    let email_verified =
        match identity_bridge::fetch_email_verified(
            supabase_url,
            service_role_key,
            &verified.provider_subject,
        )
        .await
        {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "control: GoTrue email verification lookup failed closed"
                );
                false
            }
        };

    let mut conn = match state.registry.conn().await {
        Ok(conn) => conn,
        Err(err) => {
            tracing::error!(error = %err, "control: device approve DB connect failed");
            return internal_error();
        }
    };
    let principal_id = match identity_bridge::provision_or_link(
        &mut conn,
        "supabase",
        &verified.provider_subject,
        verified.email.as_deref(),
        email_verified,
    )
    .await
    {
        Ok(principal_id) => principal_id,
        Err(err) => {
            tracing::error!(error = %err, "control: device approve identity bridge failed");
            return internal_error();
        }
    };

    let key = crypto::derive_key(state.master_key.expose_secret());
    let aad = device_refresh_aad(&device_code_hash);
    let refresh_enc = match crypto::encrypt(&key, &aad, body.refresh_token.as_bytes()) {
        Ok(value) => value,
        Err(err) => {
            tracing::error!(error = %err, "control: device refresh-token encrypt failed");
            return internal_error();
        }
    };

    let updated = match state
        .control_pg
        .execute(
            "UPDATE zeroship.device_grants \
             SET principal_id = $1, \
                 gotrue_refresh_token_enc = $2, \
                 status = 'approved' \
             WHERE device_code_hash = $3 \
               AND status = 'pending' \
               AND expires_at > NOW()",
            &[&principal_id, &refresh_enc, &device_code_hash],
        )
        .await
    {
        Ok(updated) => updated,
        Err(err) => {
            tracing::error!(error = %err, "control: device approve update failed");
            return internal_error();
        }
    };
    if updated == 0 {
        return bad_request("invalid_user_code");
    }

    web::HttpResponse::NoContent().finish()
}

pub async fn device_token(
    state: State<Arc<AppState>>,
    body: Json<DeviceTokenRequest>,
) -> web::HttpResponse {
    if body.grant_type != "urn:ietf:params:oauth:grant-type:device_code" {
        return oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type");
    }
    let device_code = body.device_code.trim();
    if device_code.is_empty() {
        return oauth_error(StatusCode::BAD_REQUEST, "expired_token");
    }
    let device_code_hash = sha256_hex(device_code);

    let mut conn = match state.registry.conn().await {
        Ok(conn) => conn,
        Err(err) => {
            tracing::error!(error = %err, "control: device token DB connect failed");
            return internal_error();
        }
    };
    let tx = match conn.transaction().await {
        Ok(tx) => tx,
        Err(err) => {
            tracing::error!(error = %err, "control: device token tx begin failed");
            return internal_error();
        }
    };
    let row = match tx
        .query_opt(
            "SELECT status, expires_at, last_polled_at, gotrue_refresh_token_enc \
             FROM zeroship.device_grants \
             WHERE device_code_hash = $1 \
             FOR UPDATE",
            &[&device_code_hash],
        )
        .await
    {
        Ok(row) => row,
        Err(err) => {
            tracing::error!(error = %err, "control: device token lookup failed");
            let _ = tx.rollback().await;
            return internal_error();
        }
    };
    let Some(row) = row else {
        let _ = tx.commit().await;
        return oauth_error(StatusCode::BAD_REQUEST, "expired_token");
    };

    let now = Utc::now();
    let status: String = row.get("status");
    let expires_at: DateTime<Utc> = row.get("expires_at");
    let last_polled_at: Option<DateTime<Utc>> = row.get("last_polled_at");

    if expires_at <= now {
        if let Err(err) = tx
            .execute(
                "DELETE FROM zeroship.device_grants WHERE device_code_hash = $1",
                &[&device_code_hash],
            )
            .await
        {
            tracing::warn!(error = %err, "control: expired device grant delete failed");
        }
        let _ = tx.commit().await;
        return oauth_error(StatusCode::BAD_REQUEST, "expired_token");
    }

    if let Some(last) = last_polled_at {
        if now - last < Duration::seconds(POLL_INTERVAL_SECS) {
            if let Err(err) = tx
                .execute(
                    "UPDATE zeroship.device_grants \
                     SET last_polled_at = NOW() \
                     WHERE device_code_hash = $1",
                    &[&device_code_hash],
                )
                .await
            {
                tracing::warn!(error = %err, "control: slow_down poll stamp failed");
            }
            let _ = tx.commit().await;
            return oauth_error(StatusCode::BAD_REQUEST, "slow_down");
        }
    }

    match status.as_str() {
        "pending" => {
            if let Err(err) = tx
                .execute(
                    "UPDATE zeroship.device_grants \
                     SET last_polled_at = NOW() \
                     WHERE device_code_hash = $1",
                    &[&device_code_hash],
                )
                .await
            {
                tracing::error!(error = %err, "control: pending device poll stamp failed");
                let _ = tx.rollback().await;
                return internal_error();
            }
            let _ = tx.commit().await;
            oauth_error(StatusCode::BAD_REQUEST, "authorization_pending")
        }
        "denied" => {
            let _ = tx
                .execute(
                    "DELETE FROM zeroship.device_grants WHERE device_code_hash = $1",
                    &[&device_code_hash],
                )
                .await;
            let _ = tx.commit().await;
            oauth_error(StatusCode::BAD_REQUEST, "access_denied")
        }
        "approved" => {
            let Some(refresh_enc) = row.get::<_, Option<Vec<u8>>>("gotrue_refresh_token_enc")
            else {
                tracing::error!("control: approved device grant missing refresh token");
                let _ = tx.rollback().await;
                return internal_error();
            };
            let key = crypto::derive_key(state.master_key.expose_secret());
            let aad = device_refresh_aad(&device_code_hash);
            let refresh_token = match crypto::decrypt(&key, &aad, &refresh_enc)
                .and_then(|plain| String::from_utf8(plain).map_err(|_| crypto::CryptoError::Decrypt))
            {
                Ok(value) => value,
                Err(err) => {
                    tracing::error!(error = %err, "control: device refresh-token decrypt failed");
                    let _ = tx.rollback().await;
                    return internal_error();
                }
            };
            if let Err(err) = tx
                .execute(
                    "DELETE FROM zeroship.device_grants WHERE device_code_hash = $1",
                    &[&device_code_hash],
                )
                .await
            {
                tracing::error!(error = %err, "control: approved device grant delete failed");
                let _ = tx.rollback().await;
                return internal_error();
            }
            if let Err(err) = tx.commit().await {
                tracing::error!(error = %err, "control: approved device grant commit failed");
                return internal_error();
            }
            let Some(supabase_url) = state.auth_provider.supabase_url() else {
                return unsupported_provider();
            };
            let Some(anon_key) = state.auth_provider.supabase_anon_key() else {
                return unsupported_provider();
            };
            web::HttpResponse::Ok().json(&DeviceTokenResponse {
                refresh_token,
                token_type: "Bearer",
                provider: "supabase",
                auth_url: supabase_url.to_string(),
                token_endpoint: supabase_refresh_token_endpoint(supabase_url),
                anon_key: anon_key.to_string(),
            })
        }
        other => {
            tracing::error!(status = other, "control: invalid device grant status");
            let _ = tx.rollback().await;
            internal_error()
        }
    }
}

#[must_use]
pub fn device_refresh_aad(device_code_hash: &str) -> Vec<u8> {
    let mut aad = Vec::with_capacity(DEVICE_REFRESH_AAD_PREFIX.len() + device_code_hash.len());
    aad.extend_from_slice(DEVICE_REFRESH_AAD_PREFIX);
    aad.extend_from_slice(device_code_hash.as_bytes());
    aad
}

fn ensure_supabase_provider(state: &AppState) -> Result<(), web::HttpResponse> {
    if state.auth_provider.supabase_url().is_some()
        && state.auth_provider.supabase_anon_key().is_some()
    {
        Ok(())
    } else {
        Err(unsupported_provider())
    }
}

async fn verified_gotrue_bearer(
    state: &AppState,
    req: &web::HttpRequest,
) -> Result<zeroship_core::auth_provider::VerifiedToken, web::HttpResponse> {
    let header = req
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let Some(token) = extract_bearer(header) else {
        return Err(web::HttpResponse::Unauthorized().json(&json!({"error": "unauthorized"})));
    };
    let verified = state
        .auth_provider
        .verify_token(token)
        .await
        .map_err(|err| match err {
            VerifyTokenError::InactiveToken
            | VerifyTokenError::MissingSubject
            | VerifyTokenError::MissingIssuer
            | VerifyTokenError::UnknownIssuer(_) => {
                web::HttpResponse::Unauthorized().json(&json!({"error": "unauthorized"}))
            }
            VerifyTokenError::HydraIntrospection(err) => {
                tracing::warn!(error = %err, "control: device approve bearer verify failed");
                web::HttpResponse::Unauthorized().json(&json!({"error": "unauthorized"}))
            }
            VerifyTokenError::PlatformVerification(err) => {
                tracing::warn!(error = %err, "control: device approve platform bearer verify failed");
                web::HttpResponse::Unauthorized().json(&json!({"error": "unauthorized"}))
            }
        })?;

    match &verified.provider_authz {
        ProviderAuthz::GoTrueRole(role) if role == "authenticated" => Ok(verified),
        _ => Err(web::HttpResponse::Unauthorized().json(&json!({"error": "unauthorized"}))),
    }
}

fn generate_device_code() -> String {
    let mut bytes = [0_u8; DEVICE_CODE_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn generate_user_code() -> String {
    let mut rng = rand::rngs::OsRng;
    let mut chars = [0_u8; 8];
    for ch in &mut chars {
        let idx = rng.gen_range(0..USER_CODE_ALPHABET.len());
        *ch = USER_CODE_ALPHABET[idx];
    }
    format!(
        "{}{}{}{}-{}{}{}{}",
        chars[0] as char,
        chars[1] as char,
        chars[2] as char,
        chars[3] as char,
        chars[4] as char,
        chars[5] as char,
        chars[6] as char,
        chars[7] as char
    )
}

fn normalize_user_code(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .flat_map(char::to_uppercase)
        .collect()
}

fn sha256_hex(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    hex::encode(digest)
}

fn verification_uri(state: &AppState) -> String {
    let scheme = if state.insecure_dev { "http" } else { "https" };
    let domain = state.app_base_domain.trim();
    let domain = if domain.is_empty() {
        "zeroship.ai"
    } else {
        domain
    };
    format!("{scheme}://auth.{domain}/device")
}

fn verification_uri_complete(verification_uri: &str, user_code: &str) -> String {
    let mut url = url::Url::parse(verification_uri).expect("verification URI is absolute");
    url.query_pairs_mut().append_pair("user_code", user_code);
    url.to_string()
}

fn supabase_refresh_token_endpoint(supabase_url: &str) -> String {
    format!(
        "{}/auth/v1/token?grant_type=refresh_token",
        supabase_url.trim_end_matches('/')
    )
}

fn unsupported_provider() -> web::HttpResponse {
    web::HttpResponse::BadRequest().json(&json!({"error": "unsupported_provider"}))
}

fn bad_request(error: &'static str) -> web::HttpResponse {
    web::HttpResponse::BadRequest().json(&json!({"error": error}))
}

fn oauth_error(status: StatusCode, error: &'static str) -> web::HttpResponse {
    web::HttpResponse::build(status).json(&json!({"error": error}))
}

fn internal_error() -> web::HttpResponse {
    web::HttpResponse::InternalServerError().json(&json!({"error": "internal error"}))
}
