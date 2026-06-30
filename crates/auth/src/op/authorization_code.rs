//! Closed-world OAuth2/OIDC authorization-code + PKCE endpoints.

use std::sync::Arc;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use compio_postgres::Client;
use ntex::http::header::{HeaderValue, COOKIE, LOCATION};
use ntex::http::StatusCode;
use ntex::web::{self, HttpRequest, HttpResponse};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::config::AuthConfig;
use crate::op::{AccessTokenMint, IdTokenMint, Issuer, ACCESS_TOKEN_TTL_SECS};
use crate::sessions::login as login_session;
use crate::store::sessions as session_store;

const AUTH_CODE_TTL_SECS: i64 = 60;
const PKCE_METHOD_S256: &str = "S256";
const TOKEN_TYPE_BEARER: &str = "Bearer";

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/authorize")
            .route(web::get().to(authorize_get))
            .route(web::post().to(authorize_post)),
    )
    .service(web::resource("/token").route(web::post().to(token_post)));
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthorizeRequest {
    pub response_type: Option<String>,
    pub client_id: Option<String>,
    pub redirect_uri: Option<String>,
    pub scope: Option<String>,
    pub state: Option<String>,
    pub nonce: Option<String>,
    pub code_challenge: Option<String>,
    pub code_challenge_method: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TokenRequest {
    pub grant_type: String,
    pub code: Option<String>,
    pub redirect_uri: Option<String>,
    pub client_id: Option<String>,
    pub code_verifier: Option<String>,
}

#[derive(Debug, Serialize)]
struct TokenResponse {
    access_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id_token: Option<String>,
    token_type: &'static str,
    expires_in: u64,
    scope: String,
}

#[derive(Debug, Clone)]
struct OAuthClient {
    client_id: String,
    redirect_uris: Vec<String>,
    scopes: Vec<String>,
    app_id: Option<Uuid>,
    sector_identifier: String,
}

impl OAuthClient {
    fn resource_audience(&self) -> String {
        self.app_id
            .map(|id| format!("app:{id}"))
            .unwrap_or_else(|| "zeroship".to_string())
    }
}

#[derive(Debug)]
struct ConsumedCode {
    client_id: String,
    redirect_uri: String,
    pkce_challenge: String,
    pkce_method: String,
    granted_scopes: Vec<String>,
    nonce: Option<String>,
    user_id: Uuid,
}

#[derive(Debug)]
struct OAuthError {
    status: StatusCode,
    error: &'static str,
    description: &'static str,
}

impl OAuthError {
    fn invalid_request(description: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error: "invalid_request",
            description,
        }
    }

    fn invalid_client(description: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error: "invalid_client",
            description,
        }
    }

    fn invalid_grant(description: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error: "invalid_grant",
            description,
        }
    }

    fn invalid_scope(description: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error: "invalid_scope",
            description,
        }
    }

    fn unsupported_response_type(description: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error: "unsupported_response_type",
            description,
        }
    }

    fn unsupported_grant_type(description: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error: "unsupported_grant_type",
            description,
        }
    }

    fn server_error(description: &'static str) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error: "server_error",
            description,
        }
    }
}

#[allow(clippy::future_not_send)]
pub async fn authorize_get(
    req: HttpRequest,
    query: web::types::Query<AuthorizeRequest>,
    cfg: web::types::State<Arc<AuthConfig>>,
    db: web::types::State<Arc<Client>>,
    issuer: web::types::State<Arc<Issuer>>,
) -> HttpResponse {
    authorize(req, query.into_inner(), cfg.as_ref(), db.as_ref(), issuer.as_ref()).await
}

#[allow(clippy::future_not_send)]
pub async fn authorize_post(
    req: HttpRequest,
    form: web::types::Form<AuthorizeRequest>,
    cfg: web::types::State<Arc<AuthConfig>>,
    db: web::types::State<Arc<Client>>,
    issuer: web::types::State<Arc<Issuer>>,
) -> HttpResponse {
    authorize(req, form.into_inner(), cfg.as_ref(), db.as_ref(), issuer.as_ref()).await
}

#[allow(clippy::future_not_send)]
async fn authorize(
    req: HttpRequest,
    params: AuthorizeRequest,
    cfg: &AuthConfig,
    db: &Client,
    issuer: &Issuer,
) -> HttpResponse {
    match authorize_inner(&req, params, cfg, db, issuer).await {
        Ok(resp) => resp,
        Err(err) => oauth_error_response(err),
    }
}

#[allow(clippy::future_not_send)]
async fn authorize_inner(
    req: &HttpRequest,
    params: AuthorizeRequest,
    cfg: &AuthConfig,
    db: &Client,
    issuer: &Issuer,
) -> Result<HttpResponse, OAuthError> {
    // C1: closed-world code flow only.
    require_eq(params.response_type.as_deref(), "code", || {
        OAuthError::unsupported_response_type("response_type must be code")
    })?;

    let client_id = required_param(params.client_id.as_deref(), "client_id")?;
    let redirect_uri = required_param(params.redirect_uri.as_deref(), "redirect_uri")?;
    let client = load_client(db, client_id).await?;

    // C2: exact string match, no wildcard/prefix/suffix/normalization.
    if !client
        .redirect_uris
        .iter()
        .any(|registered| registered == redirect_uri)
    {
        return Err(OAuthError::invalid_request("redirect_uri is not registered"));
    }

    let requested_scopes = parse_scopes(params.scope.as_deref().unwrap_or(""));
    if !scope_subset(&requested_scopes, &client.scopes) {
        return Err(OAuthError::invalid_scope("scope is not allowed for client"));
    }

    // C1: PKCE is mandatory for every client and S256 is the only method.
    let code_challenge = required_param(params.code_challenge.as_deref(), "code_challenge")?;
    if !zeroship_core::pkce::is_valid_s256_challenge(code_challenge) {
        return Err(OAuthError::invalid_request("code_challenge must be S256 base64url"));
    }
    require_eq(params.code_challenge_method.as_deref(), PKCE_METHOD_S256, || {
        OAuthError::invalid_request("code_challenge_method must be S256")
    })?;

    let nonce = clean_optional(params.nonce);
    if requested_scopes.iter().any(|scope| scope == "openid") && nonce.is_none() {
        return Err(OAuthError::invalid_request("nonce is required for openid scope"));
    }

    let Some(session) = resolve_session(req, cfg, db).await? else {
        return Ok(login_redirect(req));
    };

    let granted_scopes = record_consent_grant(db, session.user_id, &client.client_id, &requested_scopes)
        .await?;

    let code = generate_code();
    let code_hash = code_hash(&code);
    db.execute(
        "INSERT INTO zeroship.oauth_authorization_codes \
            (code_hash, client_id, redirect_uri, pkce_challenge, pkce_method, \
             requested_scopes, granted_scopes, nonce, user_id, expires_at) \
         VALUES ($1, $2, $3, $4, 'S256', $5, $6, $7, $8, \
                 NOW() + ($9::text || ' seconds')::interval)",
        &[
            &code_hash,
            &client.client_id,
            &redirect_uri,
            &code_challenge,
            &requested_scopes,
            &granted_scopes,
            &nonce,
            &session.user_id,
            &AUTH_CODE_TTL_SECS.to_string(),
        ],
    )
    .await
    .map_err(|err| {
        tracing::error!(error = %err, "authorize: insert authorization code failed");
        OAuthError::server_error("authorization code store unavailable")
    })?;

    // C13: use 303, never 307. C5/C12: include the exact issuer parameter.
    let redirect = authorization_success_redirect(
        redirect_uri,
        &code,
        params.state.as_deref(),
        issuer.issuer(),
    )?;
    Ok(see_other(&redirect)
        .header("cache-control", "no-store")
        .header("referrer-policy", "no-referrer")
        .finish())
}

#[allow(clippy::future_not_send)]
pub async fn token_post(
    form: web::types::Form<TokenRequest>,
    db: web::types::State<Arc<Client>>,
    issuer: web::types::State<Arc<Issuer>>,
) -> HttpResponse {
    match token_inner(form.into_inner(), db.as_ref(), issuer.as_ref()).await {
        Ok(resp) => token_json_response(resp),
        Err(err) => oauth_error_response(err),
    }
}

#[allow(clippy::future_not_send)]
async fn token_inner(
    params: TokenRequest,
    db: &Client,
    issuer: &Issuer,
) -> Result<TokenResponse, OAuthError> {
    if params.grant_type != "authorization_code" {
        return Err(OAuthError::unsupported_grant_type(
            "only authorization_code is supported here",
        ));
    }
    let client_id = required_param(params.client_id.as_deref(), "client_id")?;
    let redirect_uri = required_param(params.redirect_uri.as_deref(), "redirect_uri")?;
    let code = required_param(params.code.as_deref(), "code")?;
    let code_verifier = required_param(params.code_verifier.as_deref(), "code_verifier")?;
    let client = load_client(db, client_id).await?;

    db.execute("BEGIN", &[])
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "token: BEGIN failed");
            OAuthError::server_error("token transaction unavailable")
        })?;

    let result = exchange_authorization_code(db, issuer, &client, redirect_uri, code, code_verifier)
        .await;
    match result {
        Ok(response) => {
            db.execute("COMMIT", &[]).await.map_err(|err| {
                tracing::error!(error = %err, "token: COMMIT failed");
                OAuthError::server_error("token transaction failed")
            })?;
            Ok(response)
        }
        Err(err) => {
            if let Err(rollback) = db.execute("ROLLBACK", &[]).await {
                tracing::error!(error = %rollback, "token: ROLLBACK failed");
            }
            Err(err)
        }
    }
}

#[allow(clippy::future_not_send)]
async fn exchange_authorization_code(
    db: &Client,
    issuer: &Issuer,
    client: &OAuthClient,
    redirect_uri: &str,
    code: &str,
    code_verifier: &str,
) -> Result<TokenResponse, OAuthError> {
    let code_hash = code_hash(code);
    let rows = db
        .query(
            "UPDATE zeroship.oauth_authorization_codes \
             SET consumed_at = NOW() \
             WHERE code_hash = $1 \
               AND consumed_at IS NULL \
               AND expires_at > NOW() \
             RETURNING client_id, redirect_uri, pkce_challenge, pkce_method, \
                       granted_scopes, nonce, user_id",
            &[&code_hash],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "token: consume authorization code failed");
            OAuthError::server_error("authorization code store unavailable")
        })?;
    let Some(row) = rows.first() else {
        // C14: not-found, expired, and already-consumed are intentionally one
        // indistinguishable invalid_grant class.
        return Err(OAuthError::invalid_grant("authorization code is invalid"));
    };

    let consumed = ConsumedCode {
        client_id: row.get("client_id"),
        redirect_uri: row.get("redirect_uri"),
        pkce_challenge: row.get("pkce_challenge"),
        pkce_method: row.get("pkce_method"),
        granted_scopes: row.get("granted_scopes"),
        nonce: row.try_get("nonce").ok().flatten(),
        user_id: row.get("user_id"),
    };

    if consumed.client_id != client.client_id || consumed.redirect_uri != redirect_uri {
        return Err(OAuthError::invalid_grant("authorization code binding mismatch"));
    }
    if consumed.pkce_method != PKCE_METHOD_S256
        || !zeroship_core::pkce::verify_s256(code_verifier, &consumed.pkce_challenge)
    {
        return Err(OAuthError::invalid_grant("pkce verification failed"));
    }
    if !consent_covers(db, consumed.user_id, &client.client_id, &consumed.granted_scopes).await? {
        return Err(OAuthError::invalid_grant("consent no longer covers grant"));
    }

    let user_id = consumed.user_id.to_string();
    let audience = client.resource_audience();
    let access_token = issuer
        .issue_access_token(&AccessTokenMint {
            user_id: &user_id,
            sector: &client.sector_identifier,
            audience: &audience,
            client_id: &client.client_id,
            scopes: &consumed.granted_scopes,
            ttl_secs: Some(ACCESS_TOKEN_TTL_SECS),
        })
        .map_err(|err| {
            tracing::error!(error = %err, "token: access-token mint failed");
            OAuthError::server_error("access token mint failed")
        })?;

    let id_token = if consumed.granted_scopes.iter().any(|scope| scope == "openid") {
        let nonce = consumed
            .nonce
            .as_deref()
            .ok_or_else(|| OAuthError::invalid_grant("openid code is missing nonce"))?;
        Some(
            issuer
                .issue_id_token(&IdTokenMint {
                    user_id: &user_id,
                    sector: &client.sector_identifier,
                    client_id: &client.client_id,
                    nonce,
                    access_token: &access_token,
                    auth_time: None,
                    amr: None,
                    acr: None,
                    ttl_secs: Some(ACCESS_TOKEN_TTL_SECS),
                })
                .map_err(|err| {
                    tracing::error!(error = %err, "token: id-token mint failed");
                    OAuthError::server_error("id token mint failed")
                })?,
        )
    } else {
        None
    };

    Ok(TokenResponse {
        access_token,
        id_token,
        token_type: TOKEN_TYPE_BEARER,
        expires_in: ACCESS_TOKEN_TTL_SECS as u64,
        scope: consumed.granted_scopes.join(" "),
    })
}

async fn resolve_session(
    req: &HttpRequest,
    cfg: &AuthConfig,
    db: &Client,
) -> Result<Option<session_store::Session>, OAuthError> {
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let Some(session_id) = login_session::parse_cookie(cookie_header, cfg.insecure_dev) else {
        return Ok(None);
    };
    session_store::validate(db, session_id).await.map_err(|err| {
        tracing::error!(error = %err, "authorize: session validation failed");
        OAuthError::server_error("session store unavailable")
    })
}

async fn load_client(db: &Client, client_id: &str) -> Result<OAuthClient, OAuthError> {
    let rows = db
        .query(
            "SELECT oc.client_id, oc.redirect_uris, oc.scopes, \
                    aoc.app_id, aoc.sector_identifier \
             FROM zeroship.oauth_clients oc \
             LEFT JOIN zeroship.app_oauth_clients aoc ON aoc.client_id = oc.client_id \
             WHERE oc.client_id = $1",
            &[&client_id],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, client_id = %client_id, "oauth client lookup failed");
            OAuthError::server_error("client registry unavailable")
        })?;
    let Some(row) = rows.first() else {
        return Err(OAuthError::invalid_client("unknown client"));
    };
    let sector_identifier = row
        .try_get::<_, Option<String>>("sector_identifier")
        .ok()
        .flatten()
        .unwrap_or_else(|| client_id.to_string());
    Ok(OAuthClient {
        client_id: row.get("client_id"),
        redirect_uris: row.get("redirect_uris"),
        scopes: sort_dedup(row.get("scopes")),
        app_id: row.try_get("app_id").ok().flatten(),
        sector_identifier,
    })
}

async fn record_consent_grant(
    db: &Client,
    user_id: Uuid,
    client_id: &str,
    requested_scopes: &[String],
) -> Result<Vec<String>, OAuthError> {
    let existing = db
        .query(
            "SELECT granted_scopes \
             FROM zeroship.oauth_grants \
             WHERE user_id = $1 AND client_id = $2",
            &[&user_id, &client_id],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "authorize: oauth grant lookup failed");
            OAuthError::server_error("consent store unavailable")
        })?;
    let mut granted = existing
        .first()
        .map(|row| row.get::<_, Vec<String>>("granted_scopes"))
        .unwrap_or_default();
    granted.extend_from_slice(requested_scopes);
    granted = sort_dedup(granted);

    db.execute(
        "INSERT INTO zeroship.oauth_grants \
             (user_id, client_id, granted_scopes, granted_at, updated_at, last_used_at) \
         VALUES ($1, $2, $3, NOW(), NOW(), NOW()) \
         ON CONFLICT (user_id, client_id) DO UPDATE \
         SET granted_scopes = EXCLUDED.granted_scopes, \
             updated_at = NOW(), \
             last_used_at = NOW()",
        &[&user_id, &client_id, &granted],
    )
    .await
    .map_err(|err| {
        tracing::error!(error = %err, "authorize: oauth grant upsert failed");
        OAuthError::server_error("consent store unavailable")
    })?;
    Ok(granted)
}

async fn consent_covers(
    db: &Client,
    user_id: Uuid,
    client_id: &str,
    granted_scopes: &[String],
) -> Result<bool, OAuthError> {
    let rows = db
        .query(
            "SELECT granted_scopes \
             FROM zeroship.oauth_grants \
             WHERE user_id = $1 AND client_id = $2",
            &[&user_id, &client_id],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "token: oauth grant lookup failed");
            OAuthError::server_error("consent store unavailable")
        })?;
    let Some(row) = rows.first() else {
        return Ok(false);
    };
    let consent_scopes = sort_dedup(row.get::<_, Vec<String>>("granted_scopes"));
    Ok(scope_subset(granted_scopes, &consent_scopes))
}

fn required_param<'a>(value: Option<&'a str>, name: &'static str) -> Result<&'a str, OAuthError> {
    let value = value.map(str::trim).unwrap_or("");
    if value.is_empty() {
        return Err(OAuthError::invalid_request(name));
    }
    Ok(value)
}

fn require_eq(
    actual: Option<&str>,
    expected: &'static str,
    err: impl FnOnce() -> OAuthError,
) -> Result<(), OAuthError> {
    if actual.map(str::trim) == Some(expected) {
        Ok(())
    } else {
        Err(err())
    }
}

fn parse_scopes(scope: &str) -> Vec<String> {
    sort_dedup(
        scope
            .split_ascii_whitespace()
            .filter(|scope| !scope.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

fn sort_dedup(mut scopes: Vec<String>) -> Vec<String> {
    scopes.sort();
    scopes.dedup();
    scopes
}

fn scope_subset(requested: &[String], allowed: &[String]) -> bool {
    let allowed = sort_dedup(allowed.to_vec());
    requested
        .iter()
        .all(|scope| allowed.binary_search(scope).is_ok())
}

fn clean_optional(value: Option<String>) -> Option<String> {
    value.map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}

fn generate_code() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn code_hash(code: &str) -> Vec<u8> {
    Sha256::digest(code.as_bytes()).to_vec()
}

fn authorization_success_redirect(
    redirect_uri: &str,
    code: &str,
    state: Option<&str>,
    issuer: &str,
) -> Result<String, OAuthError> {
    let mut url = url::Url::parse(redirect_uri)
        .map_err(|_| OAuthError::invalid_request("redirect_uri is not a valid URL"))?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("code", code);
        if let Some(state) = state.filter(|state| !state.is_empty()) {
            query.append_pair("state", state);
        }
        query.append_pair("iss", issuer);
    }
    Ok(url.to_string())
}

fn login_redirect(req: &HttpRequest) -> HttpResponse {
    let mut location = String::from("/login");
    let request_target = if req.query_string().is_empty() {
        req.path().to_string()
    } else {
        format!("{}?{}", req.path(), req.query_string())
    };
    if !request_target.is_empty() {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("return_to", &request_target)
            .finish();
        location.push('?');
        location.push_str(&query);
    }
    see_other(&location)
        .header("cache-control", "no-store")
        .finish()
}

fn see_other(location: &str) -> ntex::web::HttpResponseBuilder {
    let mut resp = HttpResponse::build(StatusCode::SEE_OTHER);
    resp.header(
        LOCATION,
        HeaderValue::from_str(location).unwrap_or_else(|_| HeaderValue::from_static("/login")),
    );
    resp
}

fn token_json_response(body: TokenResponse) -> HttpResponse {
    HttpResponse::Ok()
        .header("cache-control", "no-store")
        .header("pragma", "no-cache")
        .json(&body)
}

fn oauth_error_response(err: OAuthError) -> HttpResponse {
    HttpResponse::build(err.status)
        .header("cache-control", "no-store")
        .header("pragma", "no-cache")
        .json(&json!({
            "error": err.error,
            "error_description": err.description,
        }))
}
