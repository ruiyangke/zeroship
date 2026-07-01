//! Closed-world OAuth2/OIDC authorization-code + PKCE endpoints.

use std::sync::Arc;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use compio_postgres::{Client, GenericClient};
use ntex::http::header::{HeaderValue, COOKIE, LOCATION};
use ntex::http::StatusCode;
use ntex::web::{self, HttpRequest, HttpResponse};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::config::AuthConfig;
use crate::oidc::claims::scope_gated_identity_claims;
use crate::oidc::backchannel_logout;
use crate::oidc::refresh::{
    self, ClientAuth, ClientAuthMethod, RefreshSessionPool, RefreshTokenKeys,
};
use crate::oidc::{
    AccessTokenMint, IdTokenMint, Issuer, PrincipalIdTokenMint, ACCESS_TOKEN_TTL_SECS,
};
use crate::return_to;
use crate::sessions::login as login_session;
use crate::store::sessions as session_store;
use crate::store::users;

const AUTH_CODE_TTL_SECS: i64 = 60;
const PKCE_METHOD_S256: &str = "S256";
pub(super) const TOKEN_TYPE_BEARER: &str = "Bearer";

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/authorize")
            .route(web::get().to(authorize_get))
            .route(web::post().to(authorize_post)),
    )
    .service(web::resource("/token").route(web::post().to(token_post)));
    refresh::configure(cfg);
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
    pub refresh_token: Option<String>,
    pub client_secret: Option<String>,
    pub scope: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct TokenResponse {
    pub access_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    pub token_type: &'static str,
    pub expires_in: u64,
    pub scope: String,
}

#[derive(Debug, Clone)]
pub(super) struct OAuthClient {
    pub client_id: String,
    pub redirect_uris: Vec<String>,
    pub scopes: Vec<String>,
    pub app_id: Option<Uuid>,
    pub sector_identifier: String,
    pub client_secret_hash: Option<String>,
    pub refresh_allowed: bool,
    pub token_endpoint_auth_method: String,
    pub backchannel_logout_uri: Option<String>,
    /// P5a: gateway-brokered client — its id_token carries the global principal
    /// subject, and the authorization_code grant enforces confidential broker
    /// auth (see `brokered ⇒ client_secret_basic`, a DB CHECK + the load_client
    /// refusal below).
    pub brokered: bool,
}

impl OAuthClient {
    pub(super) fn resource_audience(&self) -> String {
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
    auth_credential_version: i64,
    sid: String,
}

#[derive(Debug)]
pub(super) struct OAuthError {
    pub status: StatusCode,
    pub error: &'static str,
    pub description: &'static str,
}

impl OAuthError {
    pub(super) fn invalid_request(description: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error: "invalid_request",
            description,
        }
    }

    pub(super) fn invalid_client(description: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error: "invalid_client",
            description,
        }
    }

    pub(super) fn invalid_grant(description: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error: "invalid_grant",
            description,
        }
    }

    pub(super) fn invalid_scope(description: &'static str) -> Self {
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

    pub(super) fn server_error(description: &'static str) -> Self {
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

    if consent_covers(db, session.user_id, &client.client_id, &requested_scopes).await? {
        touch_consent_grant(db, session.user_id, &client.client_id).await?;
    } else {
        return Ok(consent_redirect(req));
    }
    let granted_scopes = requested_scopes.clone();

    let code = generate_code();
    let code_hash = code_hash(&code);
    db.execute(
        "INSERT INTO zeroship.oauth_authorization_codes \
            (code_hash, client_id, redirect_uri, pkce_challenge, pkce_method, \
             requested_scopes, granted_scopes, nonce, user_id, auth_credential_version, sid, expires_at) \
         VALUES ($1, $2, $3, $4, 'S256', $5, $6, $7, $8, $9, \
                 $10, NOW() + ($11::text || ' seconds')::interval)",
        &[
            &code_hash,
            &client.client_id,
            &redirect_uri,
            &code_challenge,
            &requested_scopes,
            &granted_scopes,
            &nonce,
            &session.user_id,
            &session.credential_version,
            &session.id.to_string(),
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
    req: HttpRequest,
    form: web::types::Form<TokenRequest>,
    cfg: web::types::State<Arc<AuthConfig>>,
    db: web::types::State<Arc<Client>>,
    issuer: web::types::State<Arc<Issuer>>,
    refresh_pool: web::types::State<RefreshSessionPool>,
) -> HttpResponse {
    let params = form.into_inner();
    let client_auth = refresh::client_auth_from_request(
        &req,
        params.client_id.as_deref(),
        params.client_secret.as_deref(),
    );
    match token_inner(
        params,
        &client_auth,
        cfg.as_ref(),
        db.as_ref(),
        issuer.as_ref(),
        refresh_pool.get_ref(),
    )
    .await
    {
        Ok(resp) => token_json_response(resp),
        Err(err) => oauth_error_response(err),
    }
}

#[allow(clippy::future_not_send)]
async fn token_inner(
    params: TokenRequest,
    client_auth: &ClientAuth,
    cfg: &AuthConfig,
    db: &Client,
    issuer: &Issuer,
    refresh_pool: &RefreshSessionPool,
) -> Result<TokenResponse, OAuthError> {
    let result = match params.grant_type.as_str() {
        "authorization_code" => {
            let client_id = required_param(params.client_id.as_deref(), "client_id")?;
            let redirect_uri = required_param(params.redirect_uri.as_deref(), "redirect_uri")?;
            let code = required_param(params.code.as_deref(), "code")?;
            let code_verifier = required_param(params.code_verifier.as_deref(), "code_verifier")?;
            let client = load_client(db, client_id).await?;
            authenticate_authorization_code_client(issuer, &client, client_auth)?;
            let pool = refresh_pool
                .checkout_pool("token authorization_code")
                .await
                .map_err(|err| {
                    tracing::error!(error = %err, "token: dedicated database pool checkout failed");
                    OAuthError::server_error("token database unavailable")
                })?;
            let mut conn = pool.get().await.map_err(|err| {
                tracing::error!(error = %err, "token: dedicated database session checkout failed");
                OAuthError::server_error("token database unavailable")
            })?;
            let tx = conn.transaction().await.map_err(|err| {
                tracing::error!(error = %err, "token: BEGIN failed on dedicated session");
                OAuthError::server_error("token transaction unavailable")
            })?;
            let result = exchange_authorization_code(
                &tx,
                issuer,
                cfg,
                &client,
                redirect_uri,
                code,
                code_verifier,
            )
            .await;
            match result {
                Ok(response) => {
                    tx.commit().await.map_err(|err| {
                        tracing::error!(error = %err, "token: COMMIT failed");
                        OAuthError::server_error("token transaction failed")
                    })?;
                    Ok(response)
                }
                Err(err) => {
                    if let Err(rollback) = tx.rollback().await {
                        tracing::error!(error = %rollback, "token: ROLLBACK failed");
                    }
                    Err(err)
                }
            }
        }
        "refresh_token" => {
            let keys = RefreshTokenKeys::from_config(cfg)?;
            refresh::exchange_refresh_token(db, refresh_pool, issuer, &keys, &params, client_auth)
                .await
        }
        _ => Err(OAuthError::unsupported_grant_type(
            "grant_type is not supported",
        )),
    };
    result
}

#[allow(clippy::future_not_send)]
async fn exchange_authorization_code(
    db: &(impl GenericClient + ?Sized),
    issuer: &Issuer,
    cfg: &AuthConfig,
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
                       granted_scopes, nonce, user_id, auth_credential_version, sid",
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
        auth_credential_version: row.get("auth_credential_version"),
        sid: row.get("sid"),
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
    let access_token = mint_access_token(issuer, client, consumed.user_id, &consumed.granted_scopes)?;

    let id_token = if consumed.granted_scopes.iter().any(|scope| scope == "openid") {
        let nonce = consumed
            .nonce
            .as_deref()
            .ok_or_else(|| OAuthError::invalid_grant("openid code is missing nonce"))?;
        let wants_identity_claims = consumed
            .granted_scopes
            .iter()
            .any(|scope| scope == "email" || scope == "profile");
        // Only pay the user SELECT when a granted scope actually carries identity
        // claims. A bare-`openid` (authentication-only) exchange derives `sub`
        // from the already-in-hand `user_id`, so it needs no row.
        let identity_claims = if wants_identity_claims {
            let user = users::find_by_id(db, &user_id)
                .await
                .map_err(|err| {
                    tracing::error!(
                        error = %err,
                        user_id = %user_id,
                        "token: id-token user lookup failed"
                    );
                    OAuthError::server_error("id token user lookup failed")
                })?
                .ok_or_else(|| {
                    tracing::error!(user_id = %user_id, "token: consumed code user is missing");
                    OAuthError::server_error("id token user missing")
                })?;
            Some(scope_gated_identity_claims(
                &user,
                consumed.granted_scopes.iter().map(String::as_str),
            ))
        } else {
            None
        };
        let token = if client.brokered {
            issuer.issue_principal_id_token(&PrincipalIdTokenMint {
                principal_id: &user_id,
                client_id: &client.client_id,
                sid: &consumed.sid,
                nonce,
                access_token: &access_token,
                auth_time: None,
                amr: None,
                acr: None,
                email: identity_claims.as_ref().and_then(|claims| claims.email.as_deref()),
                email_verified: identity_claims
                    .as_ref()
                    .and_then(|claims| claims.email_verified),
                name: identity_claims.as_ref().and_then(|claims| claims.name.as_deref()),
                picture: identity_claims.as_ref().and_then(|claims| claims.picture.as_deref()),
                ttl_secs: Some(ACCESS_TOKEN_TTL_SECS),
            })
        } else {
            issuer.issue_id_token(&IdTokenMint {
                user_id: &user_id,
                sector: &client.sector_identifier,
                client_id: &client.client_id,
                sid: &consumed.sid,
                nonce,
                access_token: &access_token,
                auth_time: None,
                amr: None,
                acr: None,
                email: identity_claims.as_ref().and_then(|claims| claims.email.as_deref()),
                email_verified: identity_claims
                    .as_ref()
                    .and_then(|claims| claims.email_verified),
                name: identity_claims.as_ref().and_then(|claims| claims.name.as_deref()),
                picture: identity_claims.as_ref().and_then(|claims| claims.picture.as_deref()),
                ttl_secs: Some(ACCESS_TOKEN_TTL_SECS),
            })
        }
        .map_err(|err| {
            tracing::error!(
                error = %err,
                brokered = client.brokered,
                "token: id-token mint failed"
            );
            OAuthError::server_error("id token mint failed")
        })?;
        Some(token)
    } else {
        None
    };

    if id_token.is_some() {
        let subject = if client.brokered {
            user_id.clone()
        } else {
            issuer.pairwise_subject(&user_id, &client.sector_identifier)
        };
        backchannel_logout::record_rp_participation(
            db,
            consumed.user_id,
            &consumed.sid,
            &client.client_id,
            &subject,
            client.backchannel_logout_uri.as_deref(),
        )
        .await
        .map_err(|err| {
            tracing::error!(
                error = %err,
                client_id = %client.client_id,
                sid = %consumed.sid,
                "token: BCL RP participation record failed"
            );
            OAuthError::server_error("session participation store unavailable")
        })?;
    }

    let refresh_token =
        if consumed.granted_scopes.iter().any(|scope| scope == "offline_access")
            && client.refresh_allowed
        {
            let keys = RefreshTokenKeys::from_config(cfg)?;
            Some(
                refresh::issue_root_refresh_token(
                    db,
                    issuer,
                    &keys,
                    client,
                    consumed.user_id,
                    &consumed.granted_scopes,
                    consumed.auth_credential_version,
                )
                .await?,
            )
        } else {
            None
        };

    Ok(TokenResponse {
        access_token,
        id_token,
        refresh_token,
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

pub(super) async fn load_client(
    db: &(impl GenericClient + ?Sized),
    client_id: &str,
) -> Result<OAuthClient, OAuthError> {
    let rows = db
        .query(
            "SELECT oc.client_id, oc.redirect_uris, oc.scopes, \
                    oc.client_secret_hash, oc.refresh_allowed, oc.token_endpoint_auth_method, \
                    oc.brokered, oc.backchannel_logout_uri, \
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
    // A security-gating flag (global-vs-pairwise subject) must never be
    // *guessed* on a decode error — fail closed to server_error, don't default.
    let brokered = row.try_get::<_, bool>("brokered").map_err(|err| {
        tracing::error!(error = %err, client_id = %client_id, "brokered flag decode failed");
        OAuthError::server_error("client registry unavailable")
    })?;
    let token_endpoint_auth_method = row
        .try_get("token_endpoint_auth_method")
        .unwrap_or_else(|_| "none".to_string());
    // A2(ii): the load_client backstop of the "brokered ⇒ confidential auth"
    // invariant (the DB CHECK is A2(i)). A brokered client that is somehow not
    // client_secret_basic would let an app exchange a code without the broker
    // secret and receive the global-subject id_token — fail closed.
    if brokered && token_endpoint_auth_method != "client_secret_basic" {
        tracing::error!(
            client_id = %client_id,
            token_endpoint_auth_method = %token_endpoint_auth_method,
            "brokered client is not client_secret_basic — refusing (invariant violation)"
        );
        return Err(OAuthError::server_error("client misconfigured"));
    }
    Ok(OAuthClient {
        client_id: row.get("client_id"),
        redirect_uris: row.get("redirect_uris"),
        scopes: sort_dedup(row.get("scopes")),
        app_id: row.try_get("app_id").ok().flatten(),
        sector_identifier,
        client_secret_hash: row.try_get("client_secret_hash").ok().flatten(),
        refresh_allowed: row.try_get("refresh_allowed").unwrap_or(false),
        token_endpoint_auth_method,
        backchannel_logout_uri: row.try_get("backchannel_logout_uri").ok().flatten(),
        brokered,
    })
}

pub(crate) async fn persist_consent_grant(
    db: &Client,
    user_id: Uuid,
    client_id: &str,
    requested_scopes: &[String],
) -> Result<Vec<String>, String> {
    let existing = db
        .query(
            "SELECT granted_scopes \
             FROM zeroship.oauth_grants \
             WHERE user_id = $1 AND client_id = $2",
            &[&user_id, &client_id],
        )
        .await
        .map_err(|err| {
            format!("oauth grant lookup failed: {err}")
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
        format!("oauth grant upsert failed: {err}")
    })?;
    Ok(granted)
}

async fn consent_covers(
    db: &(impl GenericClient + ?Sized),
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

async fn touch_consent_grant(
    db: &Client,
    user_id: Uuid,
    client_id: &str,
) -> Result<(), OAuthError> {
    db.execute(
        "UPDATE zeroship.oauth_grants \
         SET last_used_at = NOW() \
         WHERE user_id = $1 AND client_id = $2",
        &[&user_id, &client_id],
    )
    .await
    .map_err(|err| {
        tracing::error!(error = %err, "authorize: oauth grant touch failed");
        OAuthError::server_error("consent store unavailable")
    })?;
    Ok(())
}

pub(super) fn required_param<'a>(value: Option<&'a str>, name: &'static str) -> Result<&'a str, OAuthError> {
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

pub(super) fn parse_scopes(scope: &str) -> Vec<String> {
    sort_dedup(
        scope
            .split_ascii_whitespace()
            .filter(|scope| !scope.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

pub(super) fn sort_dedup(mut scopes: Vec<String>) -> Vec<String> {
    scopes.sort();
    scopes.dedup();
    scopes
}

pub(super) fn scope_subset(requested: &[String], allowed: &[String]) -> bool {
    let allowed = sort_dedup(allowed.to_vec());
    requested
        .iter()
        .all(|scope| allowed.binary_search(scope).is_ok())
}

pub(super) fn clean_optional(value: Option<String>) -> Option<String> {
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
    let location = return_to::login_location(&return_to::request_target(req));
    see_other(&location)
        .header("cache-control", "no-store")
        .finish()
}

fn consent_redirect(req: &HttpRequest) -> HttpResponse {
    let location = return_to::consent_location(&return_to::request_target(req));
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

pub(super) fn token_json_response(body: TokenResponse) -> HttpResponse {
    HttpResponse::Ok()
        .header("cache-control", "no-store")
        .header("pragma", "no-cache")
        .json(&body)
}

pub(super) fn oauth_error_response(err: OAuthError) -> HttpResponse {
    HttpResponse::build(err.status)
        .header("cache-control", "no-store")
        .header("pragma", "no-cache")
        .json(&json!({
            "error": err.error,
            "error_description": err.description,
        }))
}

fn authenticate_authorization_code_client(
    issuer: &Issuer,
    client: &OAuthClient,
    client_auth: &ClientAuth,
) -> Result<(), OAuthError> {
    // The authorization_code grant authenticates ONLY brokered clients (public
    // `oac_` clients are PKCE-only). Non-brokered → no secret check here.
    if !client.brokered {
        return Ok(());
    }
    authenticate_brokered_client(issuer, client, client_auth)
}

/// Confidential broker-secret authentication for a brokered client, shared by
/// the authorization_code grant AND the refresh grant (`authenticate_for_refresh`).
/// A brokered client MUST present the per-app broker secret (HKDF-derived from
/// the platform master, verified by derive-and-compare); this is the control
/// that keeps the global-subject id_token out of app-controlled code.
pub(super) fn authenticate_brokered_client(
    issuer: &Issuer,
    client: &OAuthClient,
    client_auth: &ClientAuth,
) -> Result<(), OAuthError> {
    if !matches!(client_auth.method, ClientAuthMethod::Basic | ClientAuthMethod::Post) {
        return Err(OAuthError::invalid_client(
            "broker client authentication required",
        ));
    }
    let Some(auth_client_id) = client_auth.client_id.as_deref() else {
        return Err(OAuthError::invalid_client(
            "client authentication is missing client_id",
        ));
    };
    if auth_client_id != client.client_id {
        return Err(OAuthError::invalid_client("client authentication mismatch"));
    }
    let Some(secret) = client_auth.client_secret.as_deref() else {
        return Err(OAuthError::invalid_client(
            "broker client secret is required",
        ));
    };
    let ok = issuer.verify_broker_secret(&client.client_id, secret).map_err(|err| {
        tracing::error!(
            error = %err,
            client_id = %client.client_id,
            "broker secret verification unavailable"
        );
        OAuthError::server_error("broker secret unavailable")
    })?;
    if ok {
        Ok(())
    } else {
        Err(OAuthError::invalid_client(
            "broker client authentication failed",
        ))
    }
}

pub(super) fn mint_access_token(
    issuer: &Issuer,
    client: &OAuthClient,
    user_id: Uuid,
    scopes: &[String],
) -> Result<String, OAuthError> {
    let user_id = user_id.to_string();
    let audience = client.resource_audience();
    issuer
        .issue_access_token(&AccessTokenMint {
            user_id: &user_id,
            sector: &client.sector_identifier,
            audience: &audience,
            client_id: &client.client_id,
            scopes,
            ttl_secs: Some(ACCESS_TOKEN_TTL_SECS),
        })
        .map_err(|err| {
            tracing::error!(error = %err, "token: access-token mint failed");
            OAuthError::server_error("access token mint failed")
        })
}
