//! Native OP device grant support plus the internal platform-token mint.

use std::sync::Arc;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Duration, Utc};
use compio_postgres::Client;
use ntex::http::StatusCode;
use ntex::web::{self, HttpRequest, HttpResponse};
use rand::{Rng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroship_authz::Scope;
use zeroship_core::auth::{extract_bearer, validate_control_key};

use crate::config::AuthConfig;
use crate::oidc::authorization_code::{
    load_client, mint_access_token, oauth_error_response, parse_scopes, required_param,
    scope_subset, sort_dedup, OAuthError, TokenRequest, TokenResponse, TOKEN_TYPE_BEARER,
};
use crate::oidc::refresh::{self, ClientAuth, ClientAuthMethod, RefreshSessionPool, RefreshTokenKeys};
use crate::oidc::{Issuer, PrincipalAccessTokenMint, ACCESS_TOKEN_TTL_SECS};

pub const INTERNAL_PLATFORM_TOKEN_PATH: &str = "/internal/platform-token";
pub const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

const DEVICE_CODE_BYTES: usize = 32;
const DEVICE_TTL_SECS: i64 = 600;
const INITIAL_POLL_INTERVAL_SECS: i32 = 5;
const USER_CODE_ALPHABET: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ";
const USER_CODE_GROUP_LEN: usize = 4;
const USER_CODE_GROUPS: usize = 3;
const USER_CODE_CHAR_LEN: usize = USER_CODE_GROUP_LEN * USER_CODE_GROUPS;
const USER_CODE_FORMATTED_LEN: usize = USER_CODE_CHAR_LEN + (USER_CODE_GROUPS - 1);
const USER_CODE_ATTEMPTS: usize = 8;
const OP_DEVICE_PROVIDER: &str = "op";
const DEFAULT_DEVICE_SCOPE: &str = "openid";

#[derive(Debug, Deserialize)]
pub struct DeviceAuthorizationRequest {
    pub client_id: String,
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DeviceAuthorizationResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceApproval {
    Approved,
    NotFound,
}

#[derive(Debug, Clone)]
pub(crate) struct NativeDeviceGrantDetails {
    pub client_id: String,
    pub client_name: String,
    pub scopes: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct MintPlatformTokenRequest {
    pub principal_id: String,
    pub audience: String,
    pub client_id: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    pub ttl_secs: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct MintPlatformTokenResponse {
    pub access_token: String,
    pub token_type: &'static str,
    pub expires_in: u64,
    pub scope: String,
    pub provider: &'static str,
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource(INTERNAL_PLATFORM_TOKEN_PATH).route(web::post().to(mint_platform_token)),
    );
}

#[allow(clippy::future_not_send)]
pub async fn device_authorization(
    form: web::types::Form<DeviceAuthorizationRequest>,
    cfg: web::types::State<Arc<AuthConfig>>,
    db: web::types::State<Arc<Client>>,
) -> HttpResponse {
    match device_authorization_inner(form.into_inner(), cfg.as_ref(), db.as_ref()).await {
        Ok(body) => HttpResponse::Ok()
            .header("cache-control", "no-store")
            .header("pragma", "no-cache")
            .json(&body),
        Err(err) => oauth_error_response(err),
    }
}

#[allow(clippy::future_not_send)]
async fn device_authorization_inner(
    params: DeviceAuthorizationRequest,
    cfg: &AuthConfig,
    db: &Client,
) -> Result<DeviceAuthorizationResponse, OAuthError> {
    let client_id = required_param(Some(params.client_id.as_str()), "client_id")?;
    let client = load_client(db, client_id).await?;
    if client.brokered || client.token_endpoint_auth_method != "none" {
        return Err(oauth_error(
            StatusCode::BAD_REQUEST,
            "unauthorized_client",
            "client is not allowed to use the device grant",
        ));
    }

    let requested_scopes = match params.scope.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(scope) => parse_scopes(scope),
        None => vec![DEFAULT_DEVICE_SCOPE.to_string()],
    };
    if !scope_subset(&requested_scopes, &client.scopes) {
        return Err(OAuthError::invalid_scope("scope is not allowed for client"));
    }
    let scope = requested_scopes.join(" ");

    for _ in 0..USER_CODE_ATTEMPTS {
        let device_code = generate_device_code();
        let device_code_hash = sha256_hex(&device_code);
        let user_code = generate_user_code();
        let inserted = db
            .query_opt(
                "INSERT INTO zeroship.device_grants \
                    (device_code_hash, user_code, status, provider, scope, client_id, expires_at, \
                     poll_interval_secs) \
                 VALUES ($1, $2, 'pending', $3, $4, $5, NOW() + make_interval(secs => $6::INT), \
                         $7) \
                 ON CONFLICT DO NOTHING \
                 RETURNING user_code",
                &[
                    &device_code_hash,
                    &user_code,
                    &OP_DEVICE_PROVIDER,
                    &scope,
                    &client.client_id,
                    &i32::try_from(DEVICE_TTL_SECS).unwrap_or(600),
                    &INITIAL_POLL_INTERVAL_SECS,
                ],
            )
            .await
            .map_err(|err| {
                tracing::error!(error = %err, client_id = %client.client_id, "device authorization insert failed");
                OAuthError::server_error("device authorization store unavailable")
            })?;
        if inserted.is_some() {
            let verification_uri = format!("{}/device", cfg.public_url());
            let verification_uri_complete = verification_uri_complete(&verification_uri, &user_code);
            return Ok(DeviceAuthorizationResponse {
                device_code,
                user_code,
                verification_uri,
                verification_uri_complete,
                expires_in: DEVICE_TTL_SECS as u64,
                interval: INITIAL_POLL_INTERVAL_SECS as u64,
            });
        }
    }

    tracing::error!("device authorization exhausted user_code collision retries");
    Err(OAuthError::server_error("device authorization unavailable"))
}

#[allow(clippy::future_not_send)]
pub(crate) async fn native_user_code_details(
    db: &Client,
    user_code: &str,
) -> Result<Option<NativeDeviceGrantDetails>, String> {
    let user_code = normalize_user_code(user_code);
    if user_code.is_empty() {
        return Ok(None);
    }
    let rows = db
        .query(
            "SELECT dg.client_id, dg.scope, COALESCE(oc.client_name, dg.client_id) AS client_name \
             FROM zeroship.device_grants dg \
             JOIN zeroship.oauth_clients oc ON oc.client_id = dg.client_id \
             WHERE dg.user_code = $1 \
               AND dg.provider = $2 \
               AND dg.status = 'pending' \
               AND dg.expires_at > NOW() \
             LIMIT 1",
            &[&user_code, &OP_DEVICE_PROVIDER],
        )
        .await
        .map_err(|err| format!("native device grant detail lookup failed: {err}"))?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let scope: String = row.get("scope");
    Ok(Some(NativeDeviceGrantDetails {
        client_id: row.get("client_id"),
        client_name: row.get("client_name"),
        scopes: parse_scopes(&scope),
    }))
}

#[allow(clippy::future_not_send)]
pub(crate) async fn approve_user_code(
    db: &Client,
    user_code: &str,
    user_id: Uuid,
    sid: Uuid,
    auth_credential_version: i64,
) -> Result<DeviceApproval, String> {
    let user_code = normalize_user_code(user_code);
    if user_code.is_empty() {
        return Ok(DeviceApproval::NotFound);
    }
    let updated = db
        .execute(
            "UPDATE zeroship.device_grants \
             SET principal_id = $1, \
                 sid = $2, \
                 auth_credential_version = $3, \
                 status = 'approved' \
             WHERE user_code = $4 \
               AND provider = $5 \
               AND status = 'pending' \
               AND expires_at > NOW()",
            &[
                &user_id,
                &sid.to_string(),
                &auth_credential_version,
                &user_code,
                &OP_DEVICE_PROVIDER,
            ],
        )
        .await
        .map_err(|err| format!("native device grant approve failed: {err}"))?;
    if updated == 0 {
        Ok(DeviceApproval::NotFound)
    } else {
        Ok(DeviceApproval::Approved)
    }
}

#[allow(clippy::future_not_send)]
pub(super) async fn exchange_device_code(
    params: &TokenRequest,
    client_auth: &ClientAuth,
    cfg: &AuthConfig,
    db: &Client,
    issuer: &Issuer,
    refresh_pool: &RefreshSessionPool,
) -> Result<TokenResponse, OAuthError> {
    let client_id = required_param(params.client_id.as_deref(), "client_id")?;
    let device_code = required_param(params.device_code.as_deref(), "device_code")?;
    let client = load_client(db, client_id).await?;
    authenticate_device_client(&client, client_auth)?;
    let device_code_hash = sha256_hex(device_code);

    let pool = refresh_pool
        .checkout_pool("token device_code")
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "device token: dedicated database pool checkout failed");
            OAuthError::server_error("device token database unavailable")
        })?;
    let mut conn = pool.get().await.map_err(|err| {
        tracing::error!(error = %err, "device token: dedicated database session checkout failed");
        OAuthError::server_error("device token database unavailable")
    })?;
    let tx = conn.transaction().await.map_err(|err| {
        tracing::error!(error = %err, "device token: BEGIN failed on dedicated session");
        OAuthError::server_error("device token transaction unavailable")
    })?;

    let result = exchange_device_code_locked(&tx, cfg, issuer, &client, &device_code_hash).await;
    match result {
        Ok(response) => {
            tx.commit().await.map_err(|err| {
                tracing::error!(error = %err, "device token: COMMIT failed");
                OAuthError::server_error("device token transaction failed")
            })?;
            Ok(response)
        }
        Err(err) => {
            if let Err(rollback) = tx.rollback().await {
                tracing::error!(error = %rollback, "device token: ROLLBACK failed");
            }
            Err(err)
        }
    }
}

#[allow(clippy::future_not_send)]
async fn exchange_device_code_locked(
    db: &(impl compio_postgres::GenericClient + ?Sized),
    cfg: &AuthConfig,
    issuer: &Issuer,
    client: &crate::oidc::authorization_code::OAuthClient,
    device_code_hash: &str,
) -> Result<TokenResponse, OAuthError> {
    let row = db
        .query_opt(
            "SELECT status, expires_at, last_polled_at, poll_interval_secs, principal_id, \
                    client_id, scope, auth_credential_version, sid \
             FROM zeroship.device_grants \
             WHERE device_code_hash = $1 AND provider = $2 \
             FOR UPDATE",
            &[&device_code_hash, &OP_DEVICE_PROVIDER],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "device token: lookup failed");
            OAuthError::server_error("device token store unavailable")
        })?;
    let Some(row) = row else {
        return Err(device_error("expired_token", "device code is invalid or expired"));
    };

    let status: String = row.get("status");
    let expires_at: DateTime<Utc> = row.get("expires_at");
    let last_polled_at: Option<DateTime<Utc>> = row.get("last_polled_at");
    let poll_interval_secs: i32 = row.get("poll_interval_secs");
    let stored_client_id: Option<String> = row.get("client_id");
    if stored_client_id.as_deref() != Some(client.client_id.as_str()) {
        return Err(OAuthError::invalid_grant("device code binding mismatch"));
    }

    let now = Utc::now();
    if expires_at <= now {
        delete_device_grant(db, device_code_hash).await?;
        return Err(device_error("expired_token", "device code expired"));
    }
    if let Some(last) = last_polled_at {
        if now - last < Duration::seconds(i64::from(poll_interval_secs)) {
            db.execute(
                "UPDATE zeroship.device_grants \
                 SET last_polled_at = NOW(), \
                     poll_interval_secs = poll_interval_secs + 5 \
                 WHERE device_code_hash = $1",
                &[&device_code_hash],
            )
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "device token: slow_down stamp failed");
                OAuthError::server_error("device token store unavailable")
            })?;
            return Err(device_error("slow_down", "device code was polled too quickly"));
        }
    }

    match status.as_str() {
        "pending" => {
            db.execute(
                "UPDATE zeroship.device_grants \
                 SET last_polled_at = NOW() \
                 WHERE device_code_hash = $1",
                &[&device_code_hash],
            )
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "device token: pending poll stamp failed");
                OAuthError::server_error("device token store unavailable")
            })?;
            Err(device_error(
                "authorization_pending",
                "device authorization is pending",
            ))
        }
        "denied" => {
            delete_device_grant(db, device_code_hash).await?;
            Err(device_error("access_denied", "device authorization was denied"))
        }
        "approved" => {
            let Some(user_id) = row.get::<_, Option<Uuid>>("principal_id") else {
                tracing::error!("device token: approved grant missing principal_id");
                return Err(OAuthError::server_error("device grant is incomplete"));
            };
            let auth_credential_version: i64 = row.get("auth_credential_version");
            let requested_scopes = row
                .get::<_, Option<String>>("scope")
                .map(|scope| parse_scopes(&scope))
                .unwrap_or_default();
            let granted_scopes = sort_dedup(requested_scopes);
            let Some(sid) = row.get::<_, Option<String>>("sid") else {
                tracing::error!("device token: approved grant missing sid");
                return Err(OAuthError::server_error("device grant is incomplete"));
            };

            delete_device_grant(db, device_code_hash).await?;

            let access_token = mint_access_token(issuer, client, user_id, &granted_scopes)?;
            let refresh_token =
                if granted_scopes.iter().any(|scope| scope == "offline_access")
                    && client.refresh_allowed
                {
                    let keys = RefreshTokenKeys::from_config(cfg)?;
                    Some(
                        refresh::issue_root_refresh_token(
                            db,
                            issuer,
                            &keys,
                            client,
                            user_id,
                            &granted_scopes,
                            auth_credential_version,
                        )
                        .await?,
                    )
                } else {
                    None
                };

            tracing::debug!(client_id = %client.client_id, user_id = %user_id, sid = %sid, "device token approved");
            Ok(TokenResponse {
                access_token,
                id_token: None,
                refresh_token,
                token_type: TOKEN_TYPE_BEARER,
                expires_in: ACCESS_TOKEN_TTL_SECS as u64,
                scope: granted_scopes.join(" "),
            })
        }
        other => {
            tracing::error!(status = other, "device token: invalid grant status");
            Err(OAuthError::server_error("device grant status is invalid"))
        }
    }
}

async fn delete_device_grant(
    db: &(impl compio_postgres::GenericClient + ?Sized),
    device_code_hash: &str,
) -> Result<(), OAuthError> {
    db.execute(
        "DELETE FROM zeroship.device_grants WHERE device_code_hash = $1",
        &[&device_code_hash],
    )
    .await
    .map_err(|err| {
        tracing::error!(error = %err, "device token: grant delete failed");
        OAuthError::server_error("device token store unavailable")
    })?;
    Ok(())
}

fn authenticate_device_client(
    client: &crate::oidc::authorization_code::OAuthClient,
    client_auth: &ClientAuth,
) -> Result<(), OAuthError> {
    if client.brokered || client.token_endpoint_auth_method != "none" {
        return Err(oauth_error(
            StatusCode::BAD_REQUEST,
            "unauthorized_client",
            "client is not allowed to use the device grant",
        ));
    }
    if !matches!(client_auth.method, ClientAuthMethod::None) {
        return Err(OAuthError::invalid_client(
            "device grant client authentication is not supported",
        ));
    }
    if let Some(auth_client_id) = client_auth.client_id.as_deref() {
        if auth_client_id != client.client_id {
            return Err(OAuthError::invalid_client("client authentication mismatch"));
        }
    }
    Ok(())
}

fn oauth_error(
    status: StatusCode,
    error: &'static str,
    description: &'static str,
) -> OAuthError {
    OAuthError {
        status,
        error,
        description,
    }
}

fn device_error(error: &'static str, description: &'static str) -> OAuthError {
    oauth_error(StatusCode::BAD_REQUEST, error, description)
}

fn generate_device_code() -> String {
    let mut bytes = [0_u8; DEVICE_CODE_BYTES];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn generate_user_code() -> String {
    let mut rng = rand::thread_rng();
    let mut code = String::with_capacity(USER_CODE_FORMATTED_LEN);
    for idx in 0..USER_CODE_CHAR_LEN {
        if idx > 0 && idx % USER_CODE_GROUP_LEN == 0 {
            code.push('-');
        }
        let alphabet_idx = rng.gen_range(0..USER_CODE_ALPHABET.len());
        code.push(char::from(USER_CODE_ALPHABET[alphabet_idx]));
    }
    code
}

fn normalize_user_code(value: &str) -> String {
    let chars: String = value
        .trim()
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace() && *ch != '-')
        .map(|ch| ch.to_ascii_uppercase())
        .collect();

    if chars.len() != USER_CODE_CHAR_LEN
        || !chars
            .as_bytes()
            .iter()
            .all(|ch| USER_CODE_ALPHABET.contains(ch))
    {
        return value
            .trim()
            .chars()
            .filter(|ch| !ch.is_ascii_whitespace())
            .map(|ch| ch.to_ascii_uppercase())
            .collect();
    }

    let mut normalized = String::with_capacity(USER_CODE_FORMATTED_LEN);
    for (idx, ch) in chars.chars().enumerate() {
        if idx > 0 && idx % USER_CODE_GROUP_LEN == 0 {
            normalized.push('-');
        }
        normalized.push(ch);
    }
    normalized
}

pub(crate) fn valid_user_code(value: &str) -> bool {
    let code = normalize_user_code(value);
    let bytes = code.as_bytes();
    bytes.len() == USER_CODE_FORMATTED_LEN
        && bytes
            .iter()
            .enumerate()
            .all(|(idx, ch)| {
                if (idx + 1) % (USER_CODE_GROUP_LEN + 1) == 0 {
                    *ch == b'-'
                } else {
                    USER_CODE_ALPHABET.contains(ch)
                }
            })
}

fn sha256_hex(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn verification_uri_complete(verification_uri: &str, user_code: &str) -> String {
    let mut url = url::Url::parse(verification_uri).expect("verification_uri is absolute");
    url.query_pairs_mut().append_pair("user_code", user_code);
    url.to_string()
}

#[allow(clippy::future_not_send)]
pub async fn mint_platform_token(
    req: HttpRequest,
    cfg: web::types::State<Arc<AuthConfig>>,
    issuer: web::types::State<Arc<Issuer>>,
    body: web::types::Json<MintPlatformTokenRequest>,
) -> HttpResponse {
    if !authorized(&req, cfg.as_ref()) {
        return HttpResponse::Unauthorized().json(&json!({"error": "unauthorized"}));
    }

    let principal_id = body.principal_id.trim();
    if Uuid::parse_str(principal_id).is_err() {
        return HttpResponse::BadRequest().json(&json!({"error": "invalid_principal_id"}));
    }
    if body.audience.trim().is_empty() || body.client_id.trim().is_empty() {
        return HttpResponse::BadRequest().json(&json!({"error": "invalid_token_request"}));
    }
    if body
        .scopes
        .iter()
        .any(|scope| Scope::parse(scope.trim()).is_err())
    {
        return HttpResponse::BadRequest().json(&json!({"error": "invalid_scope"}));
    }
    let ttl_secs = body.ttl_secs.unwrap_or(ACCESS_TOKEN_TTL_SECS);
    if ttl_secs <= 0 {
        return HttpResponse::BadRequest().json(&json!({"error": "invalid_ttl"}));
    }

    let scopes: Vec<String> = body
        .scopes
        .iter()
        .map(|scope| scope.trim().to_string())
        .filter(|scope| !scope.is_empty())
        .collect();
    let access_token = match issuer.issue_principal_access_token(&PrincipalAccessTokenMint {
        principal_id,
        audience: body.audience.trim(),
        client_id: body.client_id.trim(),
        scopes: &scopes,
        ttl_secs: Some(ttl_secs),
    }) {
        Ok(token) => token,
        Err(err) => {
            tracing::error!(error = %err, "auth: platform token mint failed");
            return HttpResponse::InternalServerError().json(&json!({"error": "mint_failed"}));
        }
    };

    HttpResponse::Ok().json(&MintPlatformTokenResponse {
        access_token,
        token_type: "Bearer",
        expires_in: ttl_secs as u64,
        scope: scopes.join(" "),
        provider: "platform",
    })
}

fn authorized(req: &HttpRequest, cfg: &AuthConfig) -> bool {
    let expected = cfg.control_key.trim();
    if expected.is_empty() {
        return false;
    }
    let header = req
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    extract_bearer(header).is_some_and(|provided| validate_control_key(provided, expected))
}
