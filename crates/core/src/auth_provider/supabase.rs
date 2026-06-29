use std::collections::HashSet;
use std::fmt;

use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use thiserror::Error;

use crate::auth_provider::{ProviderAuthz, VerifiedToken};
use crate::oidc_verify::{CachedKey, JwksCache, OidcError};

const SUPABASE_AUTHENTICATED_AUD: &str = "authenticated";
const SUPABASE_ASYMMETRIC_ALGORITHMS: &[Algorithm] = &[
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::ES256,
];
const SUPABASE_RSA_ALGORITHMS: &[Algorithm] =
    &[Algorithm::RS256, Algorithm::RS384, Algorithm::RS512];
const SUPABASE_EC_ALGORITHMS: &[Algorithm] = &[Algorithm::ES256];

/// Supabase Auth provider configuration.
///
/// The verification mode is pinned at construction time. There is no
/// "try HS256 then JWKS" fallback because that would reopen key-confusion
/// space when an operator accidentally configures both.
#[derive(Clone)]
pub struct SupabaseConfig {
    pub url: String,
    pub anon_key: String,
    pub service_role_key: Option<String>,
    pub verification: SupabaseVerification,
}

impl SupabaseConfig {
    /// Build a fail-closed Supabase config.
    ///
    /// Exactly one verification mode must be set:
    /// - `jwt_secret` for HS256 verification, or
    /// - `jwks_url` for asymmetric/JWKS verification.
    ///
    /// `issuer` is common to both modes and is always explicit config; hosted
    /// Supabase and self-hosted GoTrue do not share a single safe default.
    pub fn new(
        url: impl Into<String>,
        anon_key: impl Into<String>,
        service_role_key: Option<String>,
        jwt_secret: Option<String>,
        jwks_url: Option<String>,
        issuer: impl Into<String>,
    ) -> Result<Self, SupabaseConfigError> {
        let url = url.into();
        let anon_key = anon_key.into();
        let issuer = issuer.into();

        if url.trim().is_empty() {
            return Err(SupabaseConfigError::EmptyUrl);
        }

        let has_hs256 = jwt_secret.is_some();
        let has_jwks = jwks_url.is_some();
        match (has_hs256, has_jwks) {
            (true, true) => return Err(SupabaseConfigError::BothVerificationModes),
            (false, false) => return Err(SupabaseConfigError::NoVerificationMode),
            _ => {}
        }

        if issuer.trim().is_empty() {
            return Err(SupabaseConfigError::EmptyIssuer);
        }

        let verification = if let Some(jwt_secret) = jwt_secret {
            if jwt_secret.is_empty() {
                return Err(SupabaseConfigError::EmptyJwtSecret);
            }
            SupabaseVerification::Hs256 { jwt_secret, issuer }
        } else {
            let jwks_url = jwks_url.expect("jwks_url is present when HS256 mode is absent");
            if jwks_url.trim().is_empty() {
                return Err(SupabaseConfigError::EmptyJwksUrl);
            }
            SupabaseVerification::Jwks { jwks_url, issuer }
        };

        Ok(Self {
            url,
            anon_key,
            service_role_key,
            verification,
        })
    }
}

impl fmt::Debug for SupabaseConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SupabaseConfig")
            .field("url", &self.url)
            .field("anon_key", &"<redacted>")
            .field(
                "service_role_key",
                &self.service_role_key.as_ref().map(|_| "<redacted>"),
            )
            .field("verification", &self.verification)
            .finish()
    }
}

#[derive(Clone)]
pub enum SupabaseVerification {
    Hs256 { jwt_secret: String, issuer: String },
    Jwks { jwks_url: String, issuer: String },
}

impl SupabaseVerification {
    #[must_use]
    pub fn issuer(&self) -> &str {
        match self {
            Self::Hs256 { issuer, .. } | Self::Jwks { issuer, .. } => issuer,
        }
    }
}

impl fmt::Debug for SupabaseVerification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hs256 { issuer, .. } => f
                .debug_struct("Hs256")
                .field("jwt_secret", &"<redacted>")
                .field("issuer", issuer)
                .finish(),
            Self::Jwks { jwks_url, issuer } => f
                .debug_struct("Jwks")
                .field("jwks_url", jwks_url)
                .field("issuer", issuer)
                .finish(),
        }
    }
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum SupabaseConfigError {
    #[error("SUPABASE_URL is required")]
    EmptyUrl,

    #[error("configure exactly one Supabase verification mode; both HS256 and JWKS were set")]
    BothVerificationModes,

    #[error("configure exactly one Supabase verification mode; neither HS256 nor JWKS was set")]
    NoVerificationMode,

    #[error("SUPABASE_JWT_ISSUER is required")]
    EmptyIssuer,

    #[error("SUPABASE_JWT_SECRET is required for HS256 verification")]
    EmptyJwtSecret,

    #[error("SUPABASE_JWKS_URL is required for asymmetric verification")]
    EmptyJwksUrl,
}

#[derive(Debug, Clone)]
pub struct SupabaseProvider {
    config: SupabaseConfig,
    jwks: Option<JwksCache>,
}

impl SupabaseProvider {
    #[must_use]
    pub fn new(config: SupabaseConfig) -> Self {
        let jwks = match &config.verification {
            SupabaseVerification::Hs256 { .. } => None,
            SupabaseVerification::Jwks { jwks_url, .. } => Some(JwksCache::new(jwks_url.clone())),
        };
        Self { config, jwks }
    }

    #[cfg(test)]
    fn with_jwks_cache_for_test(config: SupabaseConfig, jwks: JwksCache) -> Self {
        Self {
            config,
            jwks: Some(jwks),
        }
    }

    #[allow(clippy::future_not_send)]
    pub async fn verify_token(&self, token: &str) -> Result<VerifiedToken, SupabaseVerifyError> {
        let claims = match &self.config.verification {
            SupabaseVerification::Hs256 { jwt_secret, issuer } => {
                verify_hs256(token, jwt_secret, issuer)?
            }
            SupabaseVerification::Jwks { issuer, .. } => {
                let jwks = self.jwks.as_ref().ok_or(SupabaseVerifyError::MissingJwksCache)?;
                verify_asymmetric(token, jwks, issuer).await?
            }
        };
        map_claims(claims)
    }

    #[must_use]
    pub fn issuer(&self) -> &str {
        self.config.verification.issuer()
    }
}

#[derive(Debug, Error)]
pub enum SupabaseVerifyError {
    #[error("decode JWT header: {0}")]
    DecodeHeader(String),

    #[error("JWT verify: {0}")]
    Jwt(String),

    #[error("JWKS: {0}")]
    Jwks(#[from] OidcError),

    #[error("no JWK with matching kid: {0}")]
    NoMatchingKey(String),

    #[error("asymmetric Supabase JWT requires a kid header")]
    MissingKid,

    #[error("unsupported Supabase JWT algorithm: {0:?}")]
    UnsupportedAlgorithm(Algorithm),

    #[error("supabase provider missing JWKS cache")]
    MissingJwksCache,

    #[error("issuer mismatch: expected {expected}, got {got}")]
    IssuerMismatch { expected: String, got: String },

    #[error("audience must contain authenticated; got {aud:?}")]
    AudienceMissingAuthenticated { aud: Vec<String> },

    #[error("supabase token missing subject")]
    MissingSubject,
}

#[derive(Debug, Deserialize)]
struct SupabaseClaims {
    sub: String,
    iss: String,
    aud: ClaimStrings,
    exp: u64,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    role: String,
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

fn verify_hs256(
    token: &str,
    jwt_secret: &str,
    expected_issuer: &str,
) -> Result<SupabaseClaims, SupabaseVerifyError> {
    let validation = supabase_validation(expected_issuer, vec![Algorithm::HS256]);
    let data = decode::<SupabaseClaims>(
        token,
        &DecodingKey::from_secret(jwt_secret.as_bytes()),
        &validation,
    )
    .map_err(|e| SupabaseVerifyError::Jwt(e.to_string()))?;
    verify_registered_claims(data.claims, expected_issuer)
}

#[allow(clippy::future_not_send)]
async fn verify_asymmetric(
    token: &str,
    jwks: &JwksCache,
    expected_issuer: &str,
) -> Result<SupabaseClaims, SupabaseVerifyError> {
    let header =
        decode_header(token).map_err(|e| SupabaseVerifyError::DecodeHeader(e.to_string()))?;
    let kid = header.kid.clone().ok_or(SupabaseVerifyError::MissingKid)?;
    let alg = header.alg;
    if !SUPABASE_ASYMMETRIC_ALGORITHMS.contains(&alg) {
        return Err(SupabaseVerifyError::UnsupportedAlgorithm(alg));
    }

    let keys = jwks.keys().await?;
    let key = find_key(&keys, &kid, alg)?;
    let validation = supabase_validation(expected_issuer, asymmetric_algorithms_for(alg)?);
    let data = decode::<SupabaseClaims>(token, &key.decoding, &validation)
        .map_err(|e| SupabaseVerifyError::Jwt(e.to_string()))?;
    verify_registered_claims(data.claims, expected_issuer)
}

fn find_key<'a>(
    keys: &'a [CachedKey],
    kid: &str,
    alg: Algorithm,
) -> Result<&'a CachedKey, SupabaseVerifyError> {
    keys.iter()
        .find(|key| key.kid == kid && key.alg == alg)
        .ok_or_else(|| SupabaseVerifyError::NoMatchingKey(kid.to_string()))
}

fn asymmetric_algorithms_for(alg: Algorithm) -> Result<Vec<Algorithm>, SupabaseVerifyError> {
    match alg {
        Algorithm::RS256 | Algorithm::RS384 | Algorithm::RS512 => {
            Ok(SUPABASE_RSA_ALGORITHMS.to_vec())
        }
        Algorithm::ES256 => Ok(SUPABASE_EC_ALGORITHMS.to_vec()),
        other => Err(SupabaseVerifyError::UnsupportedAlgorithm(other)),
    }
}

fn supabase_validation(expected_issuer: &str, algorithms: Vec<Algorithm>) -> Validation {
    let mut validation = Validation::new(
        *algorithms
            .first()
            .expect("Supabase validation must pin at least one JWT algorithm"),
    );
    validation.algorithms = algorithms;
    validation.set_issuer(&[expected_issuer]);
    validation.validate_aud = false;
    validation.validate_nbf = true;
    validation.leeway = 0;
    validation.required_spec_claims = ["exp", "iss", "aud", "sub"]
        .into_iter()
        .map(str::to_string)
        .collect::<HashSet<_>>();
    validation
}

fn verify_registered_claims(
    claims: SupabaseClaims,
    expected_issuer: &str,
) -> Result<SupabaseClaims, SupabaseVerifyError> {
    if claims.iss != expected_issuer {
        return Err(SupabaseVerifyError::IssuerMismatch {
            expected: expected_issuer.to_string(),
            got: claims.iss,
        });
    }
    if !claims
        .aud
        .0
        .iter()
        .any(|aud| aud == SUPABASE_AUTHENTICATED_AUD)
    {
        return Err(SupabaseVerifyError::AudienceMissingAuthenticated {
            aud: claims.aud.0.clone(),
        });
    }
    if claims.sub.is_empty() {
        return Err(SupabaseVerifyError::MissingSubject);
    }
    Ok(claims)
}

fn map_claims(claims: SupabaseClaims) -> Result<VerifiedToken, SupabaseVerifyError> {
    Ok(VerifiedToken {
        provider_subject: claims.sub,
        email: claims.email.and_then(|email| {
            if email.is_empty() {
                None
            } else {
                Some(email)
            }
        }),
        // GoTrue JWTs do not carry trustworthy email verification state.
        // The identity-bridge slice must look up `email_confirmed_at` via
        // the service-role admin API when it decides whether to link by email.
        email_verified: false,
        session_id: claims.session_id,
        provider_authz: ProviderAuthz::GoTrueRole(claims.role),
        exp: claims.exp,
        aud: Some(claims.aud.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::{json, Value};
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};
    use std::time::{SystemTime, UNIX_EPOCH};

    const URL: &str = "https://project.supabase.co";
    const ISSUER: &str = "https://project.supabase.co/auth/v1";
    const ANON_KEY: &str = "anon-key";
    const JWT_SECRET: &str = "test-supabase-jwt-secret";
    const TEST_KID: &str = "supabase-rs256-test-kid";
    const RSA_N: &str = "xNHoKDPStS1XlmQMubUWC59gR2CVsOeq4SabVnAXkyn5mWexM9yCLDShmTNkhq5mNII1c_GwbQZmUnTVtw3pFU_WsiRMAIB5ypSw-XzeoKq0IYz-IQimpQDpL0Gpih_rRIXwHWPB8C-Ia3tOy09qMiFLTluv7FTylaF0K2DXcoHOyWm4Ymbpn7LYI_LnP7aJXLIt3D8TKdNRRJ8zMRf-6m_h-jlhx43v9jHmPYZp63FaOUIyAZPnXj-8Jvq6qmMTPf_1rpIw6pyd9v6MnXtNw7Lj92s80WVRT9ZqmPwNS5avCy6kManMiqA2IaH2ygLsKMgo89FvLEAIa1u_2Yc7Iw";
    const RSA_E: &str = "AQAB";
    const RSA_PRIVATE_PEM: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvwIBADANBgkqhkiG9w0BAQEFAASCBKkwggSlAgEAAoIBAQDE0egoM9K1LVeW
ZAy5tRYLn2BHYJWw56rhJptWcBeTKfmZZ7Ez3IIsNKGZM2SGrmY0gjVz8bBtBmZS
dNW3DekVT9ayJEwAgHnKlLD5fN6gqrQhjP4hCKalAOkvQamKH+tEhfAdY8HwL4hr
e07LT2oyIUtOW6/sVPKVoXQrYNdygc7JabhiZumfstgj8uc/tolcsi3cPxMp01FE
nzMxF/7qb+H6OWHHje/2MeY9hmnrcVo5QjIBk+deP7wm+rqqYxM9//WukjDqnJ32
/oyde03DsuP3azzRZVFP1mqY/A1Llq8LLqQxqcyKoDYhofbKAuwoyCjz0W8sQAhr
W7/ZhzsjAgMBAAECggEAD2hpVsBb1fAjQ7Q4ZM9e1vEWne/bOBqiv3aAwZ8L0Wr/
TbmW3zr7e89w+SfTTlHX0XOCEL4SdC6Ekx7vXiG6Jf92jMKXqpBkOG40fot+eDB0
4o2BlX9yYEd2ecsXpScDiX08w2g8Vfu6n8Nq8zKX2y3NEejOmF88EGevyveNVjrd
GipayjVylvKoXOJBBhJwkCd192A4Usi+0LuUSjacMVp7kEKFowpZ32DuUv3vS82E
Fo1ECR7gP3UEYD3oAqFRuxMN8LSbQa4aKtXRvXmWY87pdlGGYKW/AkWKt5sZ5tW8
l8XZ5JK103yB75D/nBSjxirMaJPuw2fgGxDIgsPnAQKBgQDi+AhOlHHJHOCly9Sn
UtKJzh5s9+I5sPiJCUduih2v3U2o87bkM1JKWEDdcCR6SaUG6Dzth9K4piuqBg2g
S4MYzIQWzJkMFRL7yxlL6FQbUSTfyd4kYZGhjaMDJl3D94SVPrx2y6XBq5ZvppHc
QXgWqdwhr0/+AF98RwOBnk/KIwKBgQDd/qow98jtfvERXBqpZIwMAHFEp62GKu+c
BghOFK7LGUZnGAUAyCbxmYKs8cuBUkVIaRQbkK/f45nUcTKMSQDXxQxZPmgEte87
lcEMWayhU0dfZ4IIiSszA3bkKNqJmYvazWWYSHiiJrMU/bLC4lh+lWwWRJ0EuhQd
uKvK/X9bAQKBgQCAQqAXH+YJO4trxfr/L1uQym0BMTejWHGqpxa5zc0m882OG2OQ
I7xuDN9jA5tpi7J5a8X6iRW4iRcFtvP+UI3d9rzyUV5vqH0Y01YRQlI9Oaa33FDv
iD+O5wZmoki8lGRVHqXMEBs0ja2unJeyu0CMtiKS2oo+xKExDsRZfEgktwKBgQDK
MUh730OfpM4WfKg//rdbuw9vc7WljPe+SPRJgacOjw/DmGn+I07tIF+X+4baW7+E
y0goLykxJ5EVoKBki5176RptMlz1ZWvm/mfdQtPr//jy2UjjU2QIS7B+8QLS7wol
mIxfHirZrZvQk528yQHHEXtn8Mh+5KirxWabNTZJAQKBgQCjCdsy5+r84ZHUbqI2
aKYbKQHarCd8UclVtQ6KH0WsKYngUbTm7/6o6ULFH7h1m02+jdckFHTJDlnd6A78
LSJ0kD5O3Q7wnmUdnCZyFsYh7dgqHro84GBSHtQXaxcN04YT0FLdJUYVG1tUEWMy
chfOO390zPo2KvlyenOqynqSZA==
-----END PRIVATE KEY-----"#;

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after Unix epoch")
            .as_secs()
    }

    fn claims(aud: Value) -> Value {
        let now = now_secs();
        json!({
            "iss": ISSUER,
            "sub": "7f06a762-f03b-4d59-a317-4f25f946a011",
            "aud": aud,
            "exp": now + 300,
            "iat": now,
            "nbf": now.saturating_sub(1),
            "email": "creator@example.test",
            "session_id": "19e5b01d-74c5-4df0-9860-43d8d2c22396",
            "role": "authenticated",
            "email_verified": true,
            "user_metadata": {
                "email_verified": true
            }
        })
    }

    fn hs256_provider() -> SupabaseProvider {
        SupabaseProvider::new(
            SupabaseConfig::new(
                URL,
                ANON_KEY,
                None,
                Some(JWT_SECRET.to_string()),
                None,
                ISSUER,
            )
            .expect("valid HS256 config"),
        )
    }

    fn jwks_provider() -> SupabaseProvider {
        let decoding = DecodingKey::from_rsa_components(RSA_N, RSA_E).expect("RSA public key");
        let cache = JwksCache::for_test(vec![CachedKey {
            kid: TEST_KID.to_string(),
            alg: Algorithm::RS256,
            decoding,
        }]);
        SupabaseProvider::with_jwks_cache_for_test(
            SupabaseConfig::new(
                URL,
                ANON_KEY,
                Some("service-role-key".to_string()),
                None,
                Some(format!("{URL}/auth/v1/.well-known/jwks.json")),
                ISSUER,
            )
            .expect("valid JWKS config"),
            cache,
        )
    }

    fn sign_hs256(claims: &Value, secret: &str) -> String {
        encode(
            &Header::new(Algorithm::HS256),
            claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .expect("HS256 JWT")
    }

    fn sign_rs256(claims: &Value, kid: &str) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.to_string());
        encode(
            &header,
            claims,
            &EncodingKey::from_rsa_pem(RSA_PRIVATE_PEM.as_bytes()).expect("RSA private key"),
        )
        .expect("RS256 JWT")
    }

    fn unsigned_none_alg_token(claims: &Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"none","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("claims JSON"));
        format!("{header}.{payload}.")
    }

    fn poll_ready<T>(future: impl Future<Output = T>) -> T {
        let waker = Waker::noop();
        let mut cx = Context::from_waker(&waker);
        let mut future = pin!(future);
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("test future unexpectedly performed async IO"),
        }
    }

    #[test]
    fn supabase_config_rejects_ambiguous_or_missing_modes() {
        let both = SupabaseConfig::new(
            URL,
            ANON_KEY,
            None,
            Some(JWT_SECRET.to_string()),
            Some(format!("{URL}/auth/v1/.well-known/jwks.json")),
            ISSUER,
        );
        assert!(matches!(
            both,
            Err(SupabaseConfigError::BothVerificationModes)
        ));

        let neither = SupabaseConfig::new(URL, ANON_KEY, None, None, None, ISSUER);
        assert!(matches!(
            neither,
            Err(SupabaseConfigError::NoVerificationMode)
        ));

        let empty_url = SupabaseConfig::new(
            "",
            ANON_KEY,
            None,
            Some(JWT_SECRET.to_string()),
            None,
            ISSUER,
        );
        assert!(matches!(empty_url, Err(SupabaseConfigError::EmptyUrl)));
    }

    #[test]
    fn supabase_config_accepts_valid_hs256_and_jwks_modes() {
        let hs256 = SupabaseConfig::new(
            URL,
            ANON_KEY,
            None,
            Some(JWT_SECRET.to_string()),
            None,
            ISSUER,
        )
        .expect("HS256 mode is valid");
        assert!(matches!(
            hs256.verification,
            SupabaseVerification::Hs256 { .. }
        ));

        let jwks = SupabaseConfig::new(
            URL,
            ANON_KEY,
            Some("service-role-key".to_string()),
            None,
            Some(format!("{URL}/auth/v1/.well-known/jwks.json")),
            ISSUER,
        )
        .expect("JWKS mode is valid");
        assert!(matches!(jwks.verification, SupabaseVerification::Jwks { .. }));
    }

    #[test]
    fn supabase_validation_pins_algorithms() {
        let hs256 = supabase_validation(ISSUER, vec![Algorithm::HS256]);
        assert_eq!(hs256.algorithms, vec![Algorithm::HS256]);
        assert!(!hs256.validate_aud);
        assert!(hs256.validate_nbf);
        assert_eq!(hs256.leeway, 0);

        let asymmetric =
            supabase_validation(ISSUER, SUPABASE_ASYMMETRIC_ALGORITHMS.to_vec());
        assert_eq!(asymmetric.algorithms, SUPABASE_ASYMMETRIC_ALGORITHMS);
        assert!(!asymmetric.algorithms.contains(&Algorithm::HS256));
    }

    #[test]
    fn supabase_hs256_verifies_and_ignores_untrusted_email_verified_claims() {
        let provider = hs256_provider();
        let token = sign_hs256(&claims(json!("authenticated")), JWT_SECRET);

        let verified = poll_ready(provider.verify_token(&token)).expect("token verifies");

        assert_eq!(
            verified.provider_subject,
            "7f06a762-f03b-4d59-a317-4f25f946a011"
        );
        assert_eq!(verified.email.as_deref(), Some("creator@example.test"));
        assert!(
            !verified.email_verified,
            "GoTrue token email_verified/user_metadata.email_verified must be ignored"
        );
        assert_eq!(
            verified.session_id.as_deref(),
            Some("19e5b01d-74c5-4df0-9860-43d8d2c22396")
        );
        assert_eq!(
            verified.provider_authz,
            ProviderAuthz::GoTrueRole("authenticated".to_string())
        );
        assert_eq!(verified.aud.as_deref(), Some(&["authenticated".to_string()][..]));
    }

    #[test]
    fn supabase_hs256_accepts_string_and_array_aud() {
        let provider = hs256_provider();
        for aud in [json!("authenticated"), json!(["authenticated", "other"])] {
            let token = sign_hs256(&claims(aud), JWT_SECRET);
            poll_ready(provider.verify_token(&token)).expect("authenticated audience verifies");
        }
    }

    #[test]
    fn supabase_hs256_rejects_wrong_issuer() {
        let provider = hs256_provider();
        let mut claims = claims(json!("authenticated"));
        claims["iss"] = json!("supabase");
        let token = sign_hs256(&claims, JWT_SECRET);

        poll_ready(provider.verify_token(&token)).expect_err("wrong issuer must reject");
    }

    #[test]
    fn supabase_hs256_rejects_expired_token() {
        let provider = hs256_provider();
        let mut claims = claims(json!("authenticated"));
        claims["exp"] = json!(now_secs().saturating_sub(300));
        let token = sign_hs256(&claims, JWT_SECRET);

        poll_ready(provider.verify_token(&token)).expect_err("expired token must reject");
    }

    #[test]
    fn supabase_hs256_rejects_aud_without_authenticated() {
        let provider = hs256_provider();
        let token = sign_hs256(&claims(json!(["service_role", "other"])), JWT_SECRET);

        poll_ready(provider.verify_token(&token))
            .expect_err("aud without authenticated must reject");
    }

    #[test]
    fn supabase_hs256_rejects_wrong_secret() {
        let provider = hs256_provider();
        let token = sign_hs256(&claims(json!("authenticated")), "wrong-secret");

        poll_ready(provider.verify_token(&token)).expect_err("wrong secret must reject");
    }

    #[test]
    fn supabase_hs256_rejects_rs256_and_none_alg_confusion() {
        let provider = hs256_provider();
        let claims = claims(json!("authenticated"));

        let rs256 = sign_rs256(&claims, TEST_KID);
        poll_ready(provider.verify_token(&rs256))
            .expect_err("RS256 token must not verify against HS256 config");

        let none = unsigned_none_alg_token(&claims);
        poll_ready(provider.verify_token(&none))
            .expect_err("alg=none token must not verify against HS256 config");
    }

    #[test]
    fn supabase_jwks_rs256_verifies_and_maps_claims() {
        let provider = jwks_provider();
        let token = sign_rs256(&claims(json!(["authenticated", "other"])), TEST_KID);

        let verified = poll_ready(provider.verify_token(&token)).expect("RS256 token verifies");

        assert_eq!(
            verified.provider_subject,
            "7f06a762-f03b-4d59-a317-4f25f946a011"
        );
        assert_eq!(verified.email.as_deref(), Some("creator@example.test"));
        assert!(!verified.email_verified);
        assert_eq!(
            verified.provider_authz,
            ProviderAuthz::GoTrueRole("authenticated".to_string())
        );
        assert_eq!(
            verified.aud.as_deref(),
            Some(&["authenticated".to_string(), "other".to_string()][..])
        );
    }

    #[test]
    fn supabase_jwks_rejects_hs256_key_confusion() {
        let provider = jwks_provider();
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some(TEST_KID.to_string());
        let token = encode(
            &header,
            &claims(json!("authenticated")),
            &EncodingKey::from_secret(JWT_SECRET.as_bytes()),
        )
        .expect("HS256 JWT");

        poll_ready(provider.verify_token(&token))
            .expect_err("HS256 token must not verify against JWKS config");
    }

    #[test]
    fn supabase_jwks_rejects_wrong_kid() {
        let provider = jwks_provider();
        let token = sign_rs256(&claims(json!("authenticated")), "wrong-kid");

        poll_ready(provider.verify_token(&token)).expect_err("wrong kid must reject");
    }
}
