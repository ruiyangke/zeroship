//! OP refresh-token families: CLI/programmatic rotation + reuse detection.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Utc};
use compio_postgres::{Client, GenericClient, Pool, PoolConfig};
use ntex::web::{self, HttpRequest, HttpResponse};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroship_core::auth::{hash_api_key, hmac_sha256, validate_api_key};
use zeroship_core::crypto;
use zeroship_core::typed_id;

use crate::advisory_lock::{lock_refresh_family_xact, lock_refresh_user_xact};
use crate::config::AuthConfig;
use crate::oidc::authorization_code::{
    clean_optional, load_client, mint_access_token, parse_scopes, required_param, scope_subset,
    sort_dedup, OAuthClient, OAuthError, TokenRequest, TokenResponse, TOKEN_TYPE_BEARER,
};
use crate::oidc::{introspect, Issuer, ACCESS_TOKEN_TTL_SECS};

const REFRESH_TOKEN_BYTES: usize = 32;
const REFRESH_TOKEN_PREFIX: &str = "zrt_";
const FAMILY_IDLE_DAYS: i64 = 7;
const FAMILY_ABSOLUTE_DAYS: i64 = 30;
const IDEM_WINDOW_SECS: i64 = 30;
const IDEM_AAD_PREFIX: &[u8] = b"zs:auth:refresh_idem:v1\0";
const REFRESH_POOL_CONNECTION_TIMEOUT_SECS: u64 = 30;

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

        let config = PoolConfig {
            max_size: self.inner.pool_size,
            min_idle: 1,
            connection_timeout: Duration::from_secs(REFRESH_POOL_CONNECTION_TIMEOUT_SECS),
            ..PoolConfig::default()
        };
        let pool = Rc::new(Pool::connect_with_config(&self.inner.db_url, config).await?);
        pool.start_housekeeper();
        let pool = REFRESH_POOLS.with(|pools| {
            let mut pools = pools.borrow_mut();
            if let Some(existing) = pools.get(&key) {
                return Rc::clone(existing);
            }
            pools.insert(key, Rc::clone(&pool));
            pool
        });
        tracing::info!(
            operation,
            pool_size = self.inner.pool_size,
            "refresh dedicated session pool ready"
        );
        Ok(pool)
    }
}

#[derive(Debug, Clone)]
struct PreauthenticatedRefresh {
    presented_hash: TokenHash,
    initial: RefreshRow,
    client: OAuthClient,
}

#[derive(Debug, Clone)]
pub(super) struct RefreshTokenKeys {
    active: RefreshHashKey,
    verify: Vec<RefreshHashKey>,
    idem_key: [u8; 32],
}

#[derive(Debug, Clone)]
struct RefreshHashKey {
    version: i16,
    key: Vec<u8>,
}

#[derive(Debug, Clone)]
struct TokenHash {
    version: i16,
    hash: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedRefreshResponse {
    refresh_token: String,
    scope: String,
}

#[derive(Debug, Clone)]
struct RefreshRow {
    token_hash: Vec<u8>,
    hash_key_version: i16,
    refresh_family_id: String,
    replaced_by_token_hash: Option<Vec<u8>>,
    client_id: String,
    user_id: Uuid,
    sub: String,
    granted_scopes: Vec<String>,
    family_granted_scopes: Vec<String>,
    issued_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    family_absolute_expires_at: DateTime<Utc>,
    rotated_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
    idem_response_enc: Option<Vec<u8>>,
    idem_expires_at: Option<DateTime<Utc>>,
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

impl RefreshTokenKeys {
    pub(super) fn from_config(cfg: &AuthConfig) -> Result<Self, OAuthError> {
        let hash_file = cfg.refresh_hash_key_file.as_deref().ok_or_else(|| {
            OAuthError::server_error("refresh hash key is not configured")
        })?;
        let idem_file = cfg.refresh_idem_key_file.as_deref().ok_or_else(|| {
            OAuthError::server_error("refresh idempotency key is not configured")
        })?;
        Self::from_files(hash_file, idem_file).map_err(|err| {
            tracing::error!(error = %err, "refresh token key load failed");
            OAuthError::server_error("refresh token keys unavailable")
        })
    }

    pub fn from_files(hash_file: &Path, idem_file: &Path) -> Result<Self, String> {
        let mut verify = load_hash_keyring(hash_file)?;
        verify.sort_by(|a, b| b.version.cmp(&a.version));
        let Some(active) = verify.first().cloned() else {
            return Err(format!(
                "REFRESH_HASH_KEY_FILE {} yielded no keys",
                hash_file.display()
            ));
        };
        let idem_secret = read_secret_file(idem_file, "REFRESH_IDEM_KEY_FILE")?;
        let idem_key = crypto::derive_key(&String::from_utf8_lossy(&idem_secret));
        Ok(Self {
            active,
            verify,
            idem_key,
        })
    }

    fn active_hash(&self, raw: &str) -> TokenHash {
        TokenHash {
            version: self.active.version,
            hash: hmac_sha256(&self.active.key, raw.as_bytes()).to_vec(),
        }
    }

    fn hashes_newest_first(&self, raw: &str) -> Vec<TokenHash> {
        self.verify
            .iter()
            .map(|key| TokenHash {
                version: key.version,
                hash: hmac_sha256(&key.key, raw.as_bytes()).to_vec(),
            })
            .collect()
    }

    fn seal_cached_response(
        &self,
        predecessor_hash: &[u8],
        family_id: &str,
        body: &CachedRefreshResponse,
    ) -> Result<Vec<u8>, OAuthError> {
        let aad = idem_aad(predecessor_hash, family_id);
        let plain = serde_json::to_vec(body).map_err(|err| {
            tracing::error!(error = %err, "refresh: encode idempotency cache failed");
            OAuthError::server_error("refresh idempotency cache failed")
        })?;
        crypto::encrypt(&self.idem_key, &aad, &plain).map_err(|err| {
            tracing::error!(error = %err, "refresh: seal idempotency cache failed");
            OAuthError::server_error("refresh idempotency cache failed")
        })
    }

    fn open_cached_response(
        &self,
        predecessor_hash: &[u8],
        family_id: &str,
        enc: &[u8],
    ) -> Result<CachedRefreshResponse, OAuthError> {
        let aad = idem_aad(predecessor_hash, family_id);
        let plain = crypto::decrypt(&self.idem_key, &aad, enc).map_err(|err| {
            tracing::warn!(error = %err, "refresh: idempotency cache decrypt failed");
            OAuthError::invalid_grant("refresh token is invalid")
        })?;
        serde_json::from_slice(&plain).map_err(|err| {
            tracing::warn!(error = %err, "refresh: idempotency cache decode failed");
            OAuthError::invalid_grant("refresh token is invalid")
        })
    }
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

#[allow(clippy::future_not_send)]
pub(super) async fn issue_root_refresh_token(
    db: &(impl GenericClient + ?Sized),
    issuer: &Issuer,
    keys: &RefreshTokenKeys,
    client: &OAuthClient,
    user_id: Uuid,
    granted_scopes: &[String],
    auth_credential_version: i64,
) -> Result<String, OAuthError> {
    lock_refresh_user_xact(db, user_id).await.map_err(|err| {
        tracing::error!(error = %err, user_id = %user_id, "refresh issuance user lock failed");
        OAuthError::server_error("refresh issuance unavailable")
    })?;

    let rows = db
        .query(
            "SELECT credential_version FROM zeroship.users WHERE id = $1",
            &[&user_id],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, user_id = %user_id, "refresh issuance credential_version read failed");
            OAuthError::server_error("refresh issuance unavailable")
        })?;
    let Some(row) = rows.first() else {
        return Err(OAuthError::invalid_grant("authenticated user no longer exists"));
    };
    let current_version: i64 = row.get("credential_version");
    if current_version != auth_credential_version {
        return Err(OAuthError::invalid_grant("credential version changed"));
    }

    let raw = generate_refresh_token();
    let hash = keys.active_hash(&raw);
    let family_id = typed_id::generate("rfam");
    let user_id_string = user_id.to_string();
    let sub = issuer.pairwise_subject(&user_id_string, &client.sector_identifier);
    db.execute(
        "INSERT INTO zeroship.oauth_refresh_tokens \
            (token_hash, hash_key_version, refresh_family_id, client_id, user_id, sub, \
             granted_scopes, family_granted_scopes, expires_at, family_absolute_expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $7, \
                 LEAST(NOW() + make_interval(days => $8::INT), \
                       NOW() + make_interval(days => $9::INT)), \
                 NOW() + make_interval(days => $9::INT))",
        &[
            &hash.hash,
            &hash.version,
            &family_id,
            &client.client_id,
            &user_id,
            &sub,
            &granted_scopes,
            &i32::try_from(FAMILY_IDLE_DAYS).unwrap_or(7),
            &i32::try_from(FAMILY_ABSOLUTE_DAYS).unwrap_or(30),
        ],
    )
    .await
    .map_err(|err| {
        tracing::error!(error = %err, family_id = %family_id, "refresh root insert failed");
        OAuthError::server_error("refresh issuance unavailable")
    })?;
    Ok(raw)
}

#[allow(clippy::future_not_send)]
pub(super) async fn exchange_refresh_token(
    shared_db: &Client,
    refresh_pool: &RefreshSessionPool,
    issuer: &Issuer,
    keys: &RefreshTokenKeys,
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
    let mut conn = pool.get().await.map_err(|err| {
        tracing::error!(error = %err, "refresh: dedicated database session checkout failed");
        OAuthError::server_error("refresh database unavailable")
    })?;
    let tx = conn.transaction().await.map_err(|err| {
        tracing::error!(error = %err, "refresh: BEGIN failed on dedicated session");
        OAuthError::server_error("refresh rotation unavailable")
    })?;
    let result = exchange_refresh_token_inner(&tx, issuer, keys, params, preauth).await;
    match result {
        Ok(response) => {
            tx.commit().await.map_err(|err| {
                tracing::error!(error = %err, "refresh: COMMIT failed");
                OAuthError::server_error("refresh rotation unavailable")
            })?;
            Ok(response)
        }
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
    keys: &RefreshTokenKeys,
    params: &TokenRequest,
    client_auth: &ClientAuth,
) -> Result<PreauthenticatedRefresh, OAuthError> {
    let raw_token = required_param(params.refresh_token.as_deref(), "refresh_token")?;
    let client_id = authenticated_client_id(db, params.client_id.as_deref(), client_auth).await?;
    let client = load_client(db, &client_id).await?;
    authenticate_for_refresh(issuer, &client, client_auth).await?;
    if !client.refresh_allowed {
        return Err(OAuthError::invalid_grant("refresh token is invalid"));
    }

    let Some((presented_hash, initial)) = lookup_by_any_hash(db, keys, raw_token).await? else {
        return Err(OAuthError::invalid_grant("refresh token is invalid"));
    };

    Ok(PreauthenticatedRefresh {
        presented_hash,
        initial,
        client,
    })
}

#[allow(clippy::future_not_send)]
async fn exchange_refresh_token_inner(
    db: &(impl GenericClient + ?Sized),
    issuer: &Issuer,
    keys: &RefreshTokenKeys,
    params: &TokenRequest,
    preauth: PreauthenticatedRefresh,
) -> Result<TokenResponse, OAuthError> {
    let PreauthenticatedRefresh {
        presented_hash,
        initial,
        client,
    } = preauth;

    lock_refresh_user_xact(db, initial.user_id).await.map_err(|err| {
        tracing::error!(
            error = %err,
            user_id = %initial.user_id,
            "refresh rotation user lock failed"
        );
        OAuthError::server_error("refresh rotation unavailable")
    })?;
    lock_refresh_family_xact(db, &initial.refresh_family_id)
        .await
        .map_err(|err| {
            tracing::error!(
                error = %err,
                family_id = %initial.refresh_family_id,
                "refresh rotation family lock failed"
            );
            OAuthError::server_error("refresh rotation unavailable")
        })?;

    let Some(row) = select_refresh_row_for_update(db, &presented_hash.hash).await? else {
        return Err(OAuthError::invalid_grant("refresh token is invalid"));
    };
    if row.hash_key_version != presented_hash.version {
        return Err(OAuthError::invalid_grant("refresh token is invalid"));
    }
    if row.client_id != client.client_id {
        return Err(OAuthError::invalid_grant("refresh token is invalid"));
    }

    if row.rotated_at.is_some() {
        return replay_or_kill(db, issuer, keys, &client, &row).await;
    }

    if row.revoked_at.is_some()
        || family_has_revoked_row(db, &row.refresh_family_id).await?
        || row.expires_at <= Utc::now()
        || row.family_absolute_expires_at <= Utc::now()
    {
        return Err(OAuthError::invalid_grant("refresh token is invalid"));
    }

    let new_scopes = requested_refresh_scopes(params.scope.as_deref(), &row.family_granted_scopes)?;
    let new_raw = generate_refresh_token();
    let new_hash = keys.active_hash(&new_raw);
    let cached = CachedRefreshResponse {
        refresh_token: new_raw.clone(),
        scope: new_scopes.join(" "),
    };
    let idem_response_enc =
        keys.seal_cached_response(&row.token_hash, &row.refresh_family_id, &cached)?;

    let consumed = db
        .query(
            "UPDATE zeroship.oauth_refresh_tokens \
             SET rotated_at = NOW(), \
                 consumed_at = NOW(), \
                 last_used_at = NOW(), \
                 replaced_by_token_hash = $2, \
                 idem_response_enc = $3, \
                 idem_expires_at = NOW() + make_interval(secs => $4::INT) \
             WHERE token_hash = $1 \
               AND rotated_at IS NULL \
               AND revoked_at IS NULL \
             RETURNING refresh_family_id",
            &[
                &row.token_hash,
                &new_hash.hash,
                &idem_response_enc,
                &i32::try_from(IDEM_WINDOW_SECS).unwrap_or(30),
            ],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "refresh token consume failed");
            OAuthError::server_error("refresh rotation unavailable")
        })?;
    if consumed.is_empty() {
        kill_family(db, &row.refresh_family_id, "race").await?;
        return Err(OAuthError::invalid_grant("refresh token is invalid"));
    }

    db.execute(
        "INSERT INTO zeroship.oauth_refresh_tokens \
            (token_hash, hash_key_version, refresh_family_id, client_id, user_id, sub, \
             granted_scopes, family_granted_scopes, expires_at, family_absolute_expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, \
                 LEAST(NOW() + make_interval(days => $9::INT), $10), $10)",
        &[
            &new_hash.hash,
            &new_hash.version,
            &row.refresh_family_id,
            &row.client_id,
            &row.user_id,
            &row.sub,
            &new_scopes,
            &row.family_granted_scopes,
            &i32::try_from(FAMILY_IDLE_DAYS).unwrap_or(7),
            &row.family_absolute_expires_at,
        ],
    )
    .await
    .map_err(|err| {
        tracing::error!(error = %err, family_id = %row.refresh_family_id, "refresh child insert failed");
        OAuthError::server_error("refresh rotation unavailable")
    })?;

    let access_token = mint_access_token(issuer, &client, row.user_id, &new_scopes)?;
    Ok(TokenResponse {
        access_token,
        id_token: None,
        refresh_token: Some(new_raw),
        token_type: TOKEN_TYPE_BEARER,
        expires_in: ACCESS_TOKEN_TTL_SECS as u64,
        scope: new_scopes.join(" "),
    })
}

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
            super::authorization_code::oauth_error_response(err)
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
    let Some(raw_token) = form.token.as_deref().map(str::trim).filter(|t| !t.is_empty()) else {
        return Ok(());
    };
    let client_auth =
        client_auth_from_request(req, form.client_id.as_deref(), form.client_secret.as_deref());
    let keys = RefreshTokenKeys::from_config(cfg)?;
    let client_id = match authenticated_client_id(db, form.client_id.as_deref(), &client_auth).await
    {
        Ok(client_id) => client_id,
        Err(err) if err.error == "invalid_client" => return Err(err),
        Err(_) => return Ok(()),
    };
    let client = load_client(db, &client_id).await?;
    authenticate_for_refresh(issuer, &client, &client_auth).await?;
    if let Ok(claims) = issuer.verify_access_token(raw_token) {
        if claims.client_id == client.client_id {
            kill_families_for_subject(refresh_pool, &claims.client_id, &claims.sub).await?;
        }
        return Ok(());
    }
    let Some((_hash, row)) = lookup_by_any_hash(db, &keys, raw_token).await? else {
        return Ok(());
    };
    if row.client_id != client.client_id {
        return Ok(());
    }

    let pool = refresh_pool
        .checkout_pool("refresh revoke")
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "revoke: dedicated database pool checkout failed");
            OAuthError::server_error("revoke unavailable")
        })?;
    let mut conn = pool.get().await.map_err(|err| {
        tracing::error!(error = %err, "revoke: dedicated database session checkout failed");
        OAuthError::server_error("revoke unavailable")
    })?;
    let tx = conn.transaction().await.map_err(|err| {
        tracing::error!(error = %err, "revoke: BEGIN failed on dedicated session");
        OAuthError::server_error("revoke unavailable")
    })?;
    let result = async {
        lock_refresh_user_xact(&tx, row.user_id).await.map_err(|err| {
            tracing::error!(error = %err, user_id = %row.user_id, "revoke user lock failed");
            OAuthError::server_error("revoke unavailable")
        })?;
        lock_refresh_family_xact(&tx, &row.refresh_family_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, family_id = %row.refresh_family_id, "revoke family lock failed");
                OAuthError::server_error("revoke unavailable")
            })?;
        kill_family(&tx, &row.refresh_family_id, "revoke").await?;
        Ok(())
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

pub async fn revoke_user_refresh_families(
    refresh_pool: &RefreshSessionPool,
    user_id: Uuid,
    reason: &'static str,
) -> Result<(), String> {
    let pool = refresh_pool
        .checkout_pool("refresh user revoke")
        .await
        .map_err(|err| format!("refresh user revoke pool checkout ({reason}): {err}"))?;
    let mut conn = pool
        .get()
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
        tx.execute(
            "WITH fam AS ( \
                 SELECT DISTINCT client_id, sub \
                 FROM zeroship.oauth_refresh_tokens \
                 WHERE user_id = $1 AND revoked_at IS NULL \
             ), upd AS ( \
                 UPDATE zeroship.oauth_refresh_tokens \
                 SET revoked_at = NOW() \
                 WHERE user_id = $1 AND revoked_at IS NULL \
                 RETURNING 1 \
             ) \
             INSERT INTO zeroship.token_revocations (client_id, sub, revoked_after) \
             SELECT client_id, sub, NOW() FROM fam \
             ON CONFLICT (client_id, sub) \
               DO UPDATE SET revoked_after = EXCLUDED.revoked_after",
            &[&user_id],
        )
        .await
        .map(|_| ())
        .map_err(|err| format!("refresh user revoke families ({reason}): {err}"))
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
    tracing::info!(user_id = %user_id, reason, "refresh families revoked for user");
    Ok(())
}

pub async fn sweep_refresh_tokens(refresh_pool: &RefreshSessionPool) -> Result<(u64, u64), String> {
    let family_deleted = sweep_refresh_family_delete(refresh_pool).await?;
    let idem_reaped = sweep_refresh_idem(refresh_pool).await?;
    Ok((family_deleted, idem_reaped))
}

async fn replay_or_kill(
    db: &(impl GenericClient + ?Sized),
    issuer: &Issuer,
    keys: &RefreshTokenKeys,
    client: &OAuthClient,
    row: &RefreshRow,
) -> Result<TokenResponse, OAuthError> {
    if let (Some(successor_hash), Some(idem_expires_at), Some(enc)) = (
        row.replaced_by_token_hash.as_ref(),
        row.idem_expires_at,
        row.idem_response_enc.as_ref(),
    ) {
        let now = Utc::now();
        if idem_expires_at > now
            && row.expires_at > now
            && row.family_absolute_expires_at > now
        {
            if let Some(successor) = select_live_successor(db, successor_hash).await? {
                if successor.expires_at > now && successor.family_absolute_expires_at > now {
                    let cached =
                        keys.open_cached_response(&row.token_hash, &row.refresh_family_id, enc)?;
                    let scopes = parse_scopes(&cached.scope);
                    let access_token =
                        mint_access_token(issuer, client, successor.user_id, &scopes)?;
                    tracing::info!(
                        family_id = %row.refresh_family_id,
                        client_id = %row.client_id,
                        "refresh idempotency replay recovered"
                    );
                    return Ok(TokenResponse {
                        access_token,
                        id_token: None,
                        refresh_token: Some(cached.refresh_token),
                        token_type: TOKEN_TYPE_BEARER,
                        expires_in: ACCESS_TOKEN_TTL_SECS as u64,
                        scope: scopes.join(" "),
                    });
                }
            }
        }
    }

    kill_family(db, &row.refresh_family_id, "replay").await?;
    Err(OAuthError::invalid_grant("refresh token is invalid"))
}

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

pub(super) async fn authenticate_for_refresh(
    issuer: &Issuer,
    client: &OAuthClient,
    client_auth: &ClientAuth,
) -> Result<(), OAuthError> {
    // Brokered-first (MED-2): a brokered client authenticates by derive-and-
    // compare against the per-app broker secret, NOT a stored hash (it has none
    // — client_secret_hash is NULL). Without this branch the client_secret_basic
    // arm below would verify against the NULL hash and brokered refresh would be
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
                return Err(OAuthError::invalid_client("public client must not authenticate with a secret"));
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
        _ => Err(OAuthError::invalid_client("unsupported client authentication method")),
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

#[allow(clippy::future_not_send)]
pub(super) async fn introspect_refresh_token(
    db: &(impl GenericClient + ?Sized),
    keys: &RefreshTokenKeys,
    authenticated_client: &OAuthClient,
    raw_token: &str,
) -> Result<Option<ActiveRefreshToken>, OAuthError> {
    let Some((_hash, row)) = lookup_by_any_hash(db, keys, raw_token).await? else {
        return Ok(None);
    };
    if row.client_id != authenticated_client.client_id {
        return Ok(None);
    }
    let now = Utc::now();
    if row.rotated_at.is_some()
        || row.revoked_at.is_some()
        || row.expires_at <= now
        || row.family_absolute_expires_at <= now
        || family_has_revoked_row(db, &row.refresh_family_id).await?
    {
        return Ok(None);
    }
    Ok(Some(ActiveRefreshToken {
        scope: row.granted_scopes.join(" "),
        client_id: row.client_id,
        token_type: "refresh_token",
        exp: row.expires_at.timestamp(),
        iat: row.issued_at.timestamp(),
        sub: row.sub,
        aud: authenticated_client.resource_audience(),
    }))
}

fn verify_client_secret(client: &OAuthClient, client_auth: &ClientAuth) -> Result<(), OAuthError> {
    let Some(stored_hash) = client.client_secret_hash.as_deref() else {
        return Err(OAuthError::invalid_client("client secret is not configured"));
    };
    let Some(secret) = client_auth.client_secret.as_deref() else {
        return Err(OAuthError::invalid_client("client secret is required"));
    };
    if validate_api_key(secret, stored_hash) {
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

async fn lookup_by_any_hash(
    db: &(impl GenericClient + ?Sized),
    keys: &RefreshTokenKeys,
    raw_token: &str,
) -> Result<Option<(TokenHash, RefreshRow)>, OAuthError> {
    let hashes = keys.hashes_newest_first(raw_token);
    if hashes.is_empty() {
        return Ok(None);
    }
    let candidate_hashes = hashes
        .iter()
        .map(|hash| hash.hash.clone())
        .collect::<Vec<_>>();
    let rows = db
        .query(REFRESH_ROW_SELECT_ANY, &[&candidate_hashes])
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "refresh row lookup failed");
            OAuthError::server_error("refresh token store unavailable")
        })?
        .into_iter()
        .map(|row| row_to_refresh_row(&row))
        .collect::<Vec<_>>();
    for hash in hashes {
        if let Some(row) = rows.iter().find(|row| row.token_hash == hash.hash) {
            return Ok(Some((hash, row.clone())));
        }
    }
    Ok(None)
}

async fn select_refresh_row_for_update(
    db: &(impl GenericClient + ?Sized),
    token_hash: &[u8],
) -> Result<Option<RefreshRow>, OAuthError> {
    select_refresh_row(db, token_hash, true).await
}

async fn select_refresh_row(
    db: &(impl GenericClient + ?Sized),
    token_hash: &[u8],
    for_update: bool,
) -> Result<Option<RefreshRow>, OAuthError> {
    let sql = if for_update {
        REFRESH_ROW_SELECT_FOR_UPDATE
    } else {
        REFRESH_ROW_SELECT
    };
    let rows = db.query(sql, &[&token_hash]).await.map_err(|err| {
        tracing::error!(error = %err, "refresh row lookup failed");
        OAuthError::server_error("refresh token store unavailable")
    })?;
    Ok(rows.first().map(row_to_refresh_row))
}

const REFRESH_ROW_SELECT: &str = "\
    SELECT token_hash, hash_key_version, refresh_family_id, replaced_by_token_hash, \
           client_id, user_id, sub, granted_scopes, family_granted_scopes, issued_at, expires_at, \
           family_absolute_expires_at, rotated_at, revoked_at, idem_response_enc, idem_expires_at \
    FROM zeroship.oauth_refresh_tokens \
    WHERE token_hash = $1";
const REFRESH_ROW_SELECT_ANY: &str = "\
    SELECT token_hash, hash_key_version, refresh_family_id, replaced_by_token_hash, \
           client_id, user_id, sub, granted_scopes, family_granted_scopes, issued_at, expires_at, \
           family_absolute_expires_at, rotated_at, revoked_at, idem_response_enc, idem_expires_at \
    FROM zeroship.oauth_refresh_tokens \
    WHERE token_hash = ANY($1::BYTEA[])";
const REFRESH_ROW_SELECT_FOR_UPDATE: &str = "\
    SELECT token_hash, hash_key_version, refresh_family_id, replaced_by_token_hash, \
           client_id, user_id, sub, granted_scopes, family_granted_scopes, issued_at, expires_at, \
           family_absolute_expires_at, rotated_at, revoked_at, idem_response_enc, idem_expires_at \
    FROM zeroship.oauth_refresh_tokens \
    WHERE token_hash = $1 \
    FOR UPDATE";

fn row_to_refresh_row(row: &compio_postgres::Row) -> RefreshRow {
    RefreshRow {
        token_hash: row.get("token_hash"),
        hash_key_version: row.get("hash_key_version"),
        refresh_family_id: row.get("refresh_family_id"),
        replaced_by_token_hash: row.try_get("replaced_by_token_hash").ok().flatten(),
        client_id: row.get("client_id"),
        user_id: row.get("user_id"),
        sub: row.get("sub"),
        granted_scopes: row.get("granted_scopes"),
        family_granted_scopes: row.get("family_granted_scopes"),
        issued_at: row.get("issued_at"),
        expires_at: row.get("expires_at"),
        family_absolute_expires_at: row.get("family_absolute_expires_at"),
        rotated_at: row.try_get("rotated_at").ok().flatten(),
        revoked_at: row.try_get("revoked_at").ok().flatten(),
        idem_response_enc: row.try_get("idem_response_enc").ok().flatten(),
        idem_expires_at: row.try_get("idem_expires_at").ok().flatten(),
    }
}

async fn family_has_revoked_row(
    db: &(impl GenericClient + ?Sized),
    family_id: &str,
) -> Result<bool, OAuthError> {
    let rows = db
        .query(
            "SELECT 1 FROM zeroship.oauth_refresh_tokens \
             WHERE refresh_family_id = $1 AND revoked_at IS NOT NULL LIMIT 1",
            &[&family_id],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, family_id, "refresh family revoke check failed");
            OAuthError::server_error("refresh token store unavailable")
        })?;
    Ok(!rows.is_empty())
}

async fn select_live_successor(
    db: &(impl GenericClient + ?Sized),
    successor_hash: &[u8],
) -> Result<Option<RefreshRow>, OAuthError> {
    let rows = db
        .query(
            "SELECT token_hash, hash_key_version, refresh_family_id, replaced_by_token_hash, \
                    client_id, user_id, sub, granted_scopes, family_granted_scopes, issued_at, expires_at, \
                    family_absolute_expires_at, rotated_at, revoked_at, idem_response_enc, idem_expires_at \
             FROM zeroship.oauth_refresh_tokens \
             WHERE token_hash = $1 AND rotated_at IS NULL AND revoked_at IS NULL",
            &[&successor_hash],
        )
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "refresh successor lookup failed");
            OAuthError::server_error("refresh token store unavailable")
        })?;
    Ok(rows.first().map(row_to_refresh_row))
}

async fn kill_family(
    db: &(impl GenericClient + ?Sized),
    family_id: &str,
    reason: &'static str,
) -> Result<(), OAuthError> {
    db.execute(
        "WITH fam AS ( \
             SELECT DISTINCT client_id, sub \
             FROM zeroship.oauth_refresh_tokens \
             WHERE refresh_family_id = $1 \
         ), upd AS ( \
             UPDATE zeroship.oauth_refresh_tokens \
             SET revoked_at = NOW() \
             WHERE refresh_family_id = $1 AND revoked_at IS NULL \
             RETURNING 1 \
         ) \
         INSERT INTO zeroship.token_revocations (client_id, sub, revoked_after) \
         SELECT client_id, sub, NOW() FROM fam \
         ON CONFLICT (client_id, sub) \
           DO UPDATE SET revoked_after = EXCLUDED.revoked_after",
        &[&family_id],
    )
    .await
    .map_err(|err| {
        tracing::error!(error = %err, family_id, reason, "refresh family kill failed");
        OAuthError::server_error("refresh family revoke unavailable")
    })?;
    tracing::info!(family_id, reason, "refresh family killed");
    Ok(())
}

async fn kill_families_for_subject(
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
    let mut conn = pool.get().await.map_err(|err| {
        tracing::error!(error = %err, "access-token revoke: dedicated database session checkout failed");
        OAuthError::server_error("revoke unavailable")
    })?;
    let tx = conn.transaction().await.map_err(|err| {
        tracing::error!(error = %err, "access-token revoke: BEGIN failed on dedicated session");
        OAuthError::server_error("revoke unavailable")
    })?;
    let result = async {
        let rows = tx
            .query(
                "SELECT DISTINCT user_id, refresh_family_id \
                 FROM zeroship.oauth_refresh_tokens \
                 WHERE client_id = $1 AND sub = $2 \
                 ORDER BY user_id, refresh_family_id",
                &[&client_id, &sub],
            )
            .await
            .map_err(|err| {
                tracing::error!(
                    error = %err,
                    client_id,
                    sub,
                    "access-token revoke: refresh family lookup failed"
                );
                OAuthError::server_error("revoke unavailable")
            })?;

        let mut user_ids = Vec::new();
        let mut family_ids = Vec::new();
        for row in rows {
            user_ids.push(row.get::<_, Uuid>("user_id"));
            family_ids.push(row.get::<_, String>("refresh_family_id"));
        }
        user_ids.sort();
        user_ids.dedup();
        family_ids.sort();
        family_ids.dedup();

        for user_id in user_ids {
            lock_refresh_user_xact(&tx, user_id).await.map_err(|err| {
                tracing::error!(
                    error = %err,
                    user_id = %user_id,
                    "access-token revoke: refresh user lock failed"
                );
                OAuthError::server_error("revoke unavailable")
            })?;
        }
        for family_id in family_ids {
            lock_refresh_family_xact(&tx, &family_id)
                .await
                .map_err(|err| {
                    tracing::error!(
                        error = %err,
                        family_id = %family_id,
                        "access-token revoke: refresh family lock failed"
                    );
                    OAuthError::server_error("revoke unavailable")
                })?;
        }

        kill_families_for_subject_inner(&tx, client_id, sub).await
    }
    .await;

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

async fn kill_families_for_subject_inner(
    db: &(impl GenericClient + ?Sized),
    client_id: &str,
    sub: &str,
) -> Result<(), OAuthError> {
    db.execute(
        "WITH upd AS ( \
             UPDATE zeroship.oauth_refresh_tokens \
             SET revoked_at = NOW() \
             WHERE client_id = $1 AND sub = $2 AND revoked_at IS NULL \
             RETURNING 1 \
         ) \
         INSERT INTO zeroship.token_revocations (client_id, sub, revoked_after) \
         VALUES ($1, $2, NOW()) \
         ON CONFLICT (client_id, sub) \
           DO UPDATE SET revoked_after = EXCLUDED.revoked_after",
        &[&client_id, &sub],
    )
    .await
    .map_err(|err| {
        tracing::error!(
            error = %err,
            client_id,
            sub,
            "access-token revoke: refresh families kill failed"
        );
        OAuthError::server_error("revoke unavailable")
    })?;
    tracing::info!(client_id, sub, "access-token refresh families killed");
    Ok(())
}

async fn sweep_refresh_family_delete(refresh_pool: &RefreshSessionPool) -> Result<u64, String> {
    let enum_pool = refresh_pool
        .checkout_pool("refresh sweep family-delete enumerate")
        .await
        .map_err(|err| format!("refresh sweep enumerate family-delete pool checkout: {err}"))?;
    let enum_conn = enum_pool
        .get()
        .await
        .map_err(|err| format!("refresh sweep enumerate family-delete session checkout: {err}"))?;
    let users = enum_conn
        .query(
            "SELECT DISTINCT user_id \
             FROM zeroship.oauth_refresh_tokens \
             WHERE family_absolute_expires_at < NOW() \
                OR (rotated_at IS NOT NULL AND issued_at < NOW() - INTERVAL '24 hours')",
            &[],
        )
        .await
        .map_err(|err| format!("refresh sweep enumerate family-delete users: {err}"))?
        .into_iter()
        .map(|row| row.get::<_, Uuid>("user_id"))
        .collect::<Vec<_>>();
    drop(enum_conn);
    drop(enum_pool);
    let mut deleted = 0;
    for user_id in users {
        let pool = refresh_pool
            .checkout_pool("refresh sweep family-delete")
            .await
            .map_err(|err| format!("refresh sweep family-delete pool checkout: {err}"))?;
        let mut conn = pool
            .get()
            .await
            .map_err(|err| format!("refresh sweep family-delete session checkout: {err}"))?;
        let tx = conn
            .transaction()
            .await
            .map_err(|err| format!("refresh sweep family-delete begin: {err}"))?;
        let result = async {
            lock_refresh_user_xact(&tx, user_id)
                .await
                .map_err(|err| format!("refresh sweep family-delete lock {user_id}: {err}"))?;
            tx.execute(
                "DELETE FROM zeroship.oauth_refresh_tokens \
                 WHERE user_id = $1 \
                   AND (family_absolute_expires_at < NOW() \
                        OR (rotated_at IS NOT NULL AND issued_at < NOW() - INTERVAL '24 hours'))",
                &[&user_id],
            )
            .await
            .map_err(|err| format!("refresh sweep family-delete {user_id}: {err}"))
        }
        .await;
        match result {
            Ok(n) => {
                tx.commit()
                    .await
                    .map_err(|err| format!("refresh sweep family-delete commit: {err}"))?;
                deleted += n;
            }
            Err(err) => {
                let _ = tx.rollback().await;
                return Err(err);
            }
        }
    }
    Ok(deleted)
}

async fn sweep_refresh_idem(refresh_pool: &RefreshSessionPool) -> Result<u64, String> {
    let enum_pool = refresh_pool
        .checkout_pool("refresh sweep idem enumerate")
        .await
        .map_err(|err| format!("refresh sweep enumerate idem pool checkout: {err}"))?;
    let enum_conn = enum_pool
        .get()
        .await
        .map_err(|err| format!("refresh sweep enumerate idem session checkout: {err}"))?;
    let users = enum_conn
        .query(
            "SELECT DISTINCT user_id \
             FROM zeroship.oauth_refresh_tokens \
             WHERE idem_response_enc IS NOT NULL AND idem_expires_at < NOW()",
            &[],
        )
        .await
        .map_err(|err| format!("refresh sweep enumerate idem users: {err}"))?
        .into_iter()
        .map(|row| row.get::<_, Uuid>("user_id"))
        .collect::<Vec<_>>();
    drop(enum_conn);
    drop(enum_pool);
    let mut reaped = 0;
    for user_id in users {
        let pool = refresh_pool
            .checkout_pool("refresh sweep idem")
            .await
            .map_err(|err| format!("refresh sweep idem pool checkout: {err}"))?;
        let mut conn = pool
            .get()
            .await
            .map_err(|err| format!("refresh sweep idem session checkout: {err}"))?;
        let tx = conn
            .transaction()
            .await
            .map_err(|err| format!("refresh sweep idem begin: {err}"))?;
        let result = async {
            lock_refresh_user_xact(&tx, user_id)
                .await
                .map_err(|err| format!("refresh sweep idem lock {user_id}: {err}"))?;
            tx.execute(
                "UPDATE zeroship.oauth_refresh_tokens \
                 SET idem_response_enc = NULL, idem_expires_at = NULL \
                 WHERE user_id = $1 \
                   AND idem_response_enc IS NOT NULL \
                   AND idem_expires_at < NOW()",
                &[&user_id],
            )
            .await
            .map_err(|err| format!("refresh sweep idem {user_id}: {err}"))
        }
        .await;
        match result {
            Ok(n) => {
                tx.commit()
                    .await
                    .map_err(|err| format!("refresh sweep idem commit: {err}"))?;
                reaped += n;
            }
            Err(err) => {
                let _ = tx.rollback().await;
                return Err(err);
            }
        }
    }
    Ok(reaped)
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

fn generate_refresh_token() -> String {
    let mut bytes = [0u8; REFRESH_TOKEN_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    format!("{REFRESH_TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
}

fn idem_aad(predecessor_hash: &[u8], family_id: &str) -> Vec<u8> {
    let mut aad =
        Vec::with_capacity(IDEM_AAD_PREFIX.len() + predecessor_hash.len() + family_id.len());
    aad.extend_from_slice(IDEM_AAD_PREFIX);
    aad.extend_from_slice(predecessor_hash);
    aad.extend_from_slice(family_id.as_bytes());
    aad
}

fn load_hash_keyring(path: &Path) -> Result<Vec<RefreshHashKey>, String> {
    let raw = read_secret_file(path, "REFRESH_HASH_KEY_FILE")?;
    let text = std::str::from_utf8(&raw).map_err(|err| {
        format!(
            "REFRESH_HASH_KEY_FILE {} must be UTF-8 version:key lines: {err}",
            path.display()
        )
    })?;
    let mut keys = Vec::new();
    for (idx, line) in text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        let Some((version, secret)) = line.split_once(':') else {
            return Err(format!(
                "REFRESH_HASH_KEY_FILE {} line {} must be version:hex-or-base64url-key",
                path.display(),
                idx + 1
            ));
        };
        let version: i16 = version.trim().parse().map_err(|_| {
            format!(
                "REFRESH_HASH_KEY_FILE {} line {} has invalid version {:?}",
                path.display(),
                idx + 1,
                version
            )
        })?;
        let key = decode_key_material(secret.trim()).ok_or_else(|| {
            format!(
                "REFRESH_HASH_KEY_FILE {} line {} has unparseable key material",
                path.display(),
                idx + 1
            )
        })?;
        if key.len() < 32 {
            return Err(format!(
                "REFRESH_HASH_KEY_FILE {} line {} key for version {} is {} bytes; require at least 32",
                path.display(),
                idx + 1,
                version,
                key.len()
            ));
        }
        keys.push(RefreshHashKey { version, key });
    }
    if keys.is_empty() {
        return Err(format!(
            "REFRESH_HASH_KEY_FILE {} yielded no keys",
            path.display()
        ));
    }
    Ok(keys)
}

fn decode_key_material(value: &str) -> Option<Vec<u8>> {
    hex::decode(value)
        .ok()
        .or_else(|| URL_SAFE_NO_PAD.decode(value).ok())
        .filter(|bytes| !bytes.is_empty())
}

fn read_secret_file(path: &Path, label: &str) -> Result<Vec<u8>, String> {
    let bytes = std::fs::read(path).map_err(|err| format!("read {label} {}: {err}", path.display()))?;
    reject_insecure_permissions(path, label)?;
    if bytes.is_empty() {
        return Err(format!("{label} {} is empty", path.display()));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn reject_insecure_permissions(path: &Path, label: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;

    let mode = path
        .metadata()
        .map_err(|err| format!("stat {label} {}: {err}", path.display()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(format!(
            "{label} {} has insecure permissions {mode:o}; require owner-only permissions",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn reject_insecure_permissions(_path: &Path, _label: &str) -> Result<(), String> {
    Ok(())
}

#[must_use]
pub fn client_secret_hash(secret: &str) -> String {
    hash_api_key(secret)
}
