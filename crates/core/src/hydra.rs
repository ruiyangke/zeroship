//! Hydra OAuth2 token introspection client.
//!
//! Control-plane authorization uses this client to verify third-party
//! OAuth access tokens against hydra's admin introspection endpoint.

use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

const DEFAULT_TTL: Duration = Duration::from_secs(300);
const CACHE_CAPACITY: usize = 4096;
const INTROSPECT_PATH: &str = "/admin/oauth2/introspect";

type TokenHash = [u8; 32];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntrospectResult {
    pub active: bool,
    pub sub: Option<String>,
    /// Space-separated scope string, exactly as returned by hydra.
    pub scope: Option<String>,
    pub aud: Option<Vec<String>>,
    pub client_id: Option<String>,
    /// Absolute token expiry in UNIX seconds.
    pub exp: Option<u64>,
}

/// Token introspection client. Uses compio HTTP via `cyper`.
pub struct HydraIntrospector {
    admin_url: String,
    http_client: cyper::Client,
    cache: Mutex<lru::LruCache<TokenHash, CachedResult>>,
    ttl: Duration,
}

impl std::fmt::Debug for HydraIntrospector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cache_len = self.cache.lock().map_or(0, |cache| cache.len());
        f.debug_struct("HydraIntrospector")
            .field("admin_url", &self.admin_url)
            .field("ttl", &self.ttl)
            .field("cache_len", &cache_len)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
struct CachedResult {
    inserted_at: Instant,
    result: IntrospectResult,
}

#[derive(Debug, Error)]
pub enum IntrospectError {
    #[error("bearer token is empty")]
    EmptyToken,

    #[error("introspection request build: {0}")]
    Build(String),

    #[error("introspection request header: {0}")]
    Header(String),

    #[error("introspection request send: {0}")]
    Send(String),

    #[error("introspection response read: {0}")]
    Read(String),

    #[error("introspection returned HTTP {status}: {body}")]
    HttpStatus { status: u16, body: String },

    #[error("introspection response parse: {0}")]
    Parse(String),

    #[error("introspection cache mutex poisoned")]
    CachePoisoned,
}

impl HydraIntrospector {
    #[must_use]
    pub fn new(admin_url: impl Into<String>) -> Self {
        let capacity = NonZeroUsize::new(CACHE_CAPACITY).expect("cache capacity is non-zero");
        Self {
            admin_url: admin_url.into(),
            http_client: cyper::Client::new(),
            cache: Mutex::new(lru::LruCache::new(capacity)),
            ttl: DEFAULT_TTL,
        }
    }

    #[must_use]
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Remove every cached introspection result whose subject matches
    /// `sub`. Used after OIDC back-channel logout so an otherwise
    /// unexpired active-token cache entry cannot outlive revocation.
    pub fn invalidate_by_sub(&self, sub: &str) {
        if sub.is_empty() {
            return;
        }

        let Ok(mut cache) = self.cache.lock() else {
            tracing::warn!("hydra introspection cache mutex poisoned during sub invalidation");
            return;
        };
        let hashes: Vec<TokenHash> = cache
            .iter()
            .filter_map(|(hash, cached)| {
                (cached.result.sub.as_deref() == Some(sub)).then_some(*hash)
            })
            .collect();
        for hash in hashes {
            cache.pop(&hash);
        }
    }

    /// Token must not be empty.
    ///
    /// Returns `Ok(IntrospectResult { active: false, .. })` when hydra
    /// returns HTTP 200 with `active=false`. Returns `Err` only for
    /// caller misuse, transport, HTTP status, or response-shape failures.
    #[allow(clippy::future_not_send)]
    pub async fn introspect(&self, token: &str) -> Result<IntrospectResult, IntrospectError> {
        if token.is_empty() {
            return Err(IntrospectError::EmptyToken);
        }

        let hash = token_hash(token);
        if let Some(cached) = self.cached_result(&hash)? {
            return Ok(cached);
        }

        let result = self.fetch_introspection(token).await?;
        self.store_result(hash, result.clone())?;
        Ok(result)
    }

    fn cached_result(&self, hash: &TokenHash) -> Result<Option<IntrospectResult>, IntrospectError> {
        let now_secs = unix_now_secs();
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| IntrospectError::CachePoisoned)?;

        if let Some(cached) = cache.get(hash) {
            if !cache_entry_expired(cached, self.ttl, now_secs) {
                return Ok(Some(cached.result.clone()));
            }
        }

        cache.pop(hash);
        Ok(None)
    }

    fn store_result(
        &self,
        hash: TokenHash,
        result: IntrospectResult,
    ) -> Result<(), IntrospectError> {
        if self.ttl.is_zero() {
            return Ok(());
        }

        let now_secs = unix_now_secs();
        if result.exp.is_some_and(|exp| exp <= now_secs) {
            return Ok(());
        }

        let mut cache = self
            .cache
            .lock()
            .map_err(|_| IntrospectError::CachePoisoned)?;
        cache.put(
            hash,
            CachedResult {
                inserted_at: Instant::now(),
                result,
            },
        );
        Ok(())
    }

    #[allow(clippy::future_not_send)]
    async fn fetch_introspection(&self, token: &str) -> Result<IntrospectResult, IntrospectError> {
        let url = self.introspect_url();
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("token", token)
            .finish();

        let resp = self
            .http_client
            .request(http::Method::POST, &url)
            .map_err(|e| IntrospectError::Build(e.to_string()))?
            .header("content-type", "application/x-www-form-urlencoded")
            .map_err(|e| IntrospectError::Header(e.to_string()))?
            .body(body.into_bytes())
            .send()
            .await
            .map_err(|e| IntrospectError::Send(e.to_string()))?;

        let status = resp.status().as_u16();
        let body = resp
            .text()
            .await
            .map_err(|e| IntrospectError::Read(e.to_string()))?;

        if !(200..300).contains(&status) {
            return Err(IntrospectError::HttpStatus { status, body });
        }

        serde_json::from_str::<HydraIntrospectionResponse>(&body)
            .map(IntrospectResult::from)
            .map_err(|e| IntrospectError::Parse(format!("{e}\nbody: {body}")))
    }

    fn introspect_url(&self) -> String {
        format!("{}{}", self.admin_url.trim_end_matches('/'), INTROSPECT_PATH)
    }
}

#[derive(Debug, Deserialize)]
struct HydraIntrospectionResponse {
    active: bool,
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_aud")]
    aud: Option<Vec<String>>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    exp: Option<u64>,
}

impl From<HydraIntrospectionResponse> for IntrospectResult {
    fn from(value: HydraIntrospectionResponse) -> Self {
        Self {
            active: value.active,
            sub: value.sub,
            scope: value.scope,
            aud: value.aud,
            client_id: value.client_id,
            exp: value.exp,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum AudValue {
    One(String),
    Many(Vec<String>),
}

fn deserialize_optional_aud<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<AudValue>::deserialize(deserializer)?;
    Ok(value.map(|aud| match aud {
        AudValue::One(aud) => vec![aud],
        AudValue::Many(aud) => aud,
    }))
}

fn token_hash(token: &str) -> TokenHash {
    Sha256::digest(token.as_bytes()).into()
}

fn cache_entry_expired(cached: &CachedResult, ttl: Duration, now_secs: u64) -> bool {
    cached.inserted_at.elapsed() >= ttl || cached.result.exp.is_some_and(|exp| exp <= now_secs)
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
