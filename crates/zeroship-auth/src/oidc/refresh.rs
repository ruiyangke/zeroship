//! OP refresh rotation on the SESSION ROW: CLI/programmatic rotation and reuse
//! detection.
//!
//! The refresh FAMILY is one `zeroship.sessions` row whose secret rotates in
//! place - `secret_hash` and `prev_secret_hash` under the idle and absolute
//! expiry - and every statement that reads it lives in `crate::session_store`.
//! What is left here is the OAuth policy around those statements: client
//! authentication, scope narrowing, the response shape, and the transaction
//! and lock discipline the rotation runs under.
//!
//! **Nothing in this file mints.** Every mint goes through a
//! `session_store::ValidatedSession`, which only the validating statements
//! produce, so a rotation that skipped the read could not call the issuer even
//! if someone wrote it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use compio_postgres::{Client, GenericClient, Pool, PoolConfig, Transaction};
use ntex::web::{self, HttpRequest, HttpResponse};
use serde::Deserialize;
use zeroship_core::auth::validate_client_secret;
use zeroship_core::UserId;

use crate::advisory_lock::{lock_refresh_family_xact, lock_refresh_user_xact};
use crate::config::AuthConfig;
use crate::oidc::authorization_code::{
    clean_optional, load_client, parse_scopes, required_param, scope_subset, sort_dedup,
    OAuthClient, OAuthError, TokenRequest, TokenResponse, TOKEN_TYPE_BEARER,
};
use crate::oidc::{device_token, introspect, Issuer, ACCESS_TOKEN_TTL_SECS};
use crate::session_store::{
    self, Audience, PeekedSession, RotatedSession, SecretSlot, SessionKind, SessionRow,
    SessionSecretKeys, ValidatedSession,
};

/// The sliding idle window a rotation renews.
const FAMILY_IDLE_DAYS: i64 = 7;
/// The ceiling a session may never rotate past.
const FAMILY_ABSOLUTE_DAYS: i64 = 30;
/// How long a lost rotation response may be replayed.
const IDEM_WINDOW_SECS: i64 = 30;
/// How long a fully expired session is kept before the sweep deletes it.
const SESSION_RETENTION_DAYS: i64 = 30;
const REFRESH_POOL_ACQUIRE_TIMEOUT_SECS: u64 = 30;

thread_local! {
    static REFRESH_POOLS: RefCell<HashMap<RefreshPoolKey, Rc<Pool>>> =
        RefCell::new(HashMap::new());
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct RefreshPoolKey {
    db_url: String,
    pool_size: usize,
}

#[derive(Clone)]
pub struct RefreshSessionPool {
    inner: Arc<RefreshSessionPoolInner>,
}

struct RefreshSessionPoolInner {
    db_url: String,
    pool_size: usize,
    checkout_count: AtomicU64,
}

impl std::fmt::Debug for RefreshSessionPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefreshSessionPool")
            .field("pool_size", &self.inner.pool_size)
            .field(
                "checkout_count",
                &self.inner.checkout_count.load(Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

impl RefreshSessionPool {
    #[must_use]
    pub fn new(db_url: impl Into<String>, pool_size: usize) -> Self {
        Self {
            inner: Arc::new(RefreshSessionPoolInner {
                db_url: db_url.into(),
                pool_size: pool_size.max(1),
                checkout_count: AtomicU64::new(0),
            }),
        }
    }

    #[must_use]
    pub fn pool_size(&self) -> usize {
        self.inner.pool_size
    }

    #[doc(hidden)]
    #[must_use]
    pub fn checkout_count(&self) -> u64 {
        self.inner.checkout_count.load(Ordering::SeqCst)
    }

    pub(crate) async fn checkout_pool(
        &self,
        operation: &'static str,
    ) -> Result<Rc<Pool>, compio_postgres::Error> {
        self.inner.checkout_count.fetch_add(1, Ordering::SeqCst);
        let key = RefreshPoolKey {
            db_url: self.inner.db_url.clone(),
            pool_size: self.inner.pool_size,
        };
        if let Some(pool) = REFRESH_POOLS.with(|pools| pools.borrow().get(&key).cloned()) {
            return Ok(pool);
        }

        let mut pool_config = PoolConfig::new();
        pool_config
            .max_size(self.inner.pool_size)
            .min_idle(1)
            .acquire_timeout(Duration::from_secs(REFRESH_POOL_ACQUIRE_TIMEOUT_SECS));
        let pool = Rc::new(Pool::connect_with_pool_config(&self.inner.db_url, pool_config).await?);
        pool.start_housekeeper();
        let pool = REFRESH_POOLS.with(|pools| {
            let mut pools = pools.borrow_mut();
            if let Some(existing) = pools.get(&key) {
                return Rc::clone(existing);
            }
            pools.insert(key, Rc::clone(&pool));
            pool
        });
        tracing::debug!(operation, "refresh dedicated pool checked out");
        Ok(pool)
    }
}

struct PreauthenticatedRefresh {
    presented: PeekedSession,
    client: OAuthClient,
}

#[derive(Debug, Deserialize)]
pub struct RevokeRequest {
    pub token: Option<String>,
    pub token_type_hint: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
}

#[derive(Debug)]
pub(super) struct ClientAuth {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub method: ClientAuthMethod,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClientAuthMethod {
    None,
    Basic,
    Post,
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(web::resource("/revoke").route(web::post().to(revoke_post)));
    introspect::configure(cfg);
}

/// Load the session-secret keyring from the two configured secret files.
///
/// # Errors
///
/// `OAuthError::server_error` when either setting is unset or the material is
/// unusable. The message never names the file's contents.
pub(super) fn session_keys(cfg: &AuthConfig) -> Result<SessionSecretKeys, OAuthError> {
    let hash_file = cfg
        .refresh_hash_key_file()
        .ok_or_else(|| OAuthError::server_error("refresh hash key is not configured"))?;
    let idem_file = cfg
        .refresh_idem_key_file()
        .ok_or_else(|| OAuthError::server_error("refresh idempotency key is not configured"))?;
    SessionSecretKeys::from_files(hash_file, idem_file).map_err(|err| {
        tracing::error!(error = %err, "session secret key load failed");
        OAuthError::server_error("session secret keys unavailable")
    })
}

pub(super) fn client_auth_from_request(
    req: &HttpRequest,
    client_id: Option<&str>,
    client_secret: Option<&str>,
) -> ClientAuth {
    if let Some((id, secret)) = basic_client_auth(req) {
        return ClientAuth {
            client_id: Some(id),
            client_secret: Some(secret),
            method: ClientAuthMethod::Basic,
        };
    }
    ClientAuth {
        client_id: clean_optional(client_id.map(str::to_string)),
        client_secret: clean_optional(client_secret.map(str::to_string)),
        method: if client_secret.is_some_and(|secret| !secret.trim().is_empty()) {
            ClientAuthMethod::Post
        } else {
            ClientAuthMethod::None
        },
    }
}

// ---------------------------------------------------------------------------
// Establishing a session
// ---------------------------------------------------------------------------

/// What a grant produced: the credential to hand back, and the proof every mint
/// on this exchange needs.
pub(super) struct EstablishedSession {
    /// `None` when the grant carried no `offline_access`.
    pub secret: Option<String>,
    pub proof: ValidatedSession,
}

/// Create the session a token exchange mints from.
///
/// Every exchange creates a session; only the SECRET is conditional on
/// `offline_access`. A session with no secret can never be presented again
/// (`session_store::peek` cannot match a NULL hash), so its one and only mint is
/// the one the creating statement authorised. Minting only when `offline_access`
/// was granted would leave an access token against no stored object at all.
///
/// The audience and the subject are today's, unchanged. The first-party CLI
/// client is the PLATFORM audience and its subject is the person's own id,
/// because the `zeroship.token_revocations` marker a kill writes is looked up
/// by control under (`zeroship-cli`, principal id): a platform family storing
/// a pairwise subject would revoke a subject nothing ever presents. Every other
/// client is an APP audience with the pairwise subject over its sector.
/// The grant a token exchange is establishing a session for.
///
/// A bag rather than eight positional parameters: `user_id`, the scopes and the
/// credential version are three values a caller can transpose without the type
/// checker noticing, and the two booleans at the end were the worst of it.
pub(super) struct Establish<'a> {
    pub client: &'a OAuthClient,
    pub user_id: &'a UserId,
    pub granted_scopes: &'a [String],
    /// The credential epoch the authenticating event observed. The creating
    /// statement pins the session to it.
    pub auth_credential_version: i64,
    pub kind: SessionKind,
    /// Whether the caller is entitled to a rotating secret - `offline_access`
    /// granted and the client allowed to refresh.
    pub with_secret: bool,
}

#[allow(clippy::future_not_send)]
pub(super) async fn establish_session(
    db: &(impl GenericClient + ?Sized),
    issuer: &Issuer,
    keys: &SessionSecretKeys,
    params: &Establish<'_>,
) -> Result<EstablishedSession, OAuthError> {
    let Establish {
        client,
        user_id,
        granted_scopes,
        auth_credential_version,
        kind,
        with_secret,
    } = *params;
    lock_refresh_user_xact(db, user_id).await.map_err(|err| {
        tracing::error!(error = %err, user_id = user_id.as_str(), "session issuance user lock failed");
        OAuthError::server_error("session issuance unavailable")
    })?;

    let (audience, subject) =
        if device_token::platform_cli_policy_selected(db, &client.client_id).await? {
            (Audience::Platform, user_id.as_str().to_owned())
        } else {
            (
                Audience::App {
                    client_id: client.client_id.clone(),
                },
                issuer.pairwise_subject(user_id, &client.sector_identifier),
            )
        };

    let grant_id =
        session_store::upsert_grant(db, user_id, &audience, &subject, granted_scopes, None)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, user_id = user_id.as_str(), "grant upsert failed");
                OAuthError::server_error("session issuance unavailable")
            })?;

    let created = session_store::create(
        db,
        keys,
        &session_store::NewSession {
            person_id: user_id,
            grant_id: &grant_id,
            subject: &subject,
            grant_scopes: granted_scopes,
            parent_session_id: None,
            kind,
            scopes: granted_scopes,
            amr: &[],
            acr: None,
            label: None,
            expected_credential_epoch: Some(auth_credential_version),
            idle_days: FAMILY_IDLE_DAYS,
            absolute_days: FAMILY_ABSOLUTE_DAYS,
            with_secret,
        },
    )
    .await
    .map_err(|err| {
        tracing::error!(error = %err, user_id = user_id.as_str(), "session create failed");
        OAuthError::server_error("session issuance unavailable")
    })?;

    // The refusal arms are indistinguishable on purpose: the creating statement
    // returns no row for an inactive person, a moved credential epoch and a
    // suspended grant alike, and telling them apart at this boundary would leak
    // account state to an unauthenticated caller.
    let Some(created) = created else {
        return Err(OAuthError::invalid_grant("credential version changed"));
    };

    Ok(EstablishedSession {
        secret: created.secret,
        proof: created.proof,
    })
}

// ---------------------------------------------------------------------------
// Rotation
// ---------------------------------------------------------------------------

#[allow(clippy::future_not_send)]
pub(super) async fn exchange_refresh_token(
    shared_db: &Client,
    refresh_pool: &RefreshSessionPool,
    cfg: &AuthConfig,
    issuer: &Issuer,
    keys: &SessionSecretKeys,
    params: &TokenRequest,
    client_auth: &ClientAuth,
) -> Result<TokenResponse, OAuthError> {
    let preauth = preauthenticate_refresh(issuer, shared_db, keys, params, client_auth).await?;
    let pool = refresh_pool
        .checkout_pool("refresh rotation")
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "refresh: dedicated database pool checkout failed");
            OAuthError::server_error("refresh database unavailable")
        })?;
    let mut conn = pool.acquire().await.map_err(|err| {
        tracing::error!(error = %err, "refresh: dedicated database session checkout failed");
        OAuthError::server_error("refresh database unavailable")
    })?;
    let tx = conn.transaction().await.map_err(|err| {
        tracing::error!(error = %err, "refresh: BEGIN failed on dedicated session");
        OAuthError::server_error("refresh rotation unavailable")
    })?;
    let result = exchange_refresh_token_inner(&tx, cfg, issuer, keys, params, preauth).await;
    match result {
        Ok(response) => {
            tx.commit().await.map_err(|err| {
                tracing::error!(error = %err, "refresh: COMMIT failed");
                OAuthError::server_error("refresh rotation unavailable")
            })?;
            Ok(response)
        }
        // A refusal that KILLED the session has to commit the kill, which is
        // why an invalid_grant commits rather than rolling back.
        Err(err) if err.status == ntex::http::StatusCode::BAD_REQUEST => {
            tx.commit().await.map_err(|commit_err| {
                tracing::error!(error = %commit_err, oauth_error = err.error, "refresh: COMMIT failed");
                OAuthError::server_error("refresh rotation unavailable")
            })?;
            Err(err)
        }
        Err(err) => {
            if let Err(rollback) = tx.rollback().await {
                tracing::error!(error = %rollback, "refresh: ROLLBACK failed");
            }
            Err(err)
        }
    }
}

#[allow(clippy::future_not_send)]
async fn preauthenticate_refresh(
    issuer: &Issuer,
    db: &(impl GenericClient + ?Sized),
    keys: &SessionSecretKeys,
    params: &TokenRequest,
    client_auth: &ClientAuth,
) -> Result<PreauthenticatedRefresh, OAuthError> {
    let raw_token = required_param(params.refresh_token.as_deref(), "refresh_token")?;
    let client_id = authenticated_client_id(db, params.client_id.as_deref(), client_auth).await?;
    let client = load_client(db, &client_id).await?;
    authenticate_client(issuer, &client, client_auth)?;
    if !client.refresh_allowed {
        return Err(OAuthError::invalid_grant("refresh token is invalid"));
    }

    // A non-locking, non-validating resolve. It decides nothing: the row it
    // names is re-read under a lock and re-validated by the statement that
    // rotates it.
    let Some(presented) = session_store::peek(db, keys, raw_token)
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "refresh: session lookup failed");
            OAuthError::server_error("session store unavailable")
        })?
    else {
        return Err(OAuthError::invalid_grant("refresh token is invalid"));
    };

    Ok(PreauthenticatedRefresh { presented, client })
}

#[allow(clippy::future_not_send)]
async fn exchange_refresh_token_inner(
    db: &Transaction<'_>,
    cfg: &AuthConfig,
    issuer: &Issuer,
    keys: &SessionSecretKeys,
    params: &TokenRequest,
    preauth: PreauthenticatedRefresh,
) -> Result<TokenResponse, OAuthError> {
    let PreauthenticatedRefresh { presented, client } = preauth;

    lock_refresh_user_xact(db, &presented.person_id)
        .await
        .map_err(|err| {
            tracing::error!(
                error = %err,
                user_id = presented.person_id.as_str(),
                "refresh rotation user lock failed"
            );
            OAuthError::server_error("refresh rotation unavailable")
        })?;
    lock_refresh_family_xact(db, &presented.session_id)
        .await
        .map_err(|err| {
            tracing::error!(
                error = %err,
                session_id = %presented.session_id,
                "refresh rotation session lock failed"
            );
            OAuthError::server_error("refresh rotation unavailable")
        })?;

    let Some(row) = session_store::lock_and_read(db, &presented.session_id)
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "refresh: session read failed");
            OAuthError::server_error("session store unavailable")
        })?
    else {
        return Err(OAuthError::invalid_grant("refresh token is invalid"));
    };
    // The client that holds a session is the client the session was issued to.
    // A platform-audience session carries no client id and belongs to the
    // first-party CLI.
    let owning_client = row
        .client_id
        .clone()
        .unwrap_or_else(|| zeroship_core::device_grant::PLATFORM_CLI_CLIENT_ID.to_string());
    if owning_client != client.client_id {
        return Err(OAuthError::invalid_grant("refresh token is invalid"));
    }

    // THE SLOT IS RE-DERIVED HERE, NOT CARRIED FROM THE PEEK. `peek` ran before
    // the transaction and before the locks above, so a concurrent rotation can
    // commit in between: the secret that resolved as CURRENT is SUPERSEDED by
    // the time this lock is granted, and acting on the stale verdict refuses a
    // request that has earned the idempotent replay. See
    // `SessionRow::slot_for`.
    let Some(slot) = row.slot_for(&presented) else {
        // Neither slot matches any more: the session rotated at least twice
        // while this request waited, so the presented secret is older than the
        // one superseded slot can speak for. Unknown, not reuse - the horizon
        // note in `session_store` says why that is the honest verdict.
        return Err(OAuthError::invalid_grant("refresh token is invalid"));
    };
    if slot == SecretSlot::Superseded {
        return replay_or_kill(db, cfg, issuer, keys, &client, &presented, &row).await;
    }

    let new_scopes = requested_refresh_scopes(params.scope.as_deref(), &row.grant_scopes)?;
    let rotation = session_store::rotate(
        db,
        keys,
        &presented,
        &new_scopes,
        FAMILY_IDLE_DAYS,
        IDEM_WINDOW_SECS,
    )
    .await
    .map_err(|err| {
        tracing::error!(error = %err, "refresh: rotation failed");
        OAuthError::server_error("refresh rotation unavailable")
    })?;

    let Some(RotatedSession {
        row: rotated,
        secret,
        proof,
    }) = rotation
    else {
        // The row exists but the validating read refused it: revoked, expired,
        // suspended, or the person's credential epoch moved. One
        // indistinguishable refusal class, as C14 requires.
        return Err(OAuthError::invalid_grant("refresh token is invalid"));
    };

    let access_token = device_token::mint_grant_access_token(
        db,
        cfg,
        issuer,
        &client,
        &rotated.person_id,
        &new_scopes,
        &proof,
    )
    .await?;
    Ok(TokenResponse {
        access_token,
        id_token: None,
        refresh_token: Some(secret),
        token_type: TOKEN_TYPE_BEARER,
        expires_in: ACCESS_TOKEN_TTL_SECS as u64,
        scope: new_scopes.join(" "),
    })
}

/// A presentation of a SUPERSEDED secret is one of two things: the single retry
/// a lost response earns, or reuse. Serve the retry, then kill.
#[allow(clippy::future_not_send)]
async fn replay_or_kill(
    db: &Transaction<'_>,
    cfg: &AuthConfig,
    issuer: &Issuer,
    keys: &SessionSecretKeys,
    client: &OAuthClient,
    presented: &PeekedSession,
    row: &SessionRow,
) -> Result<TokenResponse, OAuthError> {
    let replayed = session_store::replay(db, keys, presented, row)
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "refresh: idempotent replay failed");
            OAuthError::server_error("refresh rotation unavailable")
        })?;

    // Every arm that declines to serve reaches the kill. Nothing above may
    // short-circuit with a refusal of its own: an early return would skip it,
    // so a record that will not open - which is what a rotated idempotency key
    // looks like - would disarm reuse detection for every session at once.
    let Some((cached, replayed_row, proof)) = replayed else {
        session_store::revoke(db, &row.id, "replay")
            .await
            .map_err(|err| {
                tracing::error!(error = %err, session_id = %row.id, "refresh: reuse kill failed");
                OAuthError::server_error("refresh revoke unavailable")
            })?;
        return Err(OAuthError::invalid_grant("refresh token is invalid"));
    };

    let scopes = parse_scopes(&cached.scope);
    let access_token = device_token::mint_grant_access_token(
        db,
        cfg,
        issuer,
        client,
        &replayed_row.person_id,
        &scopes,
        &proof,
    )
    .await?;
    tracing::info!(session_id = %replayed_row.id, "refresh idempotency replay recovered");
    Ok(TokenResponse {
        access_token,
        id_token: None,
        refresh_token: Some(cached.refresh_token),
        token_type: TOKEN_TYPE_BEARER,
        expires_in: ACCESS_TOKEN_TTL_SECS as u64,
        scope: scopes.join(" "),
    })
}

const REFRESH_PRINCIPAL_ACTIVE_SQL: &str = "SELECT 1 FROM zeroship.users \
     WHERE id = $1 \
       AND disabled_at IS NULL \
       AND anonymized_at IS NULL \
       AND deletion_requested_at IS NULL \
       AND deletion_scheduled_for IS NULL";

async fn refresh_user_active(
    db: &(impl GenericClient + ?Sized),
    user_id: &UserId,
) -> Result<bool, OAuthError> {
    let rows = db
        .query(REFRESH_PRINCIPAL_ACTIVE_SQL, &[&user_id.as_str()])
        .await
        .map_err(|err| {
            tracing::error!(error = %err, user_id = user_id.as_str(), "refresh lifecycle lookup failed");
            OAuthError::server_error("refresh lifecycle unavailable")
        })?;
    Ok(!rows.is_empty())
}

// ---------------------------------------------------------------------------
// Revocation
// ---------------------------------------------------------------------------

#[allow(clippy::future_not_send)]
pub async fn revoke_post(
    req: HttpRequest,
    form: web::types::Form<RevokeRequest>,
    cfg: web::types::State<Arc<AuthConfig>>,
    db: web::types::State<Arc<Client>>,
    issuer: web::types::State<Arc<Issuer>>,
    refresh_pool: web::types::State<RefreshSessionPool>,
) -> HttpResponse {
    match revoke_inner(
        &req,
        form.into_inner(),
        cfg.as_ref(),
        db.as_ref(),
        issuer.as_ref(),
        refresh_pool.get_ref(),
    )
    .await
    {
        Ok(()) => HttpResponse::Ok()
            .header("cache-control", "no-store")
            .header("pragma", "no-cache")
            .finish(),
        Err(err) if err.error == "invalid_client" => {
            super::authorization_code::client_auth_error_response(err)
        }
        Err(err) if err.status == ntex::http::StatusCode::BAD_REQUEST => HttpResponse::Ok()
            .header("cache-control", "no-store")
            .header("pragma", "no-cache")
            .finish(),
        Err(err) => super::authorization_code::oauth_error_response(err),
    }
}

#[allow(clippy::future_not_send)]
async fn revoke_inner(
    req: &HttpRequest,
    form: RevokeRequest,
    cfg: &AuthConfig,
    db: &Client,
    issuer: &Issuer,
    refresh_pool: &RefreshSessionPool,
) -> Result<(), OAuthError> {
    let _hint = form.token_type_hint.as_deref();
    let Some(raw_token) = form
        .token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return Ok(());
    };
    let client_auth = client_auth_from_request(
        req,
        form.client_id.as_deref(),
        form.client_secret.as_deref(),
    );
    let keys = session_keys(cfg)?;
    let client_id = match authenticated_client_id(db, form.client_id.as_deref(), &client_auth).await
    {
        Ok(client_id) => client_id,
        Err(err) if err.error == "invalid_client" => return Err(err),
        Err(_) => return Ok(()),
    };
    let client = load_client(db, &client_id).await?;
    authenticate_client(issuer, &client, &client_auth)?;
    if let Ok(claims) = issuer.verify_access_token(raw_token) {
        if claims.client_id == client.client_id {
            revoke_sessions_for_subject(refresh_pool, &claims.client_id, &claims.sub).await?;
        }
        return Ok(());
    }
    let Some(presented) = session_store::peek(db, &keys, raw_token)
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "revoke: session lookup failed");
            OAuthError::server_error("revoke unavailable")
        })?
    else {
        return Ok(());
    };
    let owning_client = presented
        .client_id
        .clone()
        .unwrap_or_else(|| zeroship_core::device_grant::PLATFORM_CLI_CLIENT_ID.to_string());
    if owning_client != client.client_id {
        return Ok(());
    }

    let pool = refresh_pool
        .checkout_pool("refresh revoke")
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "revoke: dedicated database pool checkout failed");
            OAuthError::server_error("revoke unavailable")
        })?;
    let mut conn = pool.acquire().await.map_err(|err| {
        tracing::error!(error = %err, "revoke: dedicated database session checkout failed");
        OAuthError::server_error("revoke unavailable")
    })?;
    let tx = conn.transaction().await.map_err(|err| {
        tracing::error!(error = %err, "revoke: BEGIN failed on dedicated session");
        OAuthError::server_error("revoke unavailable")
    })?;
    let result = async {
        lock_refresh_user_xact(&tx, &presented.person_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, user_id = presented.person_id.as_str(), "revoke user lock failed");
                OAuthError::server_error("revoke unavailable")
            })?;
        lock_refresh_family_xact(&tx, &presented.session_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, session_id = %presented.session_id, "revoke session lock failed");
                OAuthError::server_error("revoke unavailable")
            })?;
        session_store::revoke(&tx, &presented.session_id, "revoke")
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "revoke: session revoke failed");
                OAuthError::server_error("revoke unavailable")
            })
    }
    .await;
    match result {
        Ok(()) => {
            tx.commit().await.map_err(|err| {
                tracing::error!(error = %err, "revoke: COMMIT failed");
                OAuthError::server_error("revoke unavailable")
            })?;
            Ok(())
        }
        Err(err) => {
            if let Err(rollback) = tx.rollback().await {
                tracing::error!(error = %rollback, "revoke: ROLLBACK failed");
            }
            Err(err)
        }
    }
}

/// End every session a person holds, on a dedicated connection.
///
/// # Errors
///
/// A message naming the reason when the pool, the transaction or the statement
/// fails.
pub async fn revoke_person_sessions(
    refresh_pool: &RefreshSessionPool,
    user_id: &UserId,
    reason: &'static str,
) -> Result<(), String> {
    let pool = refresh_pool
        .checkout_pool("refresh user revoke")
        .await
        .map_err(|err| format!("refresh user revoke pool checkout ({reason}): {err}"))?;
    let mut conn = pool
        .acquire()
        .await
        .map_err(|err| format!("refresh user revoke session checkout ({reason}): {err}"))?;
    let tx = conn
        .transaction()
        .await
        .map_err(|err| format!("refresh user revoke begin ({reason}): {err}"))?;
    let result = async {
        lock_refresh_user_xact(&tx, user_id)
            .await
            .map_err(|err| format!("refresh user revoke lock: {err}"))?;
        revoke_person_sessions_in_transaction(&tx, user_id, reason).await
    }
    .await;
    match result {
        Ok(()) => tx
            .commit()
            .await
            .map_err(|err| format!("refresh user revoke commit ({reason}): {err}"))?,
        Err(err) => {
            let _ = tx.rollback().await;
            return Err(err);
        }
    }
    tracing::info!(
        user_id = user_id.as_str(),
        reason,
        "sessions revoked for user"
    );
    Ok(())
}

/// Revoke every live session a person holds, inside the caller's transaction.
///
/// The caller must first hold [`lock_refresh_user_xact`] on this transaction.
/// The row updates and the access-token markers ride one statement, so either
/// both effects commit or neither does.
///
/// # Errors
///
/// A message naming the reason when the statement fails.
pub(crate) async fn revoke_person_sessions_in_transaction(
    db: &(impl GenericClient + ?Sized),
    user_id: &UserId,
    reason: &'static str,
) -> Result<(), String> {
    session_store::revoke_person_sessions(db, user_id, reason)
        .await
        .map(|_| ())
}

/// End every session whose grant names this (client, subject) pair.
async fn revoke_sessions_for_subject(
    refresh_pool: &RefreshSessionPool,
    client_id: &str,
    sub: &str,
) -> Result<(), OAuthError> {
    let pool = refresh_pool
        .checkout_pool("access-token revoke")
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "access-token revoke: dedicated database pool checkout failed");
            OAuthError::server_error("revoke unavailable")
        })?;
    let mut conn = pool.acquire().await.map_err(|err| {
        tracing::error!(error = %err, "access-token revoke: dedicated database session checkout failed");
        OAuthError::server_error("revoke unavailable")
    })?;
    let tx = conn.transaction().await.map_err(|err| {
        tracing::error!(error = %err, "access-token revoke: BEGIN failed on dedicated session");
        OAuthError::server_error("revoke unavailable")
    })?;
    let result = revoke_sessions_for_subject_in_transaction(&tx, client_id, sub).await;

    match result {
        Ok(()) => {
            tx.commit().await.map_err(|err| {
                tracing::error!(error = %err, "access-token revoke: COMMIT failed");
                OAuthError::server_error("revoke unavailable")
            })?;
            Ok(())
        }
        Err(err) => {
            if let Err(rollback) = tx.rollback().await {
                tracing::error!(error = %rollback, "access-token revoke: ROLLBACK failed");
            }
            Err(err)
        }
    }
}

/// The same, inside the caller's transaction.
///
/// The person and session locks are taken in a deterministic order before any
/// write, which is what keeps two concurrent revokes from deadlocking.
///
/// # Errors
///
/// `OAuthError::server_error` when a lock or the write fails.
pub(super) async fn revoke_sessions_for_subject_in_transaction(
    db: &(impl GenericClient + ?Sized),
    client_id: &str,
    sub: &str,
) -> Result<(), OAuthError> {
    let rows = db
        .query(
            "SELECT DISTINCT s.person_id, s.id \
             FROM zeroship.sessions s \
             JOIN zeroship.grants g ON g.id = s.grant_id \
             WHERE COALESCE(g.client_id, $1) = $2 AND g.subject = $3 \
             ORDER BY s.person_id, s.id",
            &[
                &zeroship_core::device_grant::PLATFORM_CLI_CLIENT_ID,
                &client_id,
                &sub,
            ],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, client_id, sub, "access-token revoke: session lookup failed");
            OAuthError::server_error("revoke unavailable")
        })?;

    let mut person_ids = Vec::new();
    let mut session_ids = Vec::new();
    for row in rows {
        let person_id = crate::entity_ids::user_id_with_context(
            &row,
            "person_id",
            "session person_id is invalid",
        )
        .map_err(|err| {
            tracing::error!(error = %err, "access-token revoke: person_id decode failed");
            OAuthError::server_error("revoke unavailable")
        })?;
        person_ids.push(person_id);
        session_ids.push(row.get::<_, String>("id"));
    }
    person_ids.sort();
    person_ids.dedup();
    session_ids.sort();
    session_ids.dedup();

    for person_id in person_ids {
        lock_refresh_user_xact(db, &person_id)
            .await
            .map_err(|err| {
                tracing::error!(
                    error = %err,
                    user_id = person_id.as_str(),
                    "access-token revoke: user lock failed"
                );
                OAuthError::server_error("revoke unavailable")
            })?;
    }
    for session_id in &session_ids {
        lock_refresh_family_xact(db, session_id)
            .await
            .map_err(|err| {
                tracing::error!(
                    error = %err,
                    session_id = %session_id,
                    "access-token revoke: session lock failed"
                );
                OAuthError::server_error("revoke unavailable")
            })?;
    }

    for session_id in &session_ids {
        session_store::revoke(db, session_id, "access-token revoke")
            .await
            .map_err(|err| {
                tracing::error!(error = %err, session_id, "access-token revoke failed");
                OAuthError::server_error("revoke unavailable")
            })?;
    }
    tracing::info!(client_id, sub, "sessions revoked for subject");
    Ok(())
}

/// Delete expired sessions and clear spent idempotent records.
///
/// Returns `(deleted, idempotent records cleared)`.
///
/// This takes NO per-person advisory lock. It does not need one: the two
/// statements this runs touch only rows whose deadlines have already passed, and
/// a row a rotation could still validate is out of both predicates by
/// construction. A single statement taking only row locks cannot deadlock
/// against a rotation that takes them in the same
/// order.
///
/// # Errors
///
/// A message when the pool, the session checkout or either statement fails.
pub async fn sweep_sessions(refresh_pool: &RefreshSessionPool) -> Result<(u64, u64), String> {
    let pool = refresh_pool
        .checkout_pool("session sweep")
        .await
        .map_err(|err| format!("session sweep pool checkout: {err}"))?;
    let conn = pool
        .acquire()
        .await
        .map_err(|err| format!("session sweep session checkout: {err}"))?;
    session_store::sweep(&*conn, SESSION_RETENTION_DAYS).await
}

// ---------------------------------------------------------------------------
// Client authentication
// ---------------------------------------------------------------------------

pub(super) async fn authenticated_client_id(
    db: &(impl GenericClient + ?Sized),
    request_client_id: Option<&str>,
    client_auth: &ClientAuth,
) -> Result<String, OAuthError> {
    let form_client_id = clean_optional(request_client_id.map(str::to_string));
    let auth_client_id = client_auth.client_id.clone();
    match (auth_client_id, form_client_id) {
        (Some(auth_id), Some(form_id)) if auth_id != form_id => {
            Err(OAuthError::invalid_client("client authentication mismatch"))
        }
        (Some(auth_id), _) => Ok(auth_id),
        (None, Some(form_id)) => {
            let _ = load_client(db, &form_id).await?;
            Ok(form_id)
        }
        (None, None) => Err(OAuthError::invalid_client("client_id is required")),
    }
}

/// Authenticate the client behind a token-endpoint request, by the credential
/// it was actually registered with.
///
/// Shared by every grant and every client-authenticated endpoint
/// (authorization_code, refresh_token, introspect, revoke): RFC 6749 4.1.3
/// requires a client issued credentials to present them on ALL of them, so the
/// authorization_code grant cannot have a laxer rule than the refresh grant for
/// the same client. `token_endpoint_auth_method` is what decides, not
/// `brokered`: the DB CHECK only pins `brokered => client_secret_basic`, so a
/// confidential client can perfectly well be non-brokered.
pub(super) fn authenticate_client(
    issuer: &Issuer,
    client: &OAuthClient,
    client_auth: &ClientAuth,
) -> Result<(), OAuthError> {
    // Brokered-first: a brokered client authenticates by derive-and-compare
    // against the per-app broker secret, NOT a stored hash (it has none:
    // client_secret_hash is NULL). Without this branch the client_secret_basic
    // arm below would verify against the NULL hash and brokered auth would be
    // a fail-closed dead-end (the gateway's oac_ clients keep the refresh anchor).
    if client.brokered {
        return crate::oidc::authorization_code::authenticate_brokered_client(
            issuer,
            client,
            client_auth,
        );
    }
    match client.token_endpoint_auth_method.as_str() {
        "none" => {
            if client_auth.method != ClientAuthMethod::None {
                return Err(OAuthError::invalid_client(
                    "public client must not authenticate with a secret",
                ));
            }
            Ok(())
        }
        "client_secret_basic" => {
            if client_auth.method != ClientAuthMethod::Basic {
                return Err(OAuthError::invalid_client("client_secret_basic required"));
            }
            verify_client_secret(client, client_auth)
        }
        "client_secret_post" => {
            if client_auth.method != ClientAuthMethod::Post {
                return Err(OAuthError::invalid_client("client_secret_post required"));
            }
            verify_client_secret(client, client_auth)
        }
        _ => Err(OAuthError::invalid_client(
            "unsupported client authentication method",
        )),
    }
}

#[derive(Debug, Clone)]
pub(super) struct ActiveRefreshToken {
    pub scope: String,
    pub client_id: String,
    pub token_type: &'static str,
    pub exp: i64,
    pub iat: i64,
    pub sub: String,
    pub aud: String,
}

/// Introspect a presented session secret.
///
/// This mints nothing, so it takes no witness and produces none. It is a READ,
/// and it answers `None` for anything a rotation would refuse - which is why it
/// repeats those predicates rather than sharing the rotating statement.
#[allow(clippy::future_not_send)]
pub(super) async fn introspect_refresh_token(
    db: &(impl GenericClient + ?Sized),
    keys: &SessionSecretKeys,
    authenticated_client: &OAuthClient,
    raw_token: &str,
) -> Result<Option<ActiveRefreshToken>, OAuthError> {
    let Some(presented) = session_store::peek(db, keys, raw_token)
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "introspect: session lookup failed");
            OAuthError::server_error("session store unavailable")
        })?
    else {
        return Ok(None);
    };
    // Only the LIVE secret is an active token. A superseded one is at best a
    // spent replay record.
    if presented.slot() != SecretSlot::Current {
        return Ok(None);
    }
    let owning_client = presented
        .client_id
        .clone()
        .unwrap_or_else(|| zeroship_core::device_grant::PLATFORM_CLI_CLIENT_ID.to_string());
    if owning_client != authenticated_client.client_id {
        return Ok(None);
    }
    let Some(row) = session_store::lock_and_read(db, &presented.session_id)
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "introspect: session read failed");
            OAuthError::server_error("session store unavailable")
        })?
    else {
        return Ok(None);
    };
    let now = chrono::Utc::now();
    if row.revoked_at.is_some() || row.idle_expires_at <= now || row.absolute_expires_at <= now {
        return Ok(None);
    }
    if !refresh_user_active(db, &row.person_id).await? {
        return Ok(None);
    }
    Ok(Some(ActiveRefreshToken {
        scope: row.scopes.join(" "),
        client_id: owning_client,
        token_type: "refresh_token",
        exp: row.idle_expires_at.timestamp(),
        iat: row.rotated_at.unwrap_or(row.created_at).timestamp(),
        sub: row.subject,
        aud: authenticated_client.resource_audience(),
    }))
}

fn verify_client_secret(client: &OAuthClient, client_auth: &ClientAuth) -> Result<(), OAuthError> {
    let Some(stored_hash) = client.client_secret_hash.as_deref() else {
        return Err(OAuthError::invalid_client(
            "client secret is not configured",
        ));
    };
    let Some(secret) = client_auth.client_secret.as_deref() else {
        return Err(OAuthError::invalid_client("client secret is required"));
    };
    if validate_client_secret(secret, stored_hash) {
        Ok(())
    } else {
        Err(OAuthError::invalid_client("client authentication failed"))
    }
}

fn requested_refresh_scopes(
    requested: Option<&str>,
    family_granted_scopes: &[String],
) -> Result<Vec<String>, OAuthError> {
    match requested {
        Some(scope) if !scope.trim().is_empty() => {
            let scopes = parse_scopes(scope);
            if !scope_subset(&scopes, family_granted_scopes) {
                return Err(OAuthError::invalid_scope("scope exceeds original grant"));
            }
            Ok(scopes)
        }
        _ => Ok(sort_dedup(family_granted_scopes.to_vec())),
    }
}

fn basic_client_auth(req: &HttpRequest) -> Option<(String, String)> {
    let header = req
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())?;
    let encoded = header.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (id, secret) = decoded.split_once(':')?;
    Some((id.to_string(), secret.to_string()))
}

#[cfg(test)]
mod lifecycle_introspection_tests {
    use super::*;

    #[test]
    fn refresh_introspection_uses_hard_lifecycle_without_soft_lockout() {
        for column in [
            "disabled_at",
            "anonymized_at",
            "deletion_requested_at",
            "deletion_scheduled_for",
        ] {
            assert!(
                REFRESH_PRINCIPAL_ACTIVE_SQL.contains(&format!("{column} IS NULL")),
                "missing active lifecycle predicate for {column}"
            );
        }
        assert!(!REFRESH_PRINCIPAL_ACTIVE_SQL.contains("locked_until"));
    }
}
