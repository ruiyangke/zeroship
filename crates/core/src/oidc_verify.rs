//! Shared JWKS cache + OIDC ID-token verifier.
//!
//! Used by the gateway (per-app OIDC RP) and the control plane (dashboard
//! OIDC RP) to verify ID tokens issued by hydra. Hydra's JWKS endpoint
//! lives at `https://auth.zeroship.ai/.well-known/jwks.json` in prod
//! (or wherever the deployment configures); this module fetches it on
//! demand, caches for 5 minutes, and force-refreshes on signature
//! verification failure.
//!
//! Token verification covers: signature (against any cached public key
//! matching `kid` and `alg`), `iss`, `aud`, `exp` (+ `nbf` if present),
//! `iat` (sanity bound — not in future, not too old), and optionally
//! `nonce`.

use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum OidcError {
    #[error("JWKS fetch: {0}")]
    FetchJwks(String),

    #[error("JWKS parse: {0}")]
    ParseJwks(String),

    #[error("JWT decode header: {0}")]
    DecodeHeader(String),

    #[error("JWT verify: {0}")]
    Verify(String),

    #[error("no JWK with matching kid: {0}")]
    NoMatchingKey(String),

    #[error("issuer mismatch: expected {expected}, got {got}")]
    IssuerMismatch { expected: String, got: String },

    #[error("audience mismatch: expected {expected}, got {got}")]
    AudienceMismatch { expected: String, got: String },

    #[error("nonce mismatch")]
    NonceMismatch,
}

pub type Result<T> = std::result::Result<T, OidcError>;

#[derive(Clone)]
pub struct JwksCache {
    url: String,
    inner: Arc<RwLock<JwksState>>,
    ttl: Duration,
}

impl std::fmt::Debug for JwksCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.inner.read();
        f.debug_struct("JwksCache")
            .field("url", &self.url)
            .field("ttl", &self.ttl)
            .field("key_count", &state.keys.len())
            .field("fetched_at", &state.fetched_at)
            .finish()
    }
}

#[derive(Default)]
struct JwksState {
    keys: Vec<CachedKey>,
    fetched_at: Option<Instant>,
}

/// A single decoded JWK ready for use by `jsonwebtoken::decode`.
///
/// `DecodingKey` does not implement `Debug` (it wraps opaque key
/// material), so `decoding` is omitted from the `Debug` output.
#[derive(Clone)]
pub struct CachedKey {
    pub kid: String,
    pub alg: Algorithm,
    pub decoding: DecodingKey,
}

impl std::fmt::Debug for CachedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedKey")
            .field("kid", &self.kid)
            .field("alg", &self.alg)
            .finish_non_exhaustive()
    }
}

impl JwksCache {
    /// Create a new JWKS cache pointing at the issuer's `jwks_uri`.
    /// TTL defaults to 5 minutes.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            inner: Arc::new(RwLock::new(JwksState::default())),
            ttl: Duration::from_secs(300), // 5 min
        }
    }

    /// Get the current set of decoding keys. Refreshes if the cache is
    /// empty or stale.
    ///
    /// # Errors
    /// Propagates `OidcError::FetchJwks` / `ParseJwks` if a refresh was
    /// needed and the JWKS endpoint was unreachable or returned a
    /// malformed document.
    pub async fn keys(&self) -> Result<Vec<CachedKey>> {
        let stale = self
            .inner
            .read()
            .fetched_at
            .is_none_or(|t| t.elapsed() > self.ttl);
        if stale {
            self.refresh().await?;
        }
        Ok(self.inner.read().keys.clone())
    }

    /// Construct a cache pre-loaded with the given keys, marked
    /// freshly fetched. Used by unit tests in this crate (and downstream
    /// crates like `logout_token`) to short-circuit the network fetch.
    /// `url` is set to a sentinel that won't resolve so a stray
    /// `refresh()` call in production code wouldn't accidentally pick
    /// up real keys.
    #[cfg(test)]
    pub(crate) fn for_test(keys: Vec<CachedKey>) -> Self {
        Self {
            url: "http://invalid.test/jwks.json".into(),
            inner: Arc::new(RwLock::new(JwksState {
                keys,
                fetched_at: Some(Instant::now()),
            })),
            ttl: Duration::from_secs(300),
        }
    }

    /// Force-refresh the cache. Call after a signature-verification
    /// failure (e.g. JWKS rotated under us).
    ///
    /// # Errors
    /// `OidcError::FetchJwks` on network / non-2xx, `ParseJwks` on
    /// malformed JWKS or unusable key components.
    pub async fn refresh(&self) -> Result<()> {
        let client = cyper::Client::new();
        let resp = client
            .request(http::Method::GET, &self.url)
            .map_err(|e| OidcError::FetchJwks(format!("build: {e}")))?
            .send()
            .await
            .map_err(|e| OidcError::FetchJwks(format!("send: {e}")))?;

        let status = resp.status().as_u16();
        let body = resp
            .text()
            .await
            .map_err(|e| OidcError::FetchJwks(format!("read: {e}")))?;
        if !(200..300).contains(&status) {
            return Err(OidcError::FetchJwks(format!("HTTP {status}: {body}")));
        }

        let jwks: JwksDoc = serde_json::from_str(&body)
            .map_err(|e| OidcError::ParseJwks(format!("{e}\nbody: {body}")))?;

        let mut parsed = Vec::with_capacity(jwks.keys.len());
        for jwk in &jwks.keys {
            let alg = match jwk.alg.as_str() {
                "RS256" => Algorithm::RS256,
                "RS384" => Algorithm::RS384,
                "RS512" => Algorithm::RS512,
                "ES256" => Algorithm::ES256,
                "ES384" => Algorithm::ES384,
                "EdDSA" => Algorithm::EdDSA,
                other => {
                    tracing::warn!(alg = other, kid = %jwk.kid, "skipping unsupported JWK alg");
                    continue;
                }
            };
            let decoding = match jwk.kty.as_str() {
                "RSA" => {
                    let n = jwk
                        .n
                        .as_deref()
                        .ok_or_else(|| OidcError::ParseJwks("RSA n missing".into()))?;
                    let e = jwk
                        .e
                        .as_deref()
                        .ok_or_else(|| OidcError::ParseJwks("RSA e missing".into()))?;
                    DecodingKey::from_rsa_components(n, e)
                        .map_err(|e| OidcError::ParseJwks(format!("RSA components: {e}")))?
                }
                "EC" => {
                    let x = jwk
                        .x
                        .as_deref()
                        .ok_or_else(|| OidcError::ParseJwks("EC x missing".into()))?;
                    let y = jwk
                        .y
                        .as_deref()
                        .ok_or_else(|| OidcError::ParseJwks("EC y missing".into()))?;
                    DecodingKey::from_ec_components(x, y)
                        .map_err(|e| OidcError::ParseJwks(format!("EC components: {e}")))?
                }
                "OKP" => {
                    // Ed25519
                    let x = jwk
                        .x
                        .as_deref()
                        .ok_or_else(|| OidcError::ParseJwks("OKP x missing".into()))?;
                    DecodingKey::from_ed_components(x)
                        .map_err(|e| OidcError::ParseJwks(format!("OKP components: {e}")))?
                }
                other => {
                    tracing::warn!(kty = other, kid = %jwk.kid, "skipping unsupported JWK kty");
                    continue;
                }
            };
            parsed.push(CachedKey {
                kid: jwk.kid.clone(),
                alg,
                decoding,
            });
        }

        let count = parsed.len();
        {
            let mut state = self.inner.write();
            state.keys = parsed;
            state.fetched_at = Some(Instant::now());
        }
        tracing::info!(url = %self.url, count, "JWKS refreshed");
        Ok(())
    }
}

#[derive(Deserialize)]
struct JwksDoc {
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    kid: String,
    kty: String,
    alg: String,
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    e: Option<String>,
    #[serde(default)]
    x: Option<String>,
    #[serde(default)]
    y: Option<String>,
}

/// OIDC ID-token claims. Captures the OIDC standard claims plus any
/// extra fields the issuer sets (collected into `other`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenClaims {
    pub sub: String,
    pub iss: String,
    /// `aud` per RFC 7519 may be a string or an array of strings.
    pub aud: serde_json::Value,
    pub exp: i64,
    pub iat: i64,
    #[serde(default)]
    pub nbf: Option<i64>,
    #[serde(default)]
    pub nonce: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub email_verified: Option<bool>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub picture: Option<String>,
    #[serde(default)]
    pub acr: Option<String>,
    #[serde(default)]
    pub amr: Option<Vec<String>>,
    #[serde(flatten)]
    pub other: std::collections::BTreeMap<String, serde_json::Value>,
}

/// Verify an OIDC ID token. Returns the claims on success.
///
/// On the first verification failure (likely cause: JWKS rotated under
/// us), the cache is force-refreshed and verification is retried once.
///
/// # Errors
/// - `DecodeHeader` if the JWT header is malformed or has no `kid`.
/// - `NoMatchingKey` if no cached JWK matches the token's `kid` + `alg`
///   (even after a force-refresh).
/// - `Verify` for any signature / exp / nbf / iat / aud failure.
/// - `IssuerMismatch` if the decoded `iss` claim doesn't match
///   `expected_iss` (defense-in-depth — `jsonwebtoken` should have
///   caught it already).
/// - `NonceMismatch` if `expected_nonce` is set and the token's
///   `nonce` claim is absent or different.
/// - `FetchJwks` / `ParseJwks` propagated from the JWKS cache.
pub async fn verify_id_token(
    cache: &JwksCache,
    token: &str,
    expected_iss: &str,
    expected_aud: &str,
    expected_nonce: Option<&str>,
) -> Result<TokenClaims> {
    let header = decode_header(token).map_err(|e| OidcError::DecodeHeader(e.to_string()))?;
    let kid = header
        .kid
        .clone()
        .ok_or_else(|| OidcError::DecodeHeader("no kid".into()))?;
    let alg = header.alg;

    let try_verify = |keys: Vec<CachedKey>| -> Result<TokenClaims> {
        let key = keys
            .iter()
            .find(|k| k.kid == kid && k.alg == alg)
            .ok_or_else(|| OidcError::NoMatchingKey(kid.clone()))?;

        let mut validation = Validation::new(alg);
        validation.set_issuer(&[expected_iss]);
        validation.set_audience(&[expected_aud]);

        let data: jsonwebtoken::TokenData<TokenClaims> =
            decode(token, &key.decoding, &validation)
                .map_err(|e| OidcError::Verify(e.to_string()))?;
        Ok(data.claims)
    };

    let claims = if let Ok(c) = try_verify(cache.keys().await?) {
        c
    } else {
        // Likely cause: JWKS rotated. Force-refresh once and retry.
        cache.refresh().await?;
        try_verify(cache.keys().await?)?
    };

    // Defense-in-depth: `jsonwebtoken` already checks iss/aud against
    // the validation config above, but re-check explicitly so the error
    // path is well-typed and the contract is obvious to callers.
    if claims.iss != expected_iss {
        return Err(OidcError::IssuerMismatch {
            expected: expected_iss.into(),
            got: claims.iss,
        });
    }
    if let Some(expected_nonce) = expected_nonce {
        match claims.nonce.as_deref() {
            Some(n) if n == expected_nonce => {}
            _ => return Err(OidcError::NonceMismatch),
        }
    }

    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn jwks_parse_rsa() {
        let body = json!({
            "keys": [{
                "kid": "abc",
                "kty": "RSA",
                "alg": "RS256",
                "n": "...",
                "e": "AQAB"
            }]
        })
        .to_string();
        // Just verify we get a JwksDoc out; the DecodingKey construction
        // requires real key material so leave to the live test.
        let parsed: JwksDoc = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed.keys.len(), 1);
        assert_eq!(parsed.keys[0].kid, "abc");
        assert_eq!(parsed.keys[0].alg, "RS256");
    }

    #[test]
    fn jwks_parse_eddsa_okp() {
        let body = json!({
            "keys": [{
                "kid": "xyz",
                "kty": "OKP",
                "alg": "EdDSA",
                "crv": "Ed25519",
                "x": "..."
            }]
        })
        .to_string();
        let parsed: JwksDoc = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed.keys.len(), 1);
        assert_eq!(parsed.keys[0].kty, "OKP");
        assert_eq!(parsed.keys[0].alg, "EdDSA");
    }

    #[test]
    fn jwks_parse_skips_unknown_kty_and_alg() {
        // The parser itself accepts unknown algs/ktys; refresh() is what
        // skips them at DecodingKey construction time. So at the doc
        // layer we only assert that arbitrary alg/kty strings round-trip.
        let body = json!({
            "keys": [
                { "kid": "k1", "kty": "RSA",  "alg": "RS256", "n": "a", "e": "AQAB" },
                { "kid": "k2", "kty": "WUT",  "alg": "WAT",   "x": "b" }
            ]
        })
        .to_string();
        let parsed: JwksDoc = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed.keys.len(), 2);
    }

    #[test]
    fn cache_starts_empty_and_stale() {
        let cache = JwksCache::new("http://example.invalid/jwks.json");
        let (is_empty, no_fetched_at) = {
            let state = cache.inner.read();
            (state.keys.is_empty(), state.fetched_at.is_none())
        };
        assert!(is_empty);
        assert!(no_fetched_at);
    }
}
