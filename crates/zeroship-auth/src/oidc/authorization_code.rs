//! Closed-world OAuth2/OIDC authorization-code + PKCE endpoints.

use std::sync::Arc;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use compio_postgres::{Client, GenericClient, Transaction};
use ntex::http::header::{HeaderValue, COOKIE, LOCATION, WWW_AUTHENTICATE};
use ntex::http::StatusCode;
use ntex::web::{self, HttpRequest, HttpResponse};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use zeroship_core::{AppId, UserId};

use crate::config::AuthConfig;
use crate::oidc::auth_request::{AuthRequest, AuthRequestError};
use crate::oidc::backchannel_logout;
use crate::oidc::claims::{IdentityProfile, ScopeGatedIdentityClaims};
use crate::oidc::device_token;
use crate::oidc::refresh::{self, ClientAuth, ClientAuthMethod, RefreshSessionPool};
use crate::oidc::{
    AccessTokenMint, IdTokenMint, Issuer, PrincipalIdTokenMint, ACCESS_TOKEN_TTL_SECS,
};
use crate::return_to;
use crate::session_store::{SessionKind, ValidatedSession};
use crate::sessions::login as login_session;
use crate::store::sessions as idp_sessions;

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
    pub app_id: Option<AppId>,
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
    /// The `aud` an access token for this client carries.
    ///
    /// The app arm renders the printed app id, so the string the gateway builds
    /// from the app id it resolved and the string minted here are the same
    /// composition of the same value. Rendering anything else - a decoded uuid,
    /// a bare body - makes the two sides of that equality disagree while both
    /// still compile.
    pub(super) fn resource_audience(&self) -> String {
        self.app_id
            .as_ref()
            .map(|id| format!("app:{}", id.as_str()))
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
    user_id: UserId,
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
        let consent_covers = match consent_covers(
            db,
            &session.user_id,
            &client.client_id,
            &requested_scopes,
        )
        .await
        {
            Ok(consent_covers) => consent_covers,
            Err(_) => {
                return prompt_none_error_see_other(&auth_request, issuer, "interaction_required");
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
        if touch_consent_grant(db, &session.user_id, &client.client_id)
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

    if consent_covers(db, &session.user_id, &client.client_id, &requested_scopes).await? {
        touch_consent_grant(db, &session.user_id, &client.client_id).await?;
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
            &session.user_id.as_str(),
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
        user_id: crate::entity_ids::user_id_with_context(
            row,
            "user_id",
            "authorization code user_id is invalid",
        )
        .map_err(|err| {
            tracing::error!(error = %err, "token: authorization code user_id decode failed");
            OAuthError::server_error("authorization code store unavailable")
        })?,
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
        &consumed.user_id,
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
            user_id: &consumed.user_id,
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

    let access_token = mint_access_token(
        db,
        issuer,
        client,
        &consumed.user_id,
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
        // Read identity fields only for scopes that expose them.
        let identity_claims = if wants_identity_claims {
            Some(transaction_identity_claims(db, proof, &consumed.granted_scopes).await?)
        } else {
            None
        };
        let token = if client.brokered {
            issuer
                .issue_principal_id_token(
                    db,
                    &PrincipalIdTokenMint {
                        principal_id: &consumed.user_id,
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
                        user_id: &consumed.user_id,
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
            consumed.user_id.as_str().to_owned()
        } else {
            issuer.pairwise_subject(&consumed.user_id, &client.sector_identifier)
        };
        backchannel_logout::record_rp_participation(
            db,
            &consumed.user_id,
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

#[allow(clippy::future_not_send)]
async fn transaction_identity_claims(
    db: &Transaction<'_>,
    proof: &ValidatedSession,
    granted_scopes: &[String],
) -> Result<ScopeGatedIdentityClaims, OAuthError> {
    let lookup_error = |error: compio_postgres::Error| {
        tracing::error!(
            error = %error,
            user_id = proof.person_id().as_str(),
            "token: id-token identity lookup failed"
        );
        OAuthError::server_error("id token user lookup failed")
    };
    let rows = db
        .query(
            "SELECT email::text AS email, email_verified_at IS NOT NULL AS email_verified, \
             name, avatar_url FROM zeroship.users WHERE id = $1",
            &[&proof.person_id().as_str()],
        )
        .await
        .map_err(lookup_error)?;
    let row = rows.first().ok_or_else(|| {
        tracing::error!(
            user_id = proof.person_id().as_str(),
            "token: consumed code user is missing"
        );
        OAuthError::server_error("id token user missing")
    })?;
    Ok(IdentityProfile {
        email: row.try_get("email").map_err(lookup_error)?,
        email_verified: row.try_get("email_verified").map_err(lookup_error)?,
        name: row.try_get("name").map_err(lookup_error)?,
        picture: row.try_get("avatar_url").map_err(lookup_error)?,
    }
    .for_scopes(granted_scopes.iter().map(String::as_str)))
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
    let user_id = crate::entity_ids::user_id_with_context(
        row,
        "user_id",
        "authorization code replay user_id is invalid",
    )
    .map_err(|err| {
        tracing::error!(error = %err, "token: authorization code replay user_id decode failed");
        OAuthError::server_error("authorization code store unavailable")
    })?;
    let sector_identifier: String = row.get("sector_identifier");
    let sub = issuer.pairwise_subject(&user_id, &sector_identifier);
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
        app_id: crate::entity_ids::optional_app_id(row, "app_id").map_err(|err| {
            tracing::error!(error = %err, client_id = %client_id, "oauth client carries an unreadable app id");
            OAuthError::server_error("client registry unavailable")
        })?,
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
    user_id: &UserId,
    client_id: &str,
    requested_scopes: &[String],
) -> Result<Vec<String>, String> {
    let existing = db
        .query(
            "SELECT granted_scopes \
             FROM zeroship.oauth_grants \
             WHERE user_id = $1 AND client_id = $2",
            &[&user_id.as_str(), &client_id],
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
        &[&user_id.as_str(), &client_id, &granted],
    )
    .await
    .map_err(|err| format!("oauth grant upsert failed: {err}"))?;
    Ok(granted)
}

async fn consent_covers(
    db: &(impl GenericClient + ?Sized),
    user_id: &UserId,
    client_id: &str,
    granted_scopes: &[String],
) -> Result<bool, OAuthError> {
    let rows = db
        .query(
            "SELECT granted_scopes \
             FROM zeroship.oauth_grants \
             WHERE user_id = $1 AND client_id = $2",
            &[&user_id.as_str(), &client_id],
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
    user_id: &UserId,
    client_id: &str,
) -> Result<(), OAuthError> {
    db.execute(
        "UPDATE zeroship.oauth_grants \
         SET last_used_at = NOW() \
         WHERE user_id = $1 AND client_id = $2",
        &[&user_id.as_str(), &client_id],
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
    user_id: &UserId,
    scopes: &[String],
    proof: &ValidatedSession,
) -> Result<String, OAuthError> {
    crate::advisory_lock::lock_refresh_user_xact(db, user_id)
        .await
        .map_err(|err| {
            tracing::error!(error = %err, user_id = user_id.as_str(), "access-token mint user lock failed");
            OAuthError::server_error("access-token mint unavailable")
        })?;
    let active = db
        .query(ACCESS_MINT_PRINCIPAL_ACTIVE_SQL, &[&user_id.as_str()])
        .await
        .map_err(|err| {
            tracing::error!(error = %err, user_id = user_id.as_str(), "access-token mint lifecycle lookup failed");
            OAuthError::server_error("access-token mint unavailable")
        })?;
    if active.is_empty() {
        return Err(OAuthError::invalid_grant("authenticated user is inactive"));
    }

    let pairwise_sub = issuer.pairwise_subject(user_id, &client.sector_identifier);
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
            &[&client.client_id, &user_id.as_str(), &pairwise_sub],
        )
        .await
        .map_err(|err| {
            tracing::error!(
                error = %err,
                client_id = %client.client_id,
                user_id = user_id.as_str(),
                "token: pairwise identity mapping failed"
            );
            OAuthError::server_error("pairwise identity store unavailable")
        })?;
    if mapped != 1 {
        tracing::error!(
            client_id = %client.client_id,
            user_id = user_id.as_str(),
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
                user_id,
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
mod access_identity_tests;
