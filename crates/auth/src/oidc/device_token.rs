//! Native OP device grant support plus the internal platform-token mint.

use std::collections::HashSet;
use std::sync::Arc;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Duration, Utc};
use compio_postgres::{Client, GenericClient, Transaction};
use ntex::http::StatusCode;
use ntex::web::{self, HttpRequest, HttpResponse};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroship_authz::Scope;
use zeroship_core::auth::{constant_time_eq, extract_bearer};
use zeroship_core::device_grant::{
    OFFLINE_ACCESS_SCOPE, PLATFORM_CLI_CLIENT_ID, PLATFORM_CLI_ISSUABLE_SCOPES,
    PLATFORM_CLI_REGISTERED_SCOPES, PLATFORM_TOKEN_MAX_TTL_SECS,
};

use crate::advisory_lock::lock_refresh_user_xact;
use crate::config::AuthConfig;
use crate::error::{AuthError, Result as AuthResult};
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
const USER_CODE_ATTEMPTS: usize = 8;
const DEFAULT_DEVICE_SCOPE: &str = "openid";
const PLATFORM_CLI_CLIENT_NAME: &str = "zeroship CLI";
pub(crate) use zeroship_core::device_grant::{OP_PROVIDER as OP_DEVICE_PROVIDER, PLATFORM_PROVIDER};

fn platform_cli_redirect_uris() -> Vec<String> {
    Vec::new()
}

/// What the CLI registration is allowed to REQUEST - the authority ceiling
/// plus `offline_access`, which asks for the refresh family and confers no
/// authority of its own.
fn platform_cli_scopes() -> Vec<String> {
    PLATFORM_CLI_REGISTERED_SCOPES
        .iter()
        .map(|scope| (*scope).to_string())
        .collect()
}

async fn platform_cli_registration_is_exact(
    db: &(impl GenericClient + ?Sized),
) -> AuthResult<bool> {
    let redirects = platform_cli_redirect_uris();
    let scopes = platform_cli_scopes();
    let rows = db
        .query(
            "SELECT oc.client_name = $2 \
                    AND oc.client_uri IS NULL \
                    AND oc.logo_uri IS NULL \
                    AND oc.redirect_uris = $3 \
                    AND oc.scopes = $4 \
                    AND oc.skip_consent \
                    AND oc.created_by IS NULL \
                    AND oc.client_secret_hash IS NULL \
                    AND oc.refresh_allowed \
                    AND oc.token_endpoint_auth_method = 'none' \
                    AND NOT oc.brokered \
                    AND oc.backchannel_logout_uri IS NULL \
                    AND NOT EXISTS ( \
                        SELECT 1 FROM zeroship.app_oauth_clients aoc \
                        WHERE aoc.client_id = oc.client_id \
                    ) AS exact \
             FROM zeroship.oauth_clients oc \
             WHERE oc.client_id = $1",
            &[
                &PLATFORM_CLI_CLIENT_ID,
                &PLATFORM_CLI_CLIENT_NAME,
                &redirects,
                &scopes,
            ],
        )
        .await
        .map_err(|err| AuthError::Db(format!("load platform CLI registration: {err}")))?;
    let Some(row) = rows.first() else {
        return Ok(false);
    };
    row.try_get::<_, bool>("exact")
        .map_err(|err| AuthError::Db(format!("decode platform CLI registration: {err}")))
}

/// Create or repair the reserved first-party CLI client registration.
///
/// The fixed row is also checked at device authorization and redemption. This
/// startup reconciliation is availability plumbing, not the policy boundary.
pub async fn reconcile_platform_cli_client(
    db: &(impl GenericClient + ?Sized),
) -> AuthResult<()> {
    let redirects = platform_cli_redirect_uris();
    let scopes = platform_cli_scopes();
    let updated = db
        .execute(
            "INSERT INTO zeroship.oauth_clients \
                (client_id, client_name, client_uri, logo_uri, redirect_uris, scopes, \
                 skip_consent, created_by, client_secret_hash, refresh_allowed, \
                 token_endpoint_auth_method, brokered, backchannel_logout_uri) \
             VALUES ($1, $2, NULL, NULL, $3, $4, TRUE, NULL, NULL, TRUE, \
                     'none', FALSE, NULL) \
             ON CONFLICT (client_id) DO UPDATE SET \
                client_name = EXCLUDED.client_name, \
                client_uri = NULL, \
                logo_uri = NULL, \
                redirect_uris = EXCLUDED.redirect_uris, \
                scopes = EXCLUDED.scopes, \
                skip_consent = TRUE, \
                created_by = NULL, \
                client_secret_hash = NULL, \
                refresh_allowed = TRUE, \
                token_endpoint_auth_method = 'none', \
                brokered = FALSE, \
                backchannel_logout_uri = NULL \
             WHERE NOT EXISTS ( \
                SELECT 1 FROM zeroship.app_oauth_clients aoc \
                WHERE aoc.client_id = $1 \
             )",
            &[
                &PLATFORM_CLI_CLIENT_ID,
                &PLATFORM_CLI_CLIENT_NAME,
                &redirects,
                &scopes,
            ],
        )
        .await
        .map_err(|err| AuthError::Db(format!("reconcile platform CLI registration: {err}")))?;
    if updated != 1 || !platform_cli_registration_is_exact(db).await? {
        return Err(AuthError::Config(
            "zeroship-cli OAuth registration is not the fixed first-party policy".to_string(),
        ));
    }
    Ok(())
}

pub(super) async fn platform_cli_policy_selected(
    db: &(impl GenericClient + ?Sized),
    client_id: &str,
) -> Result<bool, OAuthError> {
    if client_id != PLATFORM_CLI_CLIENT_ID {
        return Ok(false);
    }
    match platform_cli_registration_is_exact(db).await {
        Ok(true) => Ok(true),
        Ok(false) => {
            tracing::error!(client_id, "platform CLI registration is not the fixed policy");
            Err(OAuthError::server_error("client misconfigured"))
        }
        Err(err) => {
            tracing::error!(error = %err, client_id, "platform CLI registration lookup failed");
            Err(OAuthError::server_error("client registry unavailable"))
        }
    }
}

/// Mint the access token a grant for `client` must hand back.
///
/// The first-party CLI client is a PLATFORM PRINCIPAL client: its token's
/// `sub` is the `zeroship.users` UUID and its `aud` is control's configured
/// resource audience, because control is the only thing that consumes it
/// (`zeroship_authn::BearerVerifier::verify_bearer` compares both, and
/// `crates/authz`'s `token_revocations` lookup is keyed on `(client_id, sub)`
/// with that same UUID). Every other client gets the ordinary pairwise OIDC
/// access token, with its per-app sector subject and app-resource audience.
///
/// Both grants that can produce a CLI token come through here - the device
/// grant, and the refresh rotation that follows it. That is the point: a
/// rotation that minted the pairwise shape would answer HTTP 200 and then be
/// refused by control, which is a failure with no error message anywhere near
/// its cause.
#[allow(clippy::future_not_send)]
pub(super) async fn mint_grant_access_token(
    db: &Transaction<'_>,
    cfg: &AuthConfig,
    issuer: &Issuer,
    client: &crate::oidc::authorization_code::OAuthClient,
    user_id: Uuid,
    scopes: &[String],
) -> Result<String, OAuthError> {
    if !platform_cli_policy_selected(db, &client.client_id).await? {
        return mint_access_token(db, issuer, client, user_id, scopes).await;
    }
    let principal_id = user_id.to_string();
    issuer
        .issue_principal_access_token(
            db,
            &PrincipalAccessTokenMint {
                principal_id: &principal_id,
                audience: cfg.settings.oauth_audience.get().trim(),
                client_id: PLATFORM_CLI_CLIENT_ID,
                scopes,
                ttl_secs: Some(ACCESS_TOKEN_TTL_SECS),
            },
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "platform CLI access-token mint failed");
            OAuthError::server_error("access token mint failed")
        })
}

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

/// A pending grant the `/device` page found for a typed user code.
///
/// The `provider` column selects which service redeems the approved row, so the
/// page renders from it rather than assuming: an [`OP_DEVICE_PROVIDER`] row is
/// redeemed at this service's `/oauth2/token` and names a registered OAuth
/// client, while a [`PLATFORM_PROVIDER`] row is redeemed at control's
/// `/api/device/token` and names none.
#[derive(Debug, Clone)]
pub(crate) struct PendingDeviceGrant {
    pub client_id: String,
    pub client_name: String,
    pub scopes: Vec<String>,
    pub provider: String,
}

impl PendingDeviceGrant {
    /// Whether this row belongs to control's deploy-token flow.
    pub(crate) fn is_platform(&self) -> bool {
        self.provider == PLATFORM_PROVIDER
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MintPlatformTokenRequest {
    pub principal_id: String,
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
    pub client_id: &'static str,
}

struct PlatformMintCaller {
    token_client_id: &'static str,
    issuable_scopes: &'static [&'static str],
}

const CONTROL_MINT_CALLER: PlatformMintCaller = PlatformMintCaller {
    token_client_id: PLATFORM_CLI_CLIENT_ID,
    issuable_scopes: &PLATFORM_CLI_ISSUABLE_SCOPES,
};

fn platform_token_scopes(
    caller: &PlatformMintCaller,
    requested: &[String],
    granted: &HashSet<String>,
) -> Vec<String> {
    let mut seen = HashSet::new();
    requested
        .iter()
        .map(|scope| scope.trim().to_string())
        .filter(|scope| {
            granted.contains(scope)
                && caller.issuable_scopes.contains(&scope.as_str())
                && seen.insert(scope.clone())
        })
        .collect()
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
    let platform_cli = platform_cli_policy_selected(db, &client.client_id).await?;
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
    let allowed_scopes = if platform_cli {
        platform_cli_scopes()
    } else {
        client.scopes.clone()
    };
    if !scope_subset(&requested_scopes, &allowed_scopes) {
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

/// Look up a pending grant by user code, across BOTH device flows.
///
/// `user_code` is unique over the whole table (`device_grants_user_code_key`),
/// so one code identifies at most one row and the `provider` column tells the
/// caller which flow it belongs to. This deliberately does NOT filter on
/// provider: filtering on [`OP_DEVICE_PROVIDER`] is precisely what made every
/// code `zeroship login` printed read as "invalid or expired" on the only page
/// that can approve anything.
///
/// The join to `oauth_clients` is a LEFT join for the same reason. Control's
/// parallel-flow rows carry no `client_id`, even though auth now registers the
/// CLI for its own OP flow, so an inner join would still drop the control rows.
#[allow(clippy::future_not_send)]
pub(crate) async fn pending_user_code_details(
    db: &Client,
    user_code: &str,
) -> Result<Option<PendingDeviceGrant>, String> {
    let user_code = normalize_user_code(user_code);
    if user_code.is_empty() {
        return Ok(None);
    }
    let rows = db
        .query(
            "SELECT dg.provider, dg.client_id, dg.scope, \
                    COALESCE(oc.client_name, dg.client_id, '') AS client_name \
             FROM zeroship.device_grants dg \
             LEFT JOIN zeroship.oauth_clients oc ON oc.client_id = dg.client_id \
             WHERE dg.user_code = $1 \
               AND dg.status = 'pending' \
               AND dg.expires_at > NOW() \
             LIMIT 1",
            &[&user_code],
        )
        .await
        .map_err(|err| format!("pending device grant detail lookup failed: {err}"))?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let provider: String = row.get("provider");
    let scope: Option<String> = row.get("scope");
    let client_id: Option<String> = row.get("client_id");
    Ok(Some(PendingDeviceGrant {
        client_id: client_id.unwrap_or_default(),
        client_name: row.get("client_name"),
        scopes: scope.as_deref().map(parse_scopes).unwrap_or_default(),
        provider,
    }))
}

/// Bind the signed-in user to a pending grant.
///
/// `provider` comes from the row [`pending_user_code_details`] just read, so the
/// UPDATE approves the same flow the page rendered a confirmation for and never
/// a different one that happened to reuse the code.
///
/// The write is identical for both flows because `principal_id` means the same
/// thing in both: a `zeroship.users` id. That is what lets a signed-in browser
/// approve a control-plane deploy grant with no control credential of its own -
/// control reads the bound principal on the CLI's next poll and mints from it.
/// `sid` and `auth_credential_version` are stamped either way; only the OP flow
/// redeems them, and for a platform row they record which IdP session
/// authorized a deploy token.
#[allow(clippy::future_not_send)]
pub(crate) async fn approve_user_code(
    db: &Client,
    user_code: &str,
    provider: &str,
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
                &provider,
            ],
        )
        .await
        .map_err(|err| format!("device grant approve failed: {err}"))?;
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
    db: &Transaction<'_>,
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
    let platform_cli = platform_cli_policy_selected(db, &client.client_id).await?;

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
            lock_refresh_user_xact(db, user_id)
                .await
                .map_err(|err| {
                    tracing::error!(
                        error = %err,
                        user_id = %user_id,
                        "device token: user lock failed"
                    );
                    OAuthError::server_error("device token validation unavailable")
                })?;
            let owner = db
                .query(
                    "SELECT 1 FROM zeroship.users \
                     WHERE id = $1 \
                       AND credential_version = $2 \
                       AND disabled_at IS NULL \
                       AND deletion_requested_at IS NULL \
                       AND deletion_scheduled_for IS NULL \
                       AND anonymized_at IS NULL",
                    &[&user_id, &auth_credential_version],
                )
                .await
                .map_err(|err| {
                    tracing::error!(
                        error = %err,
                        user_id = %user_id,
                        "device token: owner validation failed"
                    );
                    OAuthError::server_error("device token validation unavailable")
                })?;
            if owner.is_empty() {
                return Err(device_error(
                    "access_denied",
                    "device authorization is no longer valid",
                ));
            }
            let requested_scopes = row
                .get::<_, Option<String>>("scope")
                .map(|scope| parse_scopes(&scope))
                .unwrap_or_default();
            let granted_scopes = sort_dedup(requested_scopes);
            // The cap here is the client REGISTRATION, and only that.
            //
            // Say what this does not do, because two sibling paths do it and
            // the difference is invisible from the response: neither this
            // check nor the mint below intersects with
            // `zeroship.principal_grants`, which control's
            // `deploy_scopes_for_principal` and `mint_platform_token` both do.
            // An operator who deletes a grant row therefore does not narrow a
            // token issued here.
            //
            // It cannot be fixed in this function alone. The only writer of
            // platform-creator grants is control's `/api/device/token`
            // (`identity_bridge::ensure_platform_creator_grants`), and
            // `db/migrations-ts/20260702000900_grants.ts` gives `zeroship_auth`
            // SELECT only on that table - so intersecting before provisioning
            // moves would mint `scope: ""` on every first login. Pinned by
            // `crates/auth/tests/cli_device_refresh_test.rs`,
            // `the_cli_device_grant_does_not_consult_stored_principal_grants`.
            if platform_cli && !scope_subset(&granted_scopes, &platform_cli_scopes()) {
                return Err(OAuthError::invalid_grant(
                    "device grant scope is no longer allowed",
                ));
            }
            let Some(sid) = row.get::<_, Option<String>>("sid") else {
                tracing::error!("device token: approved grant missing sid");
                return Err(OAuthError::server_error("device grant is incomplete"));
            };

            delete_device_grant(db, device_code_hash).await?;

            // One lifetime for every client on this grant. The CLI used to take
            // the 12-hour ceiling BECAUSE it got no refresh token; now that it
            // does, the trade runs the other way - a short self-contained
            // bearer plus a long DB-backed family gives the same usable
            // session AND a revocation that works.
            let access_token =
                mint_grant_access_token(db, cfg, issuer, client, user_id, &granted_scopes).await?;
            let refresh_token = if granted_scopes
                .iter()
                .any(|scope| scope == OFFLINE_ACCESS_SCOPE)
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
    zeroship_core::device_grant::generate_user_code(&mut rand::thread_rng())
}

fn normalize_user_code(value: &str) -> String {
    zeroship_core::device_grant::normalize_user_code(value)
}

pub(crate) fn valid_user_code(value: &str) -> bool {
    zeroship_core::device_grant::valid_user_code(value)
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
    refresh_pool: web::types::State<RefreshSessionPool>,
    body: web::types::Json<MintPlatformTokenRequest>,
) -> HttpResponse {
    let Some(caller) = authenticated_platform_mint_caller(&req, cfg.as_ref()) else {
        return HttpResponse::Unauthorized().json(&json!({"error": "unauthorized"}));
    };

    let principal_id = match Uuid::parse_str(body.principal_id.trim()) {
        Ok(principal_id) => principal_id,
        Err(_) => {
            return HttpResponse::BadRequest().json(&json!({"error": "invalid_principal_id"}));
        }
    };
    if body
        .scopes
        .iter()
        .any(|scope| Scope::parse(scope.trim()).is_err())
    {
        return HttpResponse::BadRequest().json(&json!({"error": "invalid_scope"}));
    }
    let ttl_secs = body.ttl_secs.unwrap_or(ACCESS_TOKEN_TTL_SECS);
    if ttl_secs <= 0 || ttl_secs > PLATFORM_TOKEN_MAX_TTL_SECS {
        return HttpResponse::BadRequest().json(&json!({"error": "invalid_ttl"}));
    }

    let pool = match refresh_pool.checkout_pool("platform token mint").await {
        Ok(pool) => pool,
        Err(err) => {
            tracing::error!(error = %err, "auth: platform token database pool failed");
            return HttpResponse::InternalServerError()
                .json(&json!({"error": "grant_lookup_failed"}));
        }
    };
    let mut conn = match pool.get().await {
        Ok(conn) => conn,
        Err(err) => {
            tracing::error!(error = %err, "auth: platform token database checkout failed");
            return HttpResponse::InternalServerError()
                .json(&json!({"error": "grant_lookup_failed"}));
        }
    };
    let tx = match conn.transaction().await {
        Ok(tx) => tx,
        Err(err) => {
            tracing::error!(error = %err, "auth: platform token transaction failed");
            return HttpResponse::InternalServerError()
                .json(&json!({"error": "grant_lookup_failed"}));
        }
    };
    if let Err(err) = crate::advisory_lock::lock_refresh_user_xact(&tx, principal_id).await {
        tracing::error!(error = %err, principal_id = %principal_id, "auth: platform token user lock failed");
        return HttpResponse::InternalServerError()
            .json(&json!({"error": "grant_lookup_failed"}));
    }
    let rows = match tx
        .query(
            "SELECT pg.grant_name \
             FROM zeroship.users u \
             LEFT JOIN zeroship.principal_grants pg ON pg.principal_id = u.id \
             WHERE u.id = $1 \
               AND u.disabled_at IS NULL \
               AND u.anonymized_at IS NULL \
               AND u.deletion_requested_at IS NULL \
               AND u.deletion_scheduled_for IS NULL \
               AND (u.locked_until IS NULL OR u.locked_until <= NOW()) \
             ORDER BY pg.grant_name",
            &[&principal_id],
        )
        .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(
                error = %err,
                principal_id = %principal_id,
                "auth: platform token principal grant lookup failed"
            );
            return HttpResponse::InternalServerError()
                .json(&json!({"error": "grant_lookup_failed"}));
        }
    };
    if rows.is_empty() {
        return HttpResponse::BadRequest().json(&json!({"error": "unknown_principal"}));
    }
    let granted: HashSet<String> = rows
        .iter()
        .filter_map(|row| row.get::<_, Option<String>>("grant_name"))
        .collect();
    let scopes = platform_token_scopes(caller, &body.scopes, &granted);
    let principal_id = principal_id.to_string();
    let access_token = match issuer
        .issue_principal_access_token(
            &tx,
            &PrincipalAccessTokenMint {
                principal_id: &principal_id,
                audience: cfg.settings.oauth_audience.get().trim(),
                client_id: caller.token_client_id,
                scopes: &scopes,
                ttl_secs: Some(ttl_secs),
            },
        )
        .await
    {
        Ok(token) => token,
        Err(err) => {
            tracing::error!(error = %err, "auth: platform token mint failed");
            return HttpResponse::InternalServerError().json(&json!({"error": "mint_failed"}));
        }
    };
    if let Err(err) = tx.commit().await {
        tracing::error!(error = %err, "auth: platform token transaction commit failed");
        return HttpResponse::InternalServerError().json(&json!({"error": "mint_failed"}));
    }

    HttpResponse::Ok().json(&MintPlatformTokenResponse {
        access_token,
        token_type: "Bearer",
        expires_in: ttl_secs as u64,
        scope: scopes.join(" "),
        provider: "platform",
        client_id: caller.token_client_id,
    })
}

fn authenticated_platform_mint_caller(
    req: &HttpRequest,
    cfg: &AuthConfig,
) -> Option<&'static PlatformMintCaller> {
    let expected = cfg.settings.platform_mint_key.expose_str().trim();
    if expected.is_empty() {
        return None;
    }
    let header = req
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    extract_bearer(header)
        .filter(|provided| constant_time_eq(provided, expected))
        .map(|_| &CONTROL_MINT_CALLER)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{platform_token_scopes, CONTROL_MINT_CALLER};

    #[test]
    fn platform_token_scopes_apply_the_callers_issuance_ceiling() {
        let requested = vec!["apps:read".to_string(), "billing:write".to_string()];
        let granted = HashSet::from(["apps:read".to_string(), "billing:write".to_string()]);

        assert_eq!(
            platform_token_scopes(&CONTROL_MINT_CALLER, &requested, &granted),
            ["apps:read"]
        );
    }
}
