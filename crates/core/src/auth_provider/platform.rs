use std::collections::HashSet;
use std::fmt;

use jsonwebtoken::{decode, decode_header, Algorithm, Validation};
use serde::Deserialize;
use thiserror::Error;

use crate::auth_provider::{ProviderAuthz, VerifiedToken};
use crate::oidc_verify::{CachedKey, JwksCache, OidcError};

const PLATFORM_ACCESS_TOKEN_TYP: &str = "at+jwt";

fn resource_server_scope_claim(raw_scope: &str) -> String {
    raw_scope
        .split_whitespace()
        .filter(|scope| !is_standard_oidc_scope(scope))
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_standard_oidc_scope(scope: &str) -> bool {
    matches!(
        scope,
        "openid" | "offline_access" | "profile" | "email" | "address" | "phone"
    )
}

/// Platform OP access-token verifier configuration.
#[derive(Debug, Clone)]
pub struct PlatformConfig {
    pub issuer: String,
    pub jwks_url: String,
}

impl PlatformConfig {
    /// Build a platform verifier config.
    ///
    /// `jwks_url` defaults to `{issuer}/.well-known/jwks.json` when omitted
    /// or blank. The issuer is normalized the same way the platform issuer
    /// mints tokens: no trailing slash.
    pub fn new(
        issuer: impl Into<String>,
        jwks_url: Option<String>,
    ) -> Result<Self, PlatformConfigError> {
        let issuer = issuer.into().trim_end_matches('/').to_string();
        if issuer.trim().is_empty() {
            return Err(PlatformConfigError::EmptyIssuer);
        }

        let jwks_url = jwks_url
            .map(|url| url.trim().to_string())
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| format!("{issuer}/.well-known/jwks.json"));
        if jwks_url.is_empty() {
            return Err(PlatformConfigError::EmptyJwksUrl);
        }

        Ok(Self { issuer, jwks_url })
    }
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum PlatformConfigError {
    #[error("AUTH_PLATFORM_ISSUER is required")]
    EmptyIssuer,

    #[error("AUTH_PLATFORM_JWKS_URL is required when set")]
    EmptyJwksUrl,
}

#[derive(Debug, Clone)]
pub struct PlatformProvider {
    config: PlatformConfig,
    jwks: JwksCache,
}

impl PlatformProvider {
    #[must_use]
    pub fn new(config: PlatformConfig) -> Self {
        let jwks = JwksCache::new(config.jwks_url.clone());
        Self { config, jwks }
    }

    #[cfg(test)]
    fn with_jwks_cache_for_test(config: PlatformConfig, jwks: JwksCache) -> Self {
        Self { config, jwks }
    }

    #[allow(clippy::future_not_send)]
    pub async fn verify_token(&self, token: &str) -> Result<VerifiedToken, PlatformVerifyError> {
        let claims = verify_access_token(token, &self.jwks, &self.config.issuer).await?;
        map_claims(claims)
    }

    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.config.issuer
    }

    #[must_use]
    pub fn jwks_url(&self) -> &str {
        &self.config.jwks_url
    }
}

#[derive(Debug, Error)]
pub enum PlatformVerifyError {
    #[error("decode JWT header: {0}")]
    DecodeHeader(String),

    #[error("JWT verify: {0}")]
    Jwt(String),

    #[error("JWKS: {0}")]
    Jwks(#[from] OidcError),

    #[error("no JWK with matching kid: {0}")]
    NoMatchingKey(String),

    #[error("platform access token requires a kid header")]
    MissingKid,

    #[error("unsupported platform JWT algorithm: {0:?}")]
    UnsupportedAlgorithm(Algorithm),

    #[error("wrong JWT type: expected at+jwt, got {got:?}")]
    WrongType { got: Option<String> },

    #[error("issuer mismatch: expected {expected}, got {got}")]
    IssuerMismatch { expected: String, got: String },

    #[error("platform token missing subject")]
    MissingSubject,
}

#[derive(Debug, Deserialize)]
struct PlatformAccessClaims {
    sub: String,
    iss: String,
    aud: ClaimStrings,
    exp: u64,
    iat: u64,
    jti: String,
    client_id: String,
    scope: String,
    #[serde(default)]
    nbf: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClaimStrings(Vec<String>);

impl<'de> Deserialize<'de> for ClaimStrings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = ClaimStrings;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a string or an array of strings")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(ClaimStrings(vec![value.to_string()]))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(ClaimStrings(vec![value]))
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element::<String>()? {
                    values.push(value);
                }
                Ok(ClaimStrings(values))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

#[allow(clippy::future_not_send)]
async fn verify_access_token(
    token: &str,
    jwks: &JwksCache,
    expected_issuer: &str,
) -> Result<PlatformAccessClaims, PlatformVerifyError> {
    let header =
        decode_header(token).map_err(|e| PlatformVerifyError::DecodeHeader(e.to_string()))?;
    if header.typ.as_deref() != Some(PLATFORM_ACCESS_TOKEN_TYP) {
        return Err(PlatformVerifyError::WrongType {
            got: header.typ.clone(),
        });
    }
    let kid = header.kid.clone().ok_or(PlatformVerifyError::MissingKid)?;
    let alg = header.alg;
    if alg != Algorithm::EdDSA {
        return Err(PlatformVerifyError::UnsupportedAlgorithm(alg));
    }

    let try_verify = |keys: Vec<CachedKey>| -> Result<PlatformAccessClaims, PlatformVerifyError> {
        let key = find_key(&keys, &kid)?;
        let validation = platform_validation(expected_issuer);
        let data = decode::<PlatformAccessClaims>(token, &key.decoding, &validation)
            .map_err(|e| PlatformVerifyError::Jwt(e.to_string()))?;
        verify_registered_claims(data.claims, expected_issuer)
    };

    match try_verify(jwks.keys().await?) {
        Ok(claims) => Ok(claims),
        Err(PlatformVerifyError::NoMatchingKey(_)) => {
            jwks.refresh().await?;
            try_verify(jwks.keys().await?)
        }
        Err(err) => Err(err),
    }
}

fn find_key<'a>(
    keys: &'a [CachedKey],
    kid: &str,
) -> Result<&'a CachedKey, PlatformVerifyError> {
    keys.iter()
        .find(|key| key.kid == kid && key.alg == Algorithm::EdDSA)
        .ok_or_else(|| PlatformVerifyError::NoMatchingKey(kid.to_string()))
}

fn platform_validation(expected_issuer: &str) -> Validation {
    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.algorithms = vec![Algorithm::EdDSA];
    validation.set_issuer(&[expected_issuer]);
    // Control owns the concrete resource-server audience check because it is
    // configured as OAUTH_AUDIENCE on the resource server.
    validation.validate_aud = false;
    validation.validate_nbf = true;
    validation.leeway = 0;
    validation.required_spec_claims = [
        "exp",
        "iss",
        "aud",
        "sub",
        "iat",
        "jti",
        "client_id",
        "scope",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<HashSet<_>>();
    validation
}

fn verify_registered_claims(
    claims: PlatformAccessClaims,
    expected_issuer: &str,
) -> Result<PlatformAccessClaims, PlatformVerifyError> {
    if claims.iss != expected_issuer {
        return Err(PlatformVerifyError::IssuerMismatch {
            expected: expected_issuer.to_string(),
            got: claims.iss,
        });
    }
    if claims.sub.is_empty() {
        return Err(PlatformVerifyError::MissingSubject);
    }
    let _ = (claims.exp, claims.iat, claims.nbf, &claims.jti, &claims.client_id);
    Ok(claims)
}

fn map_claims(claims: PlatformAccessClaims) -> Result<VerifiedToken, PlatformVerifyError> {
    Ok(VerifiedToken {
        provider_subject: claims.sub,
        email: None,
        email_verified: false,
        session_id: None,
        provider_authz: ProviderAuthz::OAuthScope(resource_server_scope_claim(&claims.scope)),
        exp: claims.exp,
        client_id: Some(claims.client_id),
        iat: Some(claims.iat),
        aud: Some(claims.aud.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use ed25519_dalek::pkcs8::EncodePrivateKey;
    use ed25519_dalek::SigningKey;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use ntex::web::{self, HttpResponse};
    use parking_lot::RwLock;
    use serde_json::{json, Value};
    use std::future::Future;
    use std::pin::pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};
    use std::task::{Context, Poll, Waker};
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    const ISSUER: &str = "https://auth.zeroship.test";
    const AUDIENCE: &str = "control.zeroship.ai";
    const CLIENT_ID: &str = "zeroship-cli";
    const TEST_KID: &str = "platform-eddsa-test-kid";
    const OLD_TEST_KID: &str = "platform-old-eddsa-test-kid";
    const MISSING_TEST_KID: &str = "platform-missing-eddsa-test-kid";

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after Unix epoch")
            .as_secs()
    }

    fn claims() -> Value {
        let now = now_secs();
        json!({
            "iss": ISSUER,
            "sub": "7f06a762-f03b-4d59-a317-4f25f946a011",
            "aud": AUDIENCE,
            "exp": now + 300,
            "iat": now,
            "nbf": now.saturating_sub(1),
            "jti": "test-jti",
            "client_id": CLIENT_ID,
            "scope": "apps:read apps:deploy",
        })
    }

    fn provider() -> PlatformProvider {
        let cache = JwksCache::for_test(vec![ed_cached_key(TEST_KID, 11)]);
        PlatformProvider::with_jwks_cache_for_test(
            PlatformConfig::new(ISSUER, Some(format!("{ISSUER}/.well-known/jwks.json")))
                .expect("valid platform config"),
            cache,
        )
    }

    fn ed_cached_key(kid: &str, seed: u8) -> CachedKey {
        let sk = SigningKey::from_bytes(&[seed; 32]);
        let pub_b64 = URL_SAFE_NO_PAD.encode(sk.verifying_key().to_bytes());
        let decoding = jsonwebtoken::DecodingKey::from_ed_components(&pub_b64)
            .expect("decode Ed25519 public key");
        CachedKey {
            kid: kid.to_string(),
            alg: Algorithm::EdDSA,
            decoding,
        }
    }

    fn encoding_key(seed: u8) -> EncodingKey {
        let sk = SigningKey::from_bytes(&[seed; 32]);
        let pkcs8 = sk.to_pkcs8_der().expect("encode pkcs8");
        EncodingKey::from_ed_der(pkcs8.as_bytes())
    }

    fn sign_eddsa(claims: &Value, kid: &str, seed: u8) -> String {
        let mut header = Header::new(Algorithm::EdDSA);
        header.typ = Some(PLATFORM_ACCESS_TOKEN_TYP.to_string());
        header.kid = Some(kid.to_string());
        encode(&header, claims, &encoding_key(seed)).expect("EdDSA token")
    }

    fn wrong_alg_hs256_token(claims: &Value, kid: &str) -> String {
        let mut header = Header::new(Algorithm::HS256);
        header.typ = Some(PLATFORM_ACCESS_TOKEN_TYP.to_string());
        header.kid = Some(kid.to_string());
        encode(
            &header,
            claims,
            &jsonwebtoken::EncodingKey::from_secret(b"wrong-family-secret"),
        )
        .expect("HS256 token")
    }

    fn unsigned_none_alg_token(claims: &Value, kid: &str) -> String {
        let header = json!({
            "alg": "none",
            "typ": PLATFORM_ACCESS_TOKEN_TYP,
            "kid": kid,
        });
        format!(
            "{}.{}.",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).expect("header json")),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("claims json")),
        )
    }

    fn poll_ready<T>(future: impl Future<Output = T>) -> T {
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut future = pin!(future);
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("test future unexpectedly performed async IO"),
        }
    }

    #[test]
    fn config_defaults_jwks_url_from_issuer() {
        let config = PlatformConfig::new("https://auth.zeroship.test/", None)
            .expect("valid config");
        assert_eq!(config.issuer, ISSUER);
        assert_eq!(
            config.jwks_url,
            "https://auth.zeroship.test/.well-known/jwks.json"
        );
    }

    #[test]
    fn platform_eddsa_access_token_verifies_and_maps_claims() {
        let token = sign_eddsa(&claims(), TEST_KID, 11);
        let verified = poll_ready(provider().verify_token(&token)).expect("token verifies");

        assert_eq!(
            verified.provider_subject,
            "7f06a762-f03b-4d59-a317-4f25f946a011"
        );
        assert_eq!(
            verified.provider_authz,
            ProviderAuthz::OAuthScope("apps:read apps:deploy".to_string())
        );
        assert_eq!(
            verified.aud.as_deref(),
            Some(&["control.zeroship.ai".to_string()][..])
        );
        assert_eq!(verified.email, None);
        assert!(!verified.email_verified);
    }

    #[test]
    fn platform_rejects_wrong_type() {
        let mut header = Header::new(Algorithm::EdDSA);
        header.typ = Some("JWT".to_string());
        header.kid = Some(TEST_KID.to_string());
        let token = encode(&header, &claims(), &encoding_key(11)).expect("id-token-shaped JWT");

        let err = poll_ready(provider().verify_token(&token)).expect_err("wrong typ rejects");
        assert!(matches!(err, PlatformVerifyError::WrongType { .. }), "got: {err:?}");
    }

    #[test]
    fn platform_alg_pin_rejects_none_and_wrong_alg() {
        let claims = claims();
        let none = unsigned_none_alg_token(&claims, TEST_KID);
        poll_ready(provider().verify_token(&none)).expect_err("alg=none must reject");

        let hs256 = wrong_alg_hs256_token(&claims, TEST_KID);
        let err = poll_ready(provider().verify_token(&hs256)).expect_err("HS256 must reject");
        assert!(
            matches!(err, PlatformVerifyError::UnsupportedAlgorithm(Algorithm::HS256)),
            "got: {err:?}"
        );
    }

    #[test]
    fn platform_rejects_wrong_issuer_and_expired_token() {
        let mut wrong_issuer = claims();
        wrong_issuer["iss"] = json!("https://wrong-issuer.test");
        let token = sign_eddsa(&wrong_issuer, TEST_KID, 11);
        poll_ready(provider().verify_token(&token)).expect_err("wrong issuer rejects");

        let mut expired = claims();
        expired["exp"] = json!(now_secs().saturating_sub(60));
        let token = sign_eddsa(&expired, TEST_KID, 11);
        poll_ready(provider().verify_token(&token)).expect_err("expired token rejects");
    }

    #[compio::test]
    async fn platform_jwks_refreshes_once_on_kid_miss() {
        let mock = MockJwks::start(jwks_body(OLD_TEST_KID, 22));
        let cache = JwksCache::new(mock.jwks_url())
            .with_fetch_timeout_for_test(Duration::from_millis(500));
        cache.refresh().await.expect("initial old-key JWKS refresh");
        assert_eq!(mock.hits(), 1, "initial cache prime fetches once");

        mock.set_jwks_body(jwks_body(TEST_KID, 11));
        let provider = PlatformProvider::with_jwks_cache_for_test(
            PlatformConfig::new(ISSUER, Some(mock.jwks_url())).expect("valid config"),
            cache,
        );

        let rotated_kid_token = sign_eddsa(&claims(), TEST_KID, 11);
        provider
            .verify_token(&rotated_kid_token)
            .await
            .expect("rotated kid verifies after one forced refresh");
        assert_eq!(
            mock.hits(),
            2,
            "fresh-cache kid miss triggers exactly one forced refresh"
        );

        let missing_kid_token = sign_eddsa(&claims(), MISSING_TEST_KID, 11);
        let hits_before = mock.hits();
        let err = provider
            .verify_token(&missing_kid_token)
            .await
            .expect_err("never-existing kid still rejects");
        assert!(
            matches!(
                err,
                PlatformVerifyError::NoMatchingKey(ref kid) if kid == MISSING_TEST_KID
            ),
            "got: {err:?}"
        );
        assert_eq!(
            mock.hits(),
            hits_before + 1,
            "unknown attacker-controlled kid gets one forced refresh, not a fetch loop"
        );
    }

    fn jwks_body(kid: &str, seed: u8) -> String {
        let sk = SigningKey::from_bytes(&[seed; 32]);
        json!({
            "keys": [{
                "kid": kid,
                "kty": "OKP",
                "alg": "EdDSA",
                "crv": "Ed25519",
                "x": URL_SAFE_NO_PAD.encode(sk.verifying_key().to_bytes()),
            }]
        })
        .to_string()
    }

    struct MockJwksState {
        jwks_body: RwLock<String>,
        hits: AtomicUsize,
    }

    struct MockJwks {
        base: String,
        state: Arc<MockJwksState>,
        shutdown: Option<mpsc::Sender<()>>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl MockJwks {
        fn start(jwks_body: String) -> Self {
            let state = Arc::new(MockJwksState {
                jwks_body: RwLock::new(jwks_body),
                hits: AtomicUsize::new(0),
            });
            let factory_state = state.clone();
            let (started_tx, started_rx) = mpsc::channel();
            let (shutdown_tx, shutdown_rx) = mpsc::channel();
            let thread = thread::spawn(move || {
                ntex::rt::System::build()
                    .name("platform-jwks-mock")
                    .testing()
                    .build(ntex::rt::DefaultRuntime)
                    .block_on(async move {
                        let server = web::test::server(move || {
                            let state = factory_state.clone();
                            async move {
                                web::App::new().state(state).service(
                                    web::resource("/.well-known/jwks.json")
                                        .route(web::get().to(jwks_handler)),
                                )
                            }
                        })
                        .await;
                        let addr = server.addr();
                        started_tx.send(addr).expect("send mock server addr");
                        let _ = shutdown_rx.recv();
                        drop(server);
                    });
            });
            let addr = started_rx.recv().expect("mock server starts");
            Self {
                base: format!("http://{addr}"),
                state,
                shutdown: Some(shutdown_tx),
                thread: Some(thread),
            }
        }

        fn jwks_url(&self) -> String {
            format!("{}/.well-known/jwks.json", self.base)
        }

        fn set_jwks_body(&self, body: String) {
            *self.state.jwks_body.write() = body;
        }

        fn hits(&self) -> usize {
            self.state.hits.load(Ordering::SeqCst)
        }
    }

    impl Drop for MockJwks {
        fn drop(&mut self) {
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    async fn jwks_handler(state: web::types::State<Arc<MockJwksState>>) -> HttpResponse {
        state.hits.fetch_add(1, Ordering::SeqCst);
        HttpResponse::Ok()
            .content_type("application/json")
            .body(state.jwks_body.read().clone())
    }
}
