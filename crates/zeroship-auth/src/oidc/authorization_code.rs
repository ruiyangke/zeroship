//! Closed-world OAuth2/OIDC authorization-code + PKCE endpoints.

use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use compio_postgres::{Client, GenericClient, Transaction};
use ntex::http::StatusCode;
use ntex::http::header::{COOKIE, HeaderValue, LOCATION, WWW_AUTHENTICATE};
use ntex::web::{self, HttpRequest, HttpResponse};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::config::AuthConfig;
use crate::oidc::auth_request::{AuthRequest, AuthRequestError};
use crate::oidc::backchannel_logout;
use crate::oidc::claims::scope_gated_identity_claims;
use crate::oidc::device_token;
use crate::oidc::refresh::{self, ClientAuth, ClientAuthMethod, RefreshSessionPool};
use crate::oidc::{
    ACCESS_TOKEN_TTL_SECS, AccessTokenMint, IdTokenMint, Issuer, PrincipalIdTokenMint,
};
use crate::return_to;
use crate::session_store::{SessionKind, ValidatedSession};
use crate::sessions::login as login_session;
use crate::store::sessions as idp_sessions;
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
    .service(
        web::resource(zeroship_core::device_grant::DEVICE_AUTHORIZATION_PATH)
            .route(web::post().to(device_token::device_authorization)),
    )
    .service(
        web::resource(zeroship_core::device_grant::TOKEN_PATH).route(web::post().to(token_post)),
    );
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
    pub prompt: Option<String>,
    pub idp_hint: Option<String>,
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
    pub device_code: Option<String>,
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

enum AuthorizationCodeExchange {
    Token(TokenResponse),
    InvalidGrantAfterCommit,
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

#[derive(Debug, Clone, Copy, Default)]
struct PromptValues {
    none: bool,
    login: bool,
    consent: bool,
    select_account: bool,
    invalid_none_combo: bool,
}

impl PromptValues {
    fn parse(raw: Option<&str>) -> Self {
        let mut values = Self::default();
        let mut none_values = 0usize;
        let mut non_none_values = 0usize;

        for value in raw
            .unwrap_or("")
            .split_ascii_whitespace()
            .filter(|value| !value.is_empty())
        {
            match value {
                "none" => {
                    values.none = true;
                    none_values += 1;
                }
                "login" => {
                    values.login = true;
                    non_none_values += 1;
                }
                "consent" => {
                    values.consent = true;
                    non_none_values += 1;
                }
                "select_account" => {
                    values.select_account = true;
                    non_none_values += 1;
                }
                _ => {
                    non_none_values += 1;
                }
            }
        }

        values.invalid_none_combo = none_values > 0 && non_none_values > 0;
        values
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
    authorize(
        req,
        query.into_inner(),
        cfg.as_ref(),
        db.as_ref(),
        issuer.as_ref(),
    )
    .await
}

#[allow(clippy::future_not_send)]
pub async fn authorize_post(
    req: HttpRequest,
    form: web::types::Form<AuthorizeRequest>,
    cfg: web::types::State<Arc<AuthConfig>>,
    db: web::types::State<Arc<Client>>,
    issuer: web::types::State<Arc<Issuer>>,
) -> HttpResponse {
    authorize(
        req,
        form.into_inner(),
        cfg.as_ref(),
        db.as_ref(),
        issuer.as_ref(),
    )
    .await
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
        return Err(OAuthError::invalid_request(
            "redirect_uri is not registered",
        ));
    }

    let return_to = return_to::request_target(req);
    let auth_request = AuthRequest::from_parts(
        &return_to,
        Some(client_id),
        Some(redirect_uri),
        params.scope.as_deref(),
        params.state.as_deref(),
        params.nonce.as_deref(),
        params.prompt.as_deref(),
        params.idp_hint.as_deref(),
    )
    .map_err(auth_request_oauth_error)?;

    let requested_scopes = auth_request.scopes.clone();
    let prompt = PromptValues::parse(auth_request.prompt.as_deref());
    if prompt.invalid_none_combo {
        return prompt_none_error_see_other(&auth_request, issuer, "invalid_request");
    }

    if !scope_subset(&requested_scopes, &client.scopes) {
        if prompt.none {
            return prompt_none_error_see_other(&auth_request, issuer, "invalid_scope");
        }
        return authorization_error_see_other(&auth_request, issuer, "invalid_scope");
    }

    // C1: PKCE is mandatory for every client and S256 is the only method.
    let code_challenge = match required_param(params.code_challenge.as_deref(), "code_challenge") {
        Ok(code_challenge) => code_challenge,
        Err(err) => {
            if prompt.none {
                return prompt_none_error_see_other(&auth_request, issuer, "invalid_request");
            }
            return authorization_error_see_other(&auth_request, issuer, err.error);
        }
    };
    if !zeroship_core::pkce::is_valid_s256_challenge(code_challenge) {
        if prompt.none {
            return prompt_none_error_see_other(&auth_request, issuer, "invalid_request");
        }
        return authorization_error_see_other(&auth_request, issuer, "invalid_request");
    }
    if let Err(err) = require_eq(
        params.code_challenge_method.as_deref(),
        PKCE_METHOD_S256,
        || OAuthError::invalid_request("code_challenge_method must be S256"),
    ) {
        if prompt.none {
            return prompt_none_error_see_other(&auth_request, issuer, "invalid_request");
        }
        return authorization_error_see_other(&auth_request, issuer, err.error);
    }

    let nonce = auth_request.nonce.clone();
    if requested_scopes.iter().any(|scope| scope == "openid") && nonce.is_none() {
        if prompt.none {
            return prompt_none_error_see_other(&auth_request, issuer, "invalid_request");
        }
        return authorization_error_see_other(&auth_request, issuer, "invalid_request");
    }

    if prompt.none {
        let session = match resolve_session(req, cfg, db).await {
            Ok(Some(session)) => session,
            Ok(None) => {
                let redirect = authorization_error_redirect(
                    &auth_request.redirect_uri,
                    "login_required",
                    auth_request.state.as_deref(),
                    issuer.issuer(),
                )?;
                return Ok(error_see_other(&redirect));
            }
            Err(_) => {
                let redirect = authorization_error_redirect(
                    &auth_request.redirect_uri,
                    "interaction_required",
                    auth_request.state.as_deref(),
                    issuer.issuer(),
                )?;
                return Ok(error_see_other(&redirect));
            }
        };
        let consent_covers =
            match consent_covers(db, session.user_id, &client.client_id, &requested_scopes).await {
                Ok(consent_covers) => consent_covers,
                Err(_) => {
                    return prompt_none_error_see_other(
                        &auth_request,
                        issuer,
                        "interaction_required",
                    );
                }
            };
        if !consent_covers {
            let redirect = authorization_error_redirect(
                &auth_request.redirect_uri,
                "consent_required",
                auth_request.state.as_deref(),
                issuer.issuer(),
            )?;
            return Ok(error_see_other(&redirect));
        }
        if touch_consent_grant(db, session.user_id, &client.client_id)
            .await
            .is_err()
        {
            return prompt_none_error_see_other(&auth_request, issuer, "interaction_required");
        }
        return match issue_authorization_code(
            db,
            issuer,
            &client,
            &auth_request,
            &session,
            code_challenge,
        )
        .await
        {
            Ok(resp) => Ok(resp),
            Err(_) => prompt_none_error_see_other(&auth_request, issuer, "interaction_required"),
        };
    }

    if prompt.login || prompt.select_account {
        return Ok(login_redirect(req, &auth_request, cfg));
    }

    let Some(session) = resolve_session(req, cfg, db).await? else {
        return Ok(login_redirect(req, &auth_request, cfg));
    };

    if prompt.consent {
        return Ok(consent_redirect(req));
    }

    if consent_covers(db, session.user_id, &client.client_id, &requested_scopes).await? {
        touch_consent_grant(db, session.user_id, &client.client_id).await?;
    } else {
        return Ok(consent_redirect(req));
    }

    issue_authorization_code(db, issuer, &client, &auth_request, &session, code_challenge).await
}

#[allow(clippy::future_not_send)]
async fn issue_authorization_code(
    db: &Client,
    issuer: &Issuer,
    client: &OAuthClient,
    auth_request: &AuthRequest,
    session: &idp_sessions::Session,
    code_challenge: &str,
) -> Result<HttpResponse, OAuthError> {
    let requested_scopes = auth_request.scopes.clone();
    let granted_scopes = requested_scopes.clone();
    let nonce = auth_request.nonce.clone();
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
            &auth_request.redirect_uri,
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
        &auth_request.redirect_uri,
        &code,
        auth_request.state.as_deref(),
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
        Err(err) => client_auth_error_response(err),
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
            // A code plus its verifier is not a credential: both travel through
            // the user agent and can leak (Referer, proxy logs). A client that
            // was issued a secret authenticates here exactly as it does on
            // refresh; public clients stay PKCE-only.
            refresh::authenticate_client(issuer, &client, client_auth)?;
            let pool = refresh_pool
                .checkout_pool("token authorization_code")
                .await
                .map_err(|err| {
                    tracing::error!(error = %err, "token: dedicated database pool checkout failed");
                    OAuthError::server_error("token database unavailable")
                })?;
            let mut conn = pool.acquire().await.map_err(|err| {
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
                Ok(AuthorizationCodeExchange::Token(response)) => {
                    tx.commit().await.map_err(|err| {
                        tracing::error!(error = %err, "token: COMMIT failed");
                        OAuthError::server_error("token transaction failed")
                    })?;
                    Ok(response)
                }
                Ok(AuthorizationCodeExchange::InvalidGrantAfterCommit) => {
                    tx.commit().await.map_err(|err| {
                        tracing::error!(error = %err, "token: COMMIT failed after code replay revocation");
                        OAuthError::server_error("token transaction failed")
                    })?;
                    Err(OAuthError::invalid_grant("authorization code is invalid"))
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
            let keys = refresh::session_keys(cfg)?;
            refresh::exchange_refresh_token(
                db,
                refresh_pool,
                cfg,
                issuer,
                &keys,
                &params,
                client_auth,
            )
            .await
        }
        device_token::DEVICE_CODE_GRANT_TYPE => {
            device_token::exchange_device_code(&params, client_auth, cfg, db, issuer, refresh_pool)
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
    db: &Transaction<'_>,
    issuer: &Issuer,
    cfg: &AuthConfig,
    client: &OAuthClient,
    redirect_uri: &str,
    code: &str,
    code_verifier: &str,
) -> Result<AuthorizationCodeExchange, OAuthError> {
    let code_hash = code_hash(code);
    let rows = db
        .query(
            "UPDATE zeroship.oauth_authorization_codes AS code \
             SET consumed_at = NOW() \
             FROM zeroship.users AS owner \
             WHERE code.code_hash = $1 \
               AND code.consumed_at IS NULL \
               AND code.expires_at > NOW() \
               AND owner.id = code.user_id \
               AND owner.credential_version = code.auth_credential_version \
               AND owner.disabled_at IS NULL \
               AND owner.deletion_requested_at IS NULL \
               AND owner.deletion_scheduled_for IS NULL \
               AND owner.anonymized_at IS NULL \
             RETURNING code.client_id, code.redirect_uri, code.pkce_challenge, code.pkce_method, \
                       code.granted_scopes, code.nonce, code.user_id, \
                       code.auth_credential_version, code.sid",
            &[&code_hash],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "token: consume authorization code failed");
            OAuthError::server_error("authorization code store unavailable")
        })?;
    let Some(row) = rows.first() else {
        match revoke_replayed_authorization_code_lineage(db, issuer, &code_hash).await {
            Ok(true) => return Ok(AuthorizationCodeExchange::InvalidGrantAfterCommit),
            Ok(false) => {}
            Err(err) => {
                tracing::error!(
                    error = %err.description,
                    oauth_error = %err.error,
                    "token: authorization code replay revocation failed"
                );
            }
        }
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
        return Err(OAuthError::invalid_grant(
            "authorization code binding mismatch",
        ));
    }
    if consumed.pkce_method != PKCE_METHOD_S256
        || !zeroship_core::pkce::verify_s256(code_verifier, &consumed.pkce_challenge)
    {
        return Err(OAuthError::invalid_grant("pkce verification failed"));
    }
    if !consent_covers(
        db,
        consumed.user_id,
        &client.client_id,
        &consumed.granted_scopes,
    )
    .await?
    {
        return Err(OAuthError::invalid_grant("consent no longer covers grant"));
    }

    // Keep the global lock order refresh-user -> signing-key. Refresh exchange
    // already holds the user lock when it advances the signing-key watermark.
    //
    // THE SESSION IS ESTABLISHED BEFORE ANYTHING IS MINTED, and it is
    // established on EVERY exchange rather than only when `offline_access` was
    // granted. That is what makes this a MINT-READS-ROW path: the access token
    // and the ID token below are both minted from the proof the creating
    // statement returned, so an exchange whose person went inactive between the
    // authorization and the redemption mints nothing. Only the SECRET is
    // conditional - a session created without one can never be presented again.
    let established = refresh::establish_session(
        db,
        issuer,
        &refresh::session_keys(cfg)?,
        &refresh::Establish {
            client,
            user_id: consumed.user_id,
            granted_scopes: &consumed.granted_scopes,
            auth_credential_version: consumed.auth_credential_version,
            kind: SessionKind::Browser,
            with_secret: consumed
                .granted_scopes
                .iter()
                .any(|scope| scope == "offline_access")
                && client.refresh_allowed,
        },
    )
    .await?;
    let refresh_token = established.secret;
    let proof = &established.proof;

    let user_id = consumed.user_id.to_string();
    let access_token = mint_access_token(
        db,
        issuer,
        client,
        consumed.user_id,
        &consumed.granted_scopes,
        proof,
    )
    .await?;

    let id_token = if consumed
        .granted_scopes
        .iter()
        .any(|scope| scope == "openid")
    {
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
            issuer
                .issue_principal_id_token(
                    db,
                    &PrincipalIdTokenMint {
                        principal_id: &user_id,
                        client_id: &client.client_id,
                        sid: &consumed.sid,
                        nonce,
                        access_token: &access_token,
                        auth_time: None,
                        amr: None,
                        acr: None,
                        email: identity_claims
                            .as_ref()
                            .and_then(|claims| claims.email.as_deref()),
                        email_verified: identity_claims
                            .as_ref()
                            .and_then(|claims| claims.email_verified),
                        name: identity_claims
                            .as_ref()
                            .and_then(|claims| claims.name.as_deref()),
                        picture: identity_claims
                            .as_ref()
                            .and_then(|claims| claims.picture.as_deref()),
                        ttl_secs: Some(ACCESS_TOKEN_TTL_SECS),
                    },
                    proof,
                )
                .await
        } else {
            issuer
                .issue_id_token(
                    db,
                    &IdTokenMint {
                        user_id: &user_id,
                        sector: &client.sector_identifier,
                        client_id: &client.client_id,
                        sid: &consumed.sid,
                        nonce,
                        access_token: &access_token,
                        auth_time: None,
                        amr: None,
                        acr: None,
                        email: identity_claims
                            .as_ref()
                            .and_then(|claims| claims.email.as_deref()),
                        email_verified: identity_claims
                            .as_ref()
                            .and_then(|claims| claims.email_verified),
                        name: identity_claims
                            .as_ref()
                            .and_then(|claims| claims.name.as_deref()),
                        picture: identity_claims
                            .as_ref()
                            .and_then(|claims| claims.picture.as_deref()),
                        ttl_secs: Some(ACCESS_TOKEN_TTL_SECS),
                    },
                    proof,
                )
                .await
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

    Ok(AuthorizationCodeExchange::Token(TokenResponse {
        access_token,
        id_token,
        refresh_token,
        token_type: TOKEN_TYPE_BEARER,
        expires_in: ACCESS_TOKEN_TTL_SECS as u64,
        scope: consumed.granted_scopes.join(" "),
    }))
}

async fn revoke_replayed_authorization_code_lineage(
    db: &(impl GenericClient + ?Sized),
    issuer: &Issuer,
    code_hash: &[u8],
) -> Result<bool, OAuthError> {
    let rows = db
        .query(
            "SELECT ac.client_id, ac.user_id, \
                    COALESCE(aoc.sector_identifier, ac.client_id) AS sector_identifier \
             FROM zeroship.oauth_authorization_codes ac \
             LEFT JOIN zeroship.app_oauth_clients aoc ON aoc.client_id = ac.client_id \
             WHERE ac.code_hash = $1 \
               AND ac.consumed_at IS NOT NULL",
            &[&code_hash],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "token: authorization code replay lookup failed");
            OAuthError::server_error("authorization code store unavailable")
        })?;
    let Some(row) = rows.first() else {
        return Ok(false);
    };
    let client_id: String = row.get("client_id");
    let user_id: Uuid = row.get("user_id");
    let sector_identifier: String = row.get("sector_identifier");
    let sub = issuer.pairwise_subject(&user_id.to_string(), &sector_identifier);
    refresh::revoke_sessions_for_subject_in_transaction(db, &client_id, &sub).await?;
    Ok(true)
}

async fn resolve_session(
    req: &HttpRequest,
    _cfg: &AuthConfig,
    db: &Client,
) -> Result<Option<idp_sessions::Session>, OAuthError> {
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let Some(session_id) = login_session::parse_cookie(cookie_header) else {
        return Ok(None);
    };
    idp_sessions::validate(db, session_id).await.map_err(|err| {
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
    // Both of the next two reads decide whether this client must present a
    // secret, so neither may fall back on a decode error: a swallowed failure
    // would silently reclassify a confidential client as public and skip
    // authentication altogether. Fail closed to server_error instead.
    let token_endpoint_auth_method = row
        .try_get::<_, String>("token_endpoint_auth_method")
        .map_err(|err| {
            tracing::error!(
                error = %err,
                client_id = %client_id,
                "token_endpoint_auth_method decode failed"
            );
            OAuthError::server_error("client registry unavailable")
        })?;
    let client_secret_hash = row
        .try_get::<_, Option<String>>("client_secret_hash")
        .map_err(|err| {
            tracing::error!(
                error = %err,
                client_id = %client_id,
                "client_secret_hash decode failed"
            );
            OAuthError::server_error("client registry unavailable")
        })?;
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
        client_secret_hash,
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
        .map_err(|err| format!("oauth grant lookup failed: {err}"))?;
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
    .map_err(|err| format!("oauth grant upsert failed: {err}"))?;
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

pub(super) fn required_param<'a>(
    value: Option<&'a str>,
    name: &'static str,
) -> Result<&'a str, OAuthError> {
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
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
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

fn authorization_error_redirect(
    redirect_uri: &str,
    error: &str,
    state: Option<&str>,
    issuer: &str,
) -> Result<String, OAuthError> {
    authorization_error_redirect_location(redirect_uri, error, state, issuer)
        .map_err(|_| OAuthError::invalid_request("redirect_uri is not a valid URL"))
}

pub(crate) fn authorization_error_redirect_location(
    redirect_uri: &str,
    error: &str,
    state: Option<&str>,
    issuer: &str,
) -> Result<String, url::ParseError> {
    let mut url = url::Url::parse(redirect_uri)?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("error", error);
        if let Some(state) = state.filter(|state| !state.is_empty()) {
            query.append_pair("state", state);
        }
        query.append_pair("iss", issuer);
    }
    Ok(url.to_string())
}

fn authorization_error_see_other(
    auth_request: &AuthRequest,
    issuer: &Issuer,
    error: &str,
) -> Result<HttpResponse, OAuthError> {
    let redirect = authorization_error_redirect(
        &auth_request.redirect_uri,
        error,
        auth_request.state.as_deref(),
        issuer.issuer(),
    )?;
    Ok(error_see_other(&redirect))
}

fn prompt_none_error_see_other(
    auth_request: &AuthRequest,
    issuer: &Issuer,
    error: &str,
) -> Result<HttpResponse, OAuthError> {
    authorization_error_see_other(auth_request, issuer, error)
}

fn error_see_other(location: &str) -> HttpResponse {
    see_other(location)
        .header("cache-control", "no-store")
        .header("referrer-policy", "no-referrer")
        .finish()
}

pub(crate) fn prompt_requests_login(prompt: Option<&str>) -> bool {
    let prompt = PromptValues::parse(prompt);
    prompt.login || prompt.select_account
}

pub(crate) fn return_to_after_prompt_interaction(return_to: &str, satisfied: &[&str]) -> String {
    let Some(return_to) = return_to::valid_path(return_to) else {
        return return_to.to_string();
    };
    let Ok(parsed) = url::Url::parse(&format!("http://zeroship.local{return_to}")) else {
        return return_to.to_string();
    };

    let mut prompt_tokens = Vec::new();
    let mut query_pairs = Vec::new();
    for (key, value) in parsed.query_pairs() {
        if key == "prompt" {
            prompt_tokens.extend(value.split_ascii_whitespace().map(str::to_string));
        } else {
            query_pairs.push((key.into_owned(), value.into_owned()));
        }
    }

    if prompt_tokens.is_empty() {
        return return_to.to_string();
    }

    prompt_tokens.retain(|value| !satisfied.contains(&value.as_str()));
    if !prompt_tokens.is_empty() {
        query_pairs.push(("prompt".to_string(), prompt_tokens.join(" ")));
    }

    let mut out = parsed.path().to_string();
    if !query_pairs.is_empty() {
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        for (key, value) in query_pairs {
            serializer.append_pair(&key, &value);
        }
        out.push('?');
        out.push_str(&serializer.finish());
    }
    out
}

fn login_redirect(req: &HttpRequest, auth_request: &AuthRequest, cfg: &AuthConfig) -> HttpResponse {
    let location = auth_request
        .provider_start_location(
            cfg.google_client_id().is_some(),
            cfg.github_client_id().is_some(),
        )
        .unwrap_or_else(|| return_to::login_location(&return_to::request_target(req)));
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

/// Error responder for the client-authenticating endpoints (`/oauth2/token`,
/// `/oauth2/revoke`, `/oauth2/introspect`).
///
/// RFC 6749 5.2 answers `invalid_client` with 401 and a challenge matching the
/// scheme the client used; the same error class must not read as 400 on one
/// endpoint and 401 on another. HTTP Basic is the only scheme this OP accepts
/// in the `Authorization` header, so it is the challenge, emitted even when
/// the request carried no credentials, since a bare 401 is not a well-formed
/// response and "authenticate with Basic" is exactly what that caller needs.
/// Every other error class keeps its own status.
pub(super) fn client_auth_error_response(err: OAuthError) -> HttpResponse {
    if err.error != "invalid_client" {
        return oauth_error_response(err);
    }
    HttpResponse::build(StatusCode::UNAUTHORIZED)
        .header("cache-control", "no-store")
        .header("pragma", "no-cache")
        .header(
            WWW_AUTHENTICATE,
            "Basic realm=\"oauth2\", error=\"invalid_client\"",
        )
        .json(&json!({
            "error": err.error,
            "error_description": err.description,
        }))
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

fn auth_request_oauth_error(err: AuthRequestError) -> OAuthError {
    match err {
        AuthRequestError::MissingClientId => OAuthError::invalid_request("client_id"),
        AuthRequestError::MissingRedirectUri => OAuthError::invalid_request("redirect_uri"),
        AuthRequestError::InvalidRedirectUri => {
            OAuthError::invalid_request("redirect_uri is not a valid URL")
        }
        AuthRequestError::ReturnToNotSameOrigin
        | AuthRequestError::ReturnToParse
        | AuthRequestError::WrongPath => OAuthError::invalid_request("invalid authorize request"),
    }
}

/// Confidential broker-secret authentication for a brokered client, shared by
/// the authorization_code grant AND the refresh grant (`authenticate_client`).
/// A brokered client MUST present the per-app broker secret (HKDF-derived from
/// the platform master, verified by derive-and-compare); this is the control
/// that keeps the global-subject id_token out of app-controlled code.
pub(super) fn authenticate_brokered_client(
    issuer: &Issuer,
    client: &OAuthClient,
    client_auth: &ClientAuth,
) -> Result<(), OAuthError> {
    if !matches!(
        client_auth.method,
        ClientAuthMethod::Basic | ClientAuthMethod::Post
    ) {
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
    let ok = issuer
        .verify_broker_secret(&client.client_id, secret)
        .map_err(|err| {
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

const fn access_identity_upsert_sql() -> &'static str {
    "INSERT INTO zeroship.app_user_identities \
        (app_client_id, global_user_id, pairwise_sub) \
     VALUES ($1, $2, $3) \
     ON CONFLICT (app_client_id, global_user_id) DO UPDATE SET \
        pairwise_sub = EXCLUDED.pairwise_sub, \
        revoked_at = NULL \
     WHERE zeroship.app_user_identities.pairwise_sub = EXCLUDED.pairwise_sub"
}

const ACCESS_MINT_PRINCIPAL_ACTIVE_SQL: &str = "SELECT 1 FROM zeroship.users \
     WHERE id = $1 \
       AND disabled_at IS NULL \
       AND anonymized_at IS NULL \
       AND deletion_requested_at IS NULL \
       AND deletion_scheduled_for IS NULL";

pub(super) async fn mint_access_token(
    db: &Transaction<'_>,
    issuer: &Issuer,
    client: &OAuthClient,
    user_id: Uuid,
    scopes: &[String],
    proof: &ValidatedSession,
) -> Result<String, OAuthError> {
    crate::advisory_lock::lock_refresh_user_xact(db, user_id)
        .await
        .map_err(|err| {
            tracing::error!(error = %err, user_id = %user_id, "access-token mint user lock failed");
            OAuthError::server_error("access-token mint unavailable")
        })?;
    let active = db
        .query(ACCESS_MINT_PRINCIPAL_ACTIVE_SQL, &[&user_id])
        .await
        .map_err(|err| {
            tracing::error!(error = %err, user_id = %user_id, "access-token mint lifecycle lookup failed");
            OAuthError::server_error("access-token mint unavailable")
        })?;
    if active.is_empty() {
        return Err(OAuthError::invalid_grant("authenticated user is inactive"));
    }

    let user_id_string = user_id.to_string();
    let pairwise_sub = issuer.pairwise_subject(&user_id_string, &client.sector_identifier);
    db.execute(
        "SELECT set_config('zeroship.tenant_client', $1, true)",
        &[&client.client_id],
    )
    .await
    .map_err(|err| {
        tracing::error!(error = %err, client_id = %client.client_id, "token: pairwise tenant scope failed");
        OAuthError::server_error("pairwise identity store unavailable")
    })?;
    let mapped = db
        .execute(
            access_identity_upsert_sql(),
            &[&client.client_id, &user_id, &pairwise_sub],
        )
        .await
        .map_err(|err| {
            tracing::error!(
                error = %err,
                client_id = %client.client_id,
                user_id = %user_id,
                "token: pairwise identity mapping failed"
            );
            OAuthError::server_error("pairwise identity store unavailable")
        })?;
    if mapped != 1 {
        tracing::error!(
            client_id = %client.client_id,
            user_id = %user_id,
            "token: pairwise identity binding changed"
        );
        return Err(OAuthError::server_error(
            "pairwise identity binding changed",
        ));
    }
    let audience = client.resource_audience();
    issuer
        .issue_access_token(
            db,
            &AccessTokenMint {
                user_id: &user_id_string,
                sector: &client.sector_identifier,
                audience: &audience,
                client_id: &client.client_id,
                scopes,
                ttl_secs: Some(ACCESS_TOKEN_TTL_SECS),
            },
            proof,
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "token: access-token mint failed");
            OAuthError::server_error("access token mint failed")
        })
}

#[cfg(test)]
mod access_identity_tests {
    use std::time::Duration;

    use base64::Engine as _;
    use compio_postgres::{Client, NoTls, connect};

    use super::*;

    async fn pg_connect(dsn: &str) -> Client {
        let (client, connection) = connect(dsn, NoTls).await.expect("connect");
        compio::runtime::spawn(async move {
            let _ = connection.run().await;
        })
        .detach();
        client
    }

    /// A keyring in a private, owner-only directory. These tests need one
    /// because `mint_access_token` takes a `ValidatedSession`, and the only way
    /// to get one is to run the creating statement - which is the property
    /// under test everywhere else in the crate. A test that could fabricate the
    /// witness would be testing nothing.
    fn test_keys(tag: &str) -> crate::session_store::SessionSecretKeys {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("zs-mint-race-keys-{tag}"));
        std::fs::create_dir_all(&dir).expect("key dir");
        let hash_path = dir.join("hash");
        let idem_path = dir.join("idem");
        for (path, body) in [
            (
                &hash_path,
                "1:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff\n".as_bytes(),
            ),
            (&idem_path, "mint-race-idempotency-master-secret".as_bytes()),
        ] {
            let mut file = std::fs::File::create(path).expect("create key file");
            file.write_all(body).expect("write key file");
            drop(file);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                    .expect("chmod key file");
            }
        }
        crate::session_store::SessionSecretKeys::from_files(&hash_path, &idem_path)
            .expect("load session keys")
    }

    /// Establish a session for `user_id` under `client`, and hand back the
    /// proof its creating statement produced.
    async fn proof_for(
        tx: &Transaction<'_>,
        issuer: &Issuer,
        client: &OAuthClient,
        user_id: Uuid,
    ) -> ValidatedSession {
        let tag = Uuid::new_v4().simple().to_string();
        refresh::establish_session(
            tx,
            issuer,
            &test_keys(&tag),
            &refresh::Establish {
                client,
                user_id,
                granted_scopes: &["openid".to_string()],
                auth_credential_version: 0,
                kind: SessionKind::Browser,
                with_secret: false,
            },
        )
        .await
        .expect("establish session")
        .proof
    }

    // A missing Postgres is a FAILURE, not a skip: these fixtures exercise
    // the mint-vs-deletion lock race, and a silently skipped run would let
    // that race go unchecked while the suite still read green. Dial it or
    // panic naming the provisioning command.
    async fn mint_fixture() -> (String, Client, Client, Client, Uuid, OAuthClient, Issuer) {
        let dsn = zeroship_core::config::test_database_url();
        // A DATABASE BEHIND THE MIGRATION LEDGER IS REFUSED, NOT REPORTED AS A
        // CODE REGRESSION. `refresh::establish_session` maps every error the
        // session store raises onto one `server_error("session issuance
        // unavailable")`, so a `zeroship` schema that has never seen
        // `db/migrations-ts/20260907000100_session_object.ts` arrives here as
        // an opaque 500 inside `proof_for`'s `expect`, and the tests below
        // present as named failures naming nothing that is wrong with them.
        // That is the void run `crate::platform_fixture::live_db` exists to remove: it
        // names the database, says how far short its journal is, and prints
        // `deploy/ops/db-migrate.sh`. Asking for the journal schema is what
        // turns that ledger comparison on; the schema list alone would call a
        // behind database ready. Memoised per process, because any of these
        // tests can be the first to reach a database under a filter.
        crate::platform_fixture::live_db::require_once(
            &dsn,
            crate::platform_fixture::live_db::PLATFORM_SCHEMAS,
        );
        let setup = pg_connect(&dsn).await;
        let mint = pg_connect(&dsn).await;
        let deletion = pg_connect(&dsn).await;
        let tag = Uuid::new_v4().simple().to_string();
        let user_id: Uuid = setup
            .query_one(
                "INSERT INTO zeroship.users (email, name, password_hash) \
                 VALUES ($1::citext, 'Mint Race', 'phc') RETURNING id",
                &[&format!("mint-race-{tag}@zeroship.test")],
            )
            .await
            .expect("seed user")
            .get("id");
        let client_id = format!("oac_mint_race_{tag}");
        setup
            .execute(
                "INSERT INTO zeroship.oauth_clients \
                    (client_id, client_name, redirect_uris, scopes) \
                 VALUES ($1, 'Mint Race', $2, $3)",
                &[
                    &client_id,
                    &vec!["https://mint-race.test/callback".to_string()],
                    &vec!["openid".to_string()],
                ],
            )
            .await
            .expect("seed client");
        let client = OAuthClient {
            client_id,
            redirect_uris: vec!["https://mint-race.test/callback".to_string()],
            scopes: vec!["openid".to_string()],
            app_id: None,
            sector_identifier: "https://mint-race.test".to_string(),
            client_secret_hash: None,
            refresh_allowed: false,
            token_endpoint_auth_method: "none".to_string(),
            backchannel_logout_uri: None,
            brokered: false,
        };
        // PER-PROCESS, not the fixed `[61u8; 32]` this used to be.
        //
        // `zeroship.signing_keys` allows one `active` OP key per DATABASE:
        // `publish_active_key` retires every other active row and refuses to
        // reactivate a retired one. A constant seed gives every run the same
        // kid, so two runs sharing a suite database retire each other and the
        // second dies on its own key. MEASURED 2026-08-20, two auth suites on
        // one database: 3 failures in each run, both
        //   publish mint-race signing key: Config("signing key
        //     L0N3gfnVojR3MCyMbPF6lMf6P9ywvEtOlQe2mLgT18c is terminally
        //     retired and cannot be reactivated")
        // naming the same kid in both logs. Same reasoning as
        // `crates/zeroship-auth/tests/common/mod.rs::op_signing_key`, which cannot be
        // reached from here because this is a lib test.
        let issuer = Issuer::from_signing_key(
            &ed25519_dalek::SigningKey::from_bytes(&mint_race_signing_seed()),
            [62_u8; 32],
            "https://auth.mint-race.test".to_string(),
        )
        .expect("issuer");
        // ONCE per process. A fresh kid is not enough on its own: this fixture
        // builds one per test, and a peer run retires our row between calls, so
        // the second REPUBLISH of our own kid fails. Publishing once removes
        // the only operation that can fail; a retired row stays usable, since
        // the JWKS keeps `retiring` keys and lookups here are by kid.
        //
        // LOAD-THEN-STORE, NOT `swap`. The flag must record that a publish
        // SUCCEEDED, not that one was attempted. Written as
        // `if !PUBLISHED.swap(true, ..)` the flag is already set when the
        // `expect` below panics, so the first test reports the true error and
        // every later test in the process silently skips the publish and fails
        // somewhere downstream on a key that was never registered - one real
        // failure wearing three unrelated faces, which is the diagnosis trap
        // this whole file's fixtures exist to avoid.
        //
        // The race the swap was buying is not one worth having: this suite runs
        // `--test-threads 1`, and even threaded the worst case is publishing
        // our own still-ACTIVE kid twice, which succeeds - `publish_active_key`
        // refuses only `retiring` and `retired` rows. Skipping the publish
        // entirely is the outcome that cannot be recovered from.
        {
            use std::sync::atomic::{AtomicBool, Ordering};
            static PUBLISHED: AtomicBool = AtomicBool::new(false);
            if !PUBLISHED.load(Ordering::SeqCst) {
                issuer
                    .publish_active_key(&setup)
                    .await
                    .expect("publish mint-race signing key");
                PUBLISHED.store(true, Ordering::SeqCst);
            }
        }
        (dsn, setup, mint, deletion, user_id, client, issuer)
    }

    /// A signing seed unique to this test process, stable within it.
    ///
    /// The pid separates two live runs; the clock separates a reused pid from
    /// the process that held it before. Stable within the process because an
    /// issuer that re-derived its key would publish a second kid and retire its
    /// own.
    fn mint_race_signing_seed() -> [u8; 32] {
        static SEED: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
        *SEED.get_or_init(|| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            zeroship_core::crypto::derive_key(&format!("mint-race-{}-{nanos}", std::process::id()))
        })
    }

    fn token_iat(token: &str) -> i64 {
        let payload = token.split('.').nth(1).expect("JWT payload");
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .expect("decode JWT payload");
        serde_json::from_slice::<serde_json::Value>(&decoded).expect("parse JWT payload")["iat"]
            .as_i64()
            .expect("iat")
    }

    async fn cleanup_mint_fixture(setup: &Client, user_id: Uuid, client_id: &str) {
        let _ = setup
            .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user_id])
            .await;
        let _ = setup
            .execute(
                "DELETE FROM zeroship.oauth_clients WHERE client_id = $1",
                &[&client_id],
            )
            .await;
    }

    #[test]
    fn access_token_mint_persists_the_pairwise_revocation_mapping() {
        let sql = access_identity_upsert_sql();
        assert!(sql.contains("INSERT INTO zeroship.app_user_identities"));
        assert!(sql.contains("global_user_id"));
        assert!(sql.contains("pairwise_sub"));
        assert!(sql.contains("ON CONFLICT (app_client_id, global_user_id)"));
        assert!(sql.contains("revoked_at = NULL"));
        assert!(
            sql.contains("WHERE zeroship.app_user_identities.pairwise_sub = EXCLUDED.pairwise_sub")
        );
    }

    #[test]
    fn access_token_mint_blocks_hard_lifecycle_without_soft_lockout() {
        for column in [
            "disabled_at",
            "anonymized_at",
            "deletion_requested_at",
            "deletion_scheduled_for",
        ] {
            assert!(
                ACCESS_MINT_PRINCIPAL_ACTIVE_SQL.contains(&format!("{column} IS NULL")),
                "missing active lifecycle predicate for {column}"
            );
        }
        assert!(!ACCESS_MINT_PRINCIPAL_ACTIVE_SQL.contains("locked_until"));
    }

    #[compio::test]
    async fn access_token_mint_holds_the_user_lock_until_commit() {
        let (_dsn, setup, mut mint, _deletion, user_id, client, issuer) = mint_fixture().await;
        let tx = mint.transaction().await.expect("mint transaction");
        let proof = proof_for(&tx, &issuer, &client, user_id).await;
        mint_access_token(
            &tx,
            &issuer,
            &client,
            user_id,
            &["openid".to_string()],
            &proof,
        )
        .await
        .expect("mint token");

        let contender_acquired: bool = setup
            .query_one(
                "SELECT pg_try_advisory_xact_lock($1::INT4, hashtext($2::text))",
                &[&crate::advisory_lock::NS_USER, &user_id.to_string()],
            )
            .await
            .expect("probe mint lock")
            .get(0);
        assert!(
            !contender_acquired,
            "a competing transaction acquired the mint's user lock"
        );

        tx.rollback().await.expect("rollback mint");
        cleanup_mint_fixture(&setup, user_id, &client.client_id).await;
    }

    #[compio::test]
    async fn access_token_mint_rejects_a_deleted_principal_after_locking() {
        let (_dsn, mut setup, mut mint, _deletion, user_id, client, issuer) = mint_fixture().await;
        crate::store::users::request_deletion(&mut setup, user_id, 30)
            .await
            .expect("delete request")
            .expect("user exists");

        let tx = mint.transaction().await.expect("mint transaction");
        // The session cannot even be ESTABLISHED for a deleted principal, so
        // the refusal now arrives one step earlier than it used to - at the
        // creating statement rather than at the mint's own lifecycle check.
        // That is the shape MINT-READS-ROW buys: there is no proof to carry
        // into a mint, so the mint is unreachable rather than merely refused.
        let tag = Uuid::new_v4().simple().to_string();
        let result = refresh::establish_session(
            &tx,
            &issuer,
            &test_keys(&tag),
            &refresh::Establish {
                client: &client,
                user_id,
                granted_scopes: &["openid".to_string()],
                auth_credential_version: 0,
                kind: SessionKind::Browser,
                with_secret: false,
            },
        )
        .await
        .map(|_| ());
        assert!(
            result.is_err(),
            "a session was established for a deleted principal"
        );
        tx.rollback().await.expect("rollback mint");
        cleanup_mint_fixture(&setup, user_id, &client.client_id).await;
    }

    #[compio::test]
    async fn deletion_marker_uses_a_post_lock_timestamp() {
        let (_dsn, setup, mut mint, mut deletion, user_id, client, issuer) = mint_fixture().await;
        let tx = mint.transaction().await.expect("mint transaction");
        crate::advisory_lock::lock_refresh_user_xact(&tx, user_id)
            .await
            .expect("hold mint lock");

        let app_name = format!("mint-race-delete-{}", Uuid::new_v4().simple());
        deletion
            .execute(
                "SELECT set_config('application_name', $1, false)",
                &[&app_name],
            )
            .await
            .expect("name deletion session");
        let deletion_task = compio::runtime::spawn(async move {
            crate::store::users::request_deletion(&mut deletion, user_id, 30).await
        });
        let mut observed_wait = false;
        for _ in 0..200 {
            let waiting = setup
                .query_opt(
                    "SELECT 1 FROM pg_stat_activity \
                     WHERE application_name = $1 \
                       AND wait_event_type = 'Lock' \
                       AND wait_event = 'advisory'",
                    &[&app_name],
                )
                .await
                .expect("inspect deletion wait");
            if waiting.is_some() {
                observed_wait = true;
                break;
            }
            compio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(observed_wait, "deletion never reached the held user lock");
        compio::time::sleep(Duration::from_millis(1100)).await;
        let proof = proof_for(&tx, &issuer, &client, user_id).await;
        let token = mint_access_token(
            &tx,
            &issuer,
            &client,
            user_id,
            &["openid".to_string()],
            &proof,
        )
        .await
        .expect("mint token while deletion waits");
        let iat = token_iat(&token);
        let pairwise = issuer.pairwise_subject(&user_id.to_string(), &client.sector_identifier);
        tx.commit().await.expect("commit mint");
        deletion_task
            .await
            .expect("join deletion")
            .expect("delete request")
            .expect("user exists");

        let marker_is_newer: bool = setup
            .query_one(
                "SELECT revoked_after > \
                        TIMESTAMPTZ 'epoch' + ($3::bigint * INTERVAL '1 second') \
                 FROM zeroship.token_revocations \
                 WHERE client_id = $1 AND sub = $2",
                &[&client.client_id, &pairwise, &iat],
            )
            .await
            .expect("revocation marker")
            .get(0);
        assert!(
            marker_is_newer,
            "a deletion that waited for mint must revoke the token minted while it waited"
        );
        cleanup_mint_fixture(&setup, user_id, &client.client_id).await;
    }
}
