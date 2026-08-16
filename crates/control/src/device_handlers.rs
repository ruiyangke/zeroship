//! Control-mediated OAuth device flow for platform deploy tokens.
//!
//! This is the flow `zeroship login` drives. Control owns the RFC 8628 pending
//! rows and the polling discipline; the auth service owns issuance. Rows
//! written here carry `provider = 'platform'`
//! ([`zeroship_core::device_grant::PLATFORM_PROVIDER`]).
//!
//! Three steps, and the split between them is deliberate:
//!
//! 1. `/api/device/auth` mints the pending row and returns the user code plus
//!    the `/device` page's absolute URL, derived from the configured platform
//!    issuer so the URL the CLI prints is one the deployment actually serves.
//! 2. Approval BINDS A PRINCIPAL to the row and nothing else. Two vehicles
//!    reach it: a signed-in browser at the auth service's `/device` page, which
//!    writes the row directly (it holds the session, and `principal_id` is a
//!    `zeroship.users` id both services share), and `/api/device/approve` for a
//!    Supabase deployment, where the browser holds a GoTrue bearer instead of a
//!    zeroship session.
//! 3. `/api/device/token` mints. The CLI's poll resolves the principal's
//!    `zeroship.principal_grants`, caps the requested scopes to them, and asks
//!    the OP for a platform access token. The OP independently loads the same
//!    principal and grants and caps the scopes again before issuance.
//!
//! Minting at poll time rather than at approval time is what lets a browser
//! approve without being able to mint: the approving vehicle needs no control
//! credential, no master key, and no way to reach the OP's internal mint. It
//! also means no access token is ever written to a row, so the grant table
//! holds no secret at rest.
//!
//! This is NOT the OP's own device grant. `crates/auth/src/oidc/device_token.rs`
//! implements RFC 8628 natively over the same `zeroship.device_grants` table
//! under `provider = 'op'`, and its approved rows are redeemed at the OP's
//! `/oauth2/token` for an OIDC token. The `provider` column keeps the two
//! apart, and the auth service's `/device` page dispatches on it.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use base64::Engine as _;
use chrono::{DateTime, Duration, Utc};
use ntex::http::StatusCode;
use ntex::web;
use ntex::web::types::{Json, State};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use zeroship_core::auth::extract_bearer;
use zeroship_core::auth_provider::{ProviderAuthz, VerifiedToken, VerifyTokenError};
use zeroship_core::device_grant::{
    self, PLATFORM_CLI_CLIENT_ID, PLATFORM_PROVIDER, PLATFORM_TOKEN_MAX_TTL_SECS,
};

use crate::{identity_bridge, AppState};

const DEVICE_CODE_BYTES: usize = 32;
const DEVICE_TTL_SECS: i64 = 600;
const POLL_INTERVAL_SECS: i64 = 5;
const USER_CODE_ATTEMPTS: usize = 8;
const PLATFORM_MINT_TIMEOUT: StdDuration = StdDuration::from_secs(3);
const PLATFORM_TOKEN_ENDPOINT: &str = "/internal/platform-token";
const DEPLOY_TOKEN_SCOPES: [&str; 4] = [
    "apps:deploy",
    "apps:read",
    "apps:write",
    "secrets:read",
];

/// Lifetime of the deploy token `zeroship login` ends up holding.
///
/// The OP's default access-token lifetime is 15 minutes
/// (`crates/auth/src/oidc/issuer.rs`), which is right for a browser session
/// that can refresh silently and wrong for a CLI: this flow issues no refresh
/// token, so a 15-minute deploy token means a human runs `zeroship login`
/// again before most first deploys finish. A working day is the unit that
/// matches the credential's actual use.
///
/// What bounds it: the token's `aud` is control's OAuth audience, so the
/// gateway and app runtime do not accept it; its scope is capped to the
/// principal's stored grants intersected with [`DEPLOY_TOKEN_SCOPES`], so it
/// carries no admin or billing authority; and the CLI writes it 0600. What
/// does NOT currently bound it is a supported server-side revocation action.
/// The bearer read path honors `zeroship.token_revocations`, but disconnecting
/// an app writes a per-app client and pairwise subject. Auth's generic RFC 7009
/// writer requires an authenticated registered client, while `zeroship-cli`
/// is deliberately unregistered. No supported path writes the
/// (`zeroship-cli`, principal UUID) marker this token needs, so the shared
/// 12-hour issuance ceiling is its effective recall bound today.

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
}

#[derive(Debug, Deserialize)]
pub struct DeviceTokenRequest {
    device_code: String,
    grant_type: String,
}

#[derive(Debug, Serialize)]
pub struct DeviceTokenResponse {
    access_token: String,
    token_type: &'static str,
    provider: &'static str,
    expires_in: u64,
    scope: String,
    principal_id: String,
}

#[derive(Debug, Serialize)]
struct PlatformMintRequest<'a> {
    principal_id: &'a str,
    scopes: &'a [String],
    ttl_secs: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct PlatformMintResponse {
    access_token: String,
    expires_in: u64,
    scope: String,
    token_type: String,
    provider: String,
    client_id: String,
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(web::resource("/api/device/auth").route(web::post().to(device_auth)))
        .service(web::resource("/api/device/approve").route(web::post().to(device_approve)))
        .service(web::resource("/api/device/token").route(web::post().to(device_token)));
}

pub async fn device_auth(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    body: Json<DeviceAuthRequest>,
) -> web::HttpResponse {
    // Unauthenticated, and every call mints durable device-flow state, so an
    // unbounded caller can fill the table without ever holding a credential.
    // Shares the admin bucket so a caller cannot get a fresh allowance by
    // moving between control surfaces.
    if let Some(resp) = crate::env_handlers::admin_rate_limit(&req, &state).await {
        return resp;
    }
    if let Err(resp) = ensure_platform_device_provider(&state) {
        return resp;
    }
    let client_id = body.client_id.trim();
    if client_id.is_empty() || client_id.len() > 128 {
        return bad_request("invalid_client_id");
    }
    // Resolved BEFORE the first insert. A grant whose verification URI cannot
    // be derived is a grant no human can approve, and leaving the row behind
    // would only give the CLI something to poll forever.
    let Some(verification_uri) = verification_uri(&state) else {
        tracing::error!(
            platform_issuer = state.auth_provider.platform_issuer().unwrap_or(""),
            "control: platform issuer does not yield a /device page URL"
        );
        return internal_error();
    };

    let scope = body.scope.as_deref().map(str::trim).filter(|s| !s.is_empty());
    for _ in 0..USER_CODE_ATTEMPTS {
        let device_code = generate_device_code();
        let device_code_hash = sha256_hex(&device_code);
        let user_code = device_grant::generate_user_code(&mut rand::rngs::OsRng);
        let inserted = match state
            .control_pg
            .query_opt(
                "INSERT INTO zeroship.device_grants \
                    (device_code_hash, user_code, provider, scope, expires_at) \
                 VALUES ($1, $2, $3, $4, NOW() + ($5::TEXT)::INTERVAL) \
                 ON CONFLICT DO NOTHING \
                 RETURNING user_code",
                &[
                    &device_code_hash,
                    &user_code,
                    &PLATFORM_PROVIDER,
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

/// Approve a pending platform grant with an explicit bearer.
///
/// This is the SUPABASE deployment's browser leg: the `/device` page there runs
/// a GoTrue sign-in in the browser and posts the resulting bearer here, because
/// the auth service holds no zeroship session for that user. On a platform-only
/// deployment the browser is signed in to the auth service itself and the
/// `/device` page binds the principal directly, without this endpoint.
///
/// Approval binds a principal and nothing more. The scope cap and the mint
/// happen in [`device_token`], on the CLI's poll.
pub async fn device_approve(
    state: State<Arc<AppState>>,
    req: web::HttpRequest,
    body: Json<DeviceApproveRequest>,
) -> web::HttpResponse {
    if let Some(resp) = crate::env_handlers::admin_rate_limit(&req, &state).await {
        return resp;
    }
    // Approval uses an explicit non-ambient bearer and does not depend on
    // browser cookies.
    let verified = match verified_device_approval_bearer(&state, &req).await {
        Ok(verified) => verified,
        Err(resp) => return resp,
    };

    let user_code = device_grant::normalize_user_code(&body.user_code);
    if !device_grant::valid_user_code(&user_code) {
        return bad_request("invalid_user_code");
    }

    let principal_id = match device_approval_principal(&state, &verified).await {
        Ok(principal_id) => principal_id,
        Err(resp) => return resp,
    };
    if let Err(err) = state
        .bearer_verifier()
        .require_active_principal(principal_id)
        .await
    {
        tracing::warn!(
            error = %err,
            principal_id = %principal_id,
            "control: device approval principal is not active"
        );
        return unauthorized();
    }

    let updated = match state
        .control_pg
        .execute(
            "UPDATE zeroship.device_grants \
             SET principal_id = $1, \
                 status = 'approved' \
             WHERE user_code = $2 \
               AND provider = $3 \
               AND status = 'pending' \
               AND expires_at > NOW()",
            &[&principal_id, &user_code, &PLATFORM_PROVIDER],
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

/// NOT rate limited through the admin bucket, deliberately. The device polls
/// this endpoint on a fixed interval until the user approves - that is the
/// protocol, not abuse - so a 30-per-minute allowance would reject legitimate
/// flows, and several devices behind one NAT share a source address. Bounding
/// it needs a quota sized to the OAuth polling interval and a `slow_down`
/// response rather than a 429, which is a configuration decision.
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
            "SELECT status, expires_at, last_polled_at, principal_id, scope \
             FROM zeroship.device_grants \
             WHERE device_code_hash = $1 AND provider = $2 \
             FOR UPDATE",
            &[&device_code_hash, &PLATFORM_PROVIDER],
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
            let Some(principal_id) = row.get::<_, Option<uuid::Uuid>>("principal_id") else {
                tracing::error!("control: approved device grant missing principal_id");
                let _ = tx.rollback().await;
                return internal_error();
            };
            let requested_scope: Option<String> = row.get("scope");

            if let Err(err) = state
                .bearer_verifier()
                .require_active_principal(principal_id)
                .await
            {
                tracing::warn!(
                    error = %err,
                    principal_id = %principal_id,
                    "control: device token principal is not active"
                );
                let _ = tx.rollback().await;
                return oauth_error(StatusCode::BAD_REQUEST, "access_denied");
            }

            // A principal reaching this point has completed an interactive
            // approval, which is the platform's definition of a creator. The
            // grants are seeded once, marked by the identity link, so an
            // operator who later revokes one does not get it back on the next
            // login.
            if let Err(resp) = identity_bridge::ensure_platform_creator_grants(&tx, principal_id)
                .await
                .map_err(|err| {
                    tracing::error!(
                        error = %err,
                        principal_id = %principal_id,
                        "control: creator grant provisioning failed"
                    );
                    internal_error()
                })
            {
                let _ = tx.rollback().await;
                return resp;
            }

            let scopes = match deploy_scopes_for_principal(
                &tx,
                principal_id,
                requested_scope.as_deref(),
            )
            .await
            {
                Ok(scopes) => scopes,
                Err(resp) => {
                    let _ = tx.rollback().await;
                    return resp;
                }
            };
            // The mint is a bounded local call to the OP made while this row is
            // still locked, so the grant stays exactly-once: a second poll
            // blocks until this transaction resolves, and a failed mint rolls
            // back to `approved` for the CLI's next poll rather than burning
            // the grant.
            let minted = match mint_platform_deploy_token(&state, principal_id, &scopes).await {
                Ok(minted) => minted,
                Err(resp) => {
                    let _ = tx.rollback().await;
                    return resp;
                }
            };
            if minted.expires_in != PLATFORM_TOKEN_MAX_TTL_SECS as u64
                || minted.scope != scopes.join(" ")
            {
                tracing::error!(
                    expires_in = minted.expires_in,
                    expected_expires_in = PLATFORM_TOKEN_MAX_TTL_SECS,
                    response_scope = %minted.scope,
                    expected_scope = %scopes.join(" "),
                    "control: platform token mint response metadata mismatch"
                );
                let _ = tx.rollback().await;
                return internal_error();
            }
            if let Err(resp) = verify_minted_platform_deploy_token(
                &state,
                principal_id,
                &scopes,
                &minted.access_token,
            )
            .await
            {
                let _ = tx.rollback().await;
                return resp;
            }
            let access_token = minted.access_token;
            let expires_in = minted.expires_in;
            let scope = minted.scope;
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
            web::HttpResponse::Ok().json(&DeviceTokenResponse {
                access_token,
                token_type: "Bearer",
                provider: PLATFORM_PROVIDER,
                expires_in,
                scope,
                principal_id: principal_id.to_string(),
            })
        }
        other => {
            tracing::error!(status = other, "control: invalid device grant status");
            let _ = tx.rollback().await;
            internal_error()
        }
    }
}

/// This flow needs the PLATFORM OP, and nothing else.
///
/// Everything downstream is platform-shaped: the grant row is written with
/// `provider = 'platform'`, `mint_platform_deploy_token` posts to
/// `{platform_mint_url}/internal/platform-token` under the dedicated platform
/// mint key, and the minted token is verified back through `platform_issuer`.
/// Those three values are the whole precondition.
///
/// The mint URL is checked HERE, not only at the mint, because the alternative
/// is that a human reads a code out of the CLI, opens the page, approves, and
/// only then discovers the deployment cannot mint. Boot refuses the same
/// combination, so in a real process this arm is unreachable; it is the
/// handler-level statement of the precondition rather than a second policy.
///
/// The one Supabase-shaped arm downstream is the `GoTrueRole` branch of
/// [`device_approval_principal`], and it is not reachable from here. That is a
/// property of the trusted SET, re-derived after `LegacyAuthProvider` was
/// replaced by `AuthProvider::new(Vec<ConfiguredProvider>)`: the only producer
/// of `ProviderAuthz::GoTrueRole` is `ConfiguredProvider::Supabase`
/// (`crates/core/src/auth_provider/supabase.rs`), and `supabase_url()` answers
/// `Some` for exactly the sets that hold a `ConfiguredProvider::Supabase`
/// element. So a bearer that verifies as `GoTrueRole` proves the element is in
/// the set, and the `supabase_url` guard inside that arm cannot be the branch
/// that fires. Set SIZE does not change this - a set holding both backends
/// still answers `Some` - which is why the argument survived the refactor even
/// though its old form ("`LegacyAuthProvider` has one variant with a
/// non-optional URL") named a type that no longer exists.
///
/// So requiring a Supabase URL here protected nothing and blocked the shipped
/// default. `ZEROSHIP_AUTH_PROVIDER` unset or `platform` builds a one-element
/// platform set, whose `supabase_url()` is `None` by construction, so
/// `/api/device/auth` answered 400 on every default deployment and
/// `zeroship login` could not start. The clause is a leftover from
/// `ensure_supabase_provider`, which this function replaced when the flow
/// stopped being GoTrue-bound.
fn ensure_platform_device_provider(state: &AppState) -> Result<(), web::HttpResponse> {
    if state.auth_provider.platform_issuer().is_some()
        && !state.auth_platform_mint_key.is_empty()
        && state.platform_mint_url.is_some()
    {
        Ok(())
    } else {
        Err(unsupported_provider())
    }
}

async fn deploy_scopes_for_principal(
    pg: &(impl compio_postgres::GenericClient + ?Sized),
    principal_id: uuid::Uuid,
    requested_scope: Option<&str>,
) -> Result<Vec<String>, web::HttpResponse> {
    let requested = requested_deploy_scope_set(requested_scope);
    let rows = pg
        .query(
            "SELECT grant_name \
             FROM zeroship.principal_grants \
             WHERE principal_id = $1 \
             ORDER BY grant_name",
            &[&principal_id],
        )
        .await
        .map_err(|err| {
            tracing::error!(
                error = %err,
                principal_id = %principal_id,
                "control: device approve grant lookup failed"
            );
            internal_error()
        })?;
    let granted: HashSet<String> = rows
        .iter()
        .map(|row| row.get::<_, String>("grant_name"))
        .collect();
    Ok(DEPLOY_TOKEN_SCOPES
        .iter()
        .copied()
        .filter(|scope| requested.contains(*scope) && granted.contains(*scope))
        .map(str::to_string)
        .collect())
}

fn requested_deploy_scope_set(raw_scope: Option<&str>) -> HashSet<&'static str> {
    let mut requested = HashSet::new();
    let Some(raw_scope) = raw_scope else {
        requested.extend(DEPLOY_TOKEN_SCOPES);
        return requested;
    };
    for scope in raw_scope.split_whitespace() {
        if let Some(deploy_scope) = DEPLOY_TOKEN_SCOPES
            .iter()
            .copied()
            .find(|candidate| *candidate == scope)
        {
            requested.insert(deploy_scope);
        }
    }
    requested
}

async fn mint_platform_deploy_token(
    state: &AppState,
    principal_id: uuid::Uuid,
    scopes: &[String],
) -> Result<PlatformMintResponse, web::HttpResponse> {
    if state.auth_provider.platform_issuer().is_none() {
        return Err(unsupported_provider());
    }
    // The CONFIGURED destination, never one derived from the issuer. The issuer
    // names the OP's public identity; this names an address control can reach.
    // A fall back to the issuer here is what would silently restore the
    // egress-and-back POST that made every device approval fail, so there is
    // none: an unconfigured mint URL is refused, loudly, and boot refuses it
    // earlier still.
    //
    // The mint route is mounted on the auth service's ROOT config
    // (`crates/auth/src/server.rs` calls `oidc::device_token::configure(cfg)`
    // outside the `/oauth2` scope), so the endpoint hangs off the bare origin.
    let Some(mint_base_url) = state.platform_mint_url.as_deref() else {
        tracing::error!(
            "control: no platform mint URL is configured, so the deploy token cannot be minted; \
             set --auth-platform-mint-url / ZEROSHIP_AUTH_PLATFORM_MINT_URL"
        );
        return Err(internal_error());
    };
    let url = format!("{mint_base_url}{PLATFORM_TOKEN_ENDPOINT}");
    let principal_id_string = principal_id.to_string();
    let body = PlatformMintRequest {
        principal_id: &principal_id_string,
        scopes,
        ttl_secs: Some(PLATFORM_TOKEN_MAX_TTL_SECS),
    };
    let body = serde_json::to_vec(&body).map_err(|err| {
        tracing::error!(error = %err, "control: platform token mint request encode failed");
        internal_error()
    })?;
    let client = cyper::Client::new();
    let builder = client
        .post(&url)
        .map_err(|err| {
            tracing::error!(error = %err, "control: platform token mint request build failed");
            internal_error()
        })?
        .header("content-type", "application/json")
        .map_err(|err| {
            tracing::error!(error = %err, "control: platform token mint content-type failed");
            internal_error()
        })?
        .header(
            "authorization",
            &format!(
                "Bearer {}",
                state.auth_platform_mint_key.expose_secret()
            ),
        )
        .map_err(|err| {
            tracing::error!(error = %err, "control: platform token mint auth header failed");
            internal_error()
        })?;
    let response = compio::time::timeout(PLATFORM_MINT_TIMEOUT, builder.body(body).send())
        .await
        .map_err(|_| {
            tracing::error!("control: platform token mint request timed out");
            internal_error()
        })?
        .map_err(|err| {
            tracing::error!(error = %err, "control: platform token mint transport failed");
            internal_error()
        })?;
    let status = response.status().as_u16();
    let bytes = response.bytes().await.map_err(|err| {
        tracing::error!(error = %err, "control: platform token mint body read failed");
        internal_error()
    })?;
    if !(200..300).contains(&status) {
        tracing::error!(
            status,
            body = %String::from_utf8_lossy(&bytes),
            "control: platform token mint rejected"
        );
        return Err(internal_error());
    }
    let minted: PlatformMintResponse = serde_json::from_slice(&bytes).map_err(|err| {
        tracing::error!(error = %err, "control: platform token mint response parse failed");
        internal_error()
    })?;
    if minted.provider != "platform"
        || !minted.token_type.eq_ignore_ascii_case("Bearer")
        || minted.client_id != PLATFORM_CLI_CLIENT_ID
    {
        tracing::error!(
            provider = %minted.provider,
            token_type = %minted.token_type,
            client_id = %minted.client_id,
            "control: platform token mint response had invalid shape"
        );
        return Err(internal_error());
    }
    Ok(minted)
}

async fn verify_minted_platform_deploy_token(
    state: &AppState,
    principal_id: uuid::Uuid,
    scopes: &[String],
    access_token: &str,
) -> Result<(), web::HttpResponse> {
    let verified = state
        .auth_provider
        .verify_token(access_token)
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "control: minted platform token verification failed");
            internal_error()
        })?;
    let expected_subject = principal_id.to_string();
    let expected_scope = scopes.join(" ");
    let audience_ok = verified.aud.as_ref().is_some_and(|audiences| {
        audiences
            .iter()
            .any(|audience| audience == &state.expected_oauth_audience)
    });
    let scope_ok = matches!(
        &verified.provider_authz,
        ProviderAuthz::OAuthScope(raw_scope) if raw_scope == &expected_scope
    );
    let client_ok = verified.client_id.as_deref() == Some(PLATFORM_CLI_CLIENT_ID);
    if verified.provider_subject != expected_subject || !audience_ok || !scope_ok || !client_ok {
        tracing::error!(
            subject = %verified.provider_subject,
            expected_subject = %expected_subject,
            audience_ok,
            client_ok,
            authz = ?verified.provider_authz,
            expected_scope = %expected_scope,
            "control: minted platform token claims did not match device approval"
        );
        return Err(internal_error());
    }
    Ok(())
}

/// Verify the explicit bearer used for device approval.
///
/// WHAT IS CHECKED, for a platform OAuth bearer: the signature, issuer,
/// algorithm and lifetime (by `AuthProvider::verify_token`), that the token's
/// `aud` names THIS control plane, and that its family is not revoked. Those
/// last two are exactly the checks `zeroship_authn`'s `oauth_guard_from_bearer`
/// runs on the same token type, and this path calls the same revocation
/// function rather than a second copy of it.
///
/// WHAT IS NOT CHECKED: the incoming OAuth scope. That is deliberate - it is
/// not an approval rule, because the deploy token is restricted to the
/// principal's stored grants when it is minted. But saying only that was how
/// the other two came to be missing: the sentence answered the scope question
/// so convincingly that nobody asked what else the path let through, and the
/// answer was "an audience it was never issued for, and a revoked family". A
/// stolen bearer could start a device grant (that endpoint is unauthenticated),
/// approve it with itself, and poll out a token with a fresh `iat` - so the
/// deploy token's TTL bounded nothing.
///
/// For a GoTrue bearer, `authenticated` is still the whole rule. GoTrue tokens
/// carry `aud: "authenticated"`, not a zeroship resource audience, and there is
/// no zeroship-side revocation marker for them; imposing either check here
/// would refuse every valid GoTrue approval.
async fn verified_device_approval_bearer(
    state: &AppState,
    req: &web::HttpRequest,
) -> Result<VerifiedToken, web::HttpResponse> {
    let header = req
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let Some(token) = extract_bearer(header) else {
        return Err(unauthorized());
    };
    let verified = state
        .auth_provider
        .verify_token(token)
        .await
        .map_err(|err| match err {
            VerifyTokenError::InactiveToken
            | VerifyTokenError::MissingSubject
            | VerifyTokenError::MissingIssuer
            | VerifyTokenError::UnknownIssuer(_) => unauthorized(),
            VerifyTokenError::PlatformVerification(err) => {
                tracing::warn!(error = %err, "control: device approve platform bearer verify failed");
                unauthorized()
            }
        })?;

    match &verified.provider_authz {
        ProviderAuthz::OAuthScope(_) => {
            let audience_ok = verified.aud.as_ref().is_some_and(|audiences| {
                audiences
                    .iter()
                    .any(|audience| audience == &state.expected_oauth_audience)
            });
            if !audience_ok {
                tracing::warn!(
                    audiences = ?verified.aud,
                    expected = %state.expected_oauth_audience,
                    "control: device approve bearer was issued for another audience"
                );
                return Err(unauthorized());
            }
            state
                .bearer_verifier()
                .reject_revoked_platform_token(
                    verified.client_id.as_deref(),
                    &verified.provider_subject,
                    verified.iat,
                )
                .await
                .map_err(|err| {
                    tracing::warn!(
                        error = %err,
                        subject = %verified.provider_subject,
                        "control: device approve bearer refused by the revocation check"
                    );
                    unauthorized()
                })?;
            Ok(verified)
        }
        ProviderAuthz::GoTrueRole(role) if role == "authenticated" => Ok(verified),
        ProviderAuthz::GoTrueRole(_) => Err(unauthorized()),
    }
}

async fn device_approval_principal(
    state: &AppState,
    verified: &VerifiedToken,
) -> Result<uuid::Uuid, web::HttpResponse> {
    match &verified.provider_authz {
        ProviderAuthz::OAuthScope(_) => {
            uuid::Uuid::parse_str(&verified.provider_subject).map_err(|_| unauthorized())
        }
        ProviderAuthz::GoTrueRole(_) => {
            // LIVE, not dead: `verified` is only `GoTrueRole` when a Supabase
            // element verified it, and that element is what makes
            // `supabase_url()` answer `Some`, so this `else` cannot fire for a
            // bearer that got this far. It stays because the coupling lives in
            // another crate and nothing in the type system carries it.
            let Some(supabase_url) = state.auth_provider.supabase_url() else {
                return Err(unsupported_provider());
            };
            let service_role_key = state
                .auth_provider
                .supabase_service_role_key()
                .unwrap_or("");
            let email_verified = match identity_bridge::fetch_email_verified(
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

            let mut conn = state.registry.conn().await.map_err(|err| {
                tracing::error!(error = %err, "control: device approve DB connect failed");
                internal_error()
            })?;
            identity_bridge::provision_or_link(
                &mut conn,
                "supabase",
                &verified.provider_subject,
                verified.email.as_deref(),
                email_verified,
            )
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "control: device approve identity bridge failed");
                internal_error()
            })
        }
    }
}

fn generate_device_code() -> String {
    let mut bytes = [0_u8; DEVICE_CODE_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn sha256_hex(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    hex::encode(digest)
}

/// Absolute URL of the page a human opens to approve the grant.
///
/// Derived from the CONFIGURED platform issuer, not from `app_base_domain`.
/// The old `{scheme}://auth.{app_base_domain}/device` was a guess about where
/// the auth service lives: it is right only when the deployment happens to
/// front the OP at `auth.` under the app domain, and it ignored
/// `ZEROSHIP_AUTH_PLATFORM_ISSUER` entirely - the one value that is already
/// required to name the OP and is already validated at boot. Deriving it means
/// the URL the CLI prints is served by the same origin whose tokens control
/// verifies, by construction.
fn verification_uri(state: &AppState) -> Option<String> {
    let issuer = state.auth_provider.platform_issuer()?;
    let public_url = device_grant::op_public_url(issuer)?;
    Some(format!("{public_url}/device"))
}

fn verification_uri_complete(verification_uri: &str, user_code: &str) -> String {
    let mut url = url::Url::parse(verification_uri).expect("verification URI is absolute");
    url.query_pairs_mut().append_pair("user_code", user_code);
    url.to_string()
}

fn unauthorized() -> web::HttpResponse {
    web::HttpResponse::Unauthorized().json(&json!({"error": "unauthorized"}))
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

#[cfg(test)]
mod tests {
    use super::DEPLOY_TOKEN_SCOPES;

    #[test]
    fn deploy_tokens_can_read_secret_names() {
        assert!(DEPLOY_TOKEN_SCOPES.contains(&"secrets:read"));
    }
}
