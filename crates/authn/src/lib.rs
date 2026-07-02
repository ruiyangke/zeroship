//! Shared bearer authentication for control-like services.

use std::collections::HashSet;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Instant;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use chrono::{DateTime, Utc};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use ntex::web;
use ntex::web::HttpResponse;
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;
use zeroship_authz as authz;
use zeroship_core::auth_provider::{AuthProvider, ProviderAuthz, VerifyTokenError};
use zeroship_core::wrapper_revocation::{
    family_revoked_at, revoked_after_for, RevocationCache, REVOCATION_CACHE_MAX_ENTRIES,
};

pub type HttpRejection = web::Error;

const PAT_AUDIENCE: &str = "control.zeroship.ai";
const PAT_ISSUER: &str = "https://api.zeroship.ai";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatClaims {
    pub iss: String,
    pub aud: String,
    pub sub: String,
    pub owner: String,
    pub tid: String,
    pub jti: String,
    pub iat: i64,
    pub exp: i64,
    pub scope: String,
    pub policy_hash: String,
    pub nonce: String,
}

pub struct PatIssuer {
    private_der: Vec<u8>,
    decoding_key: DecodingKey,
    kid: String,
}

impl std::fmt::Debug for PatIssuer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PatIssuer")
            .field("kid", &self.kid)
            .finish_non_exhaustive()
    }
}

impl PatIssuer {
    pub fn new(signing_key: &ed25519_dalek::SigningKey) -> Result<Self, String> {
        use ed25519_dalek::pkcs8::EncodePrivateKey;

        let private_der = signing_key
            .to_pkcs8_der()
            .map_err(|err| format!("PAT signing key PKCS#8 encode: {err}"))?
            .as_bytes()
            .to_vec();
        let decoding_key = DecodingKey::from_ed_der(signing_key.verifying_key().as_bytes());
        let kid = jwk_thumbprint(signing_key);
        Ok(Self {
            private_der,
            decoding_key,
            kid,
        })
    }

    #[must_use]
    pub fn dev_insecure() -> Self {
        let key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        Self::new(&key).expect("generated dev PAT key is valid")
    }

    pub fn issue(
        &self,
        token_id: Uuid,
        owner_id: Uuid,
        policy_hash: String,
        expires_at: DateTime<Utc>,
    ) -> Result<String, String> {
        let now = now_unix()?;
        let mut nonce = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(nonce);
        let token_id = token_id.to_string();
        let owner_id = owner_id.to_string();
        let claims = PatClaims {
            iss: PAT_ISSUER.to_owned(),
            aud: PAT_AUDIENCE.to_owned(),
            sub: owner_id.clone(),
            owner: owner_id,
            tid: token_id.clone(),
            jti: token_id,
            iat: now,
            exp: expires_at.timestamp(),
            scope: "pat".to_owned(),
            policy_hash,
            nonce,
        };

        let mut header = Header::new(Algorithm::EdDSA);
        header.typ = Some("pat+jwt".to_owned());
        header.kid = Some(self.kid.clone());
        let key = EncodingKey::from_ed_der(&self.private_der);
        encode(&header, &claims, &key).map_err(|err| format!("PAT JWT encode: {err}"))
    }

    pub fn verify(&self, token: &str) -> Result<PatClaims, String> {
        let header = jsonwebtoken::decode_header(token)
            .map_err(|err| format!("PAT JWT header decode: {err}"))?;
        if header.typ.as_deref() != Some("pat+jwt") {
            return Err(format!("unexpected PAT JWT typ: {:?}", header.typ));
        }
        match header.kid.as_deref() {
            Some(kid) if kid == self.kid => {}
            Some(kid) => return Err(format!("unknown PAT JWT kid: {kid}")),
            None => return Err("missing PAT JWT kid".to_owned()),
        }

        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_issuer(&[PAT_ISSUER]);
        validation.set_audience(&[PAT_AUDIENCE]);
        decode::<PatClaims>(token, &self.decoding_key, &validation)
            .map(|data| data.claims)
            .map_err(|err| format!("PAT JWT verify: {err}"))
    }
}

pub fn load_signing_key_from_path(path: &Path) -> Result<ed25519_dalek::SigningKey, String> {
    let bytes = std::fs::read(path)
        .map_err(|err| format!("read PAT signing key {}: {err}", path.display()))?;
    reject_insecure_permissions(path)?;

    if let Ok(text) = std::str::from_utf8(&bytes) {
        if text.contains("-----BEGIN PRIVATE KEY-----") {
            use ed25519_dalek::pkcs8::DecodePrivateKey;
            return ed25519_dalek::SigningKey::from_pkcs8_pem(text)
                .map_err(|err| format!("Ed25519 PAT PKCS#8 PEM: {err}"));
        }
    }

    use ed25519_dalek::pkcs8::DecodePrivateKey;
    ed25519_dalek::SigningKey::from_pkcs8_der(&bytes)
        .map_err(|err| format!("Ed25519 PAT PKCS#8 DER: {err}"))
}

#[derive(Debug)]
pub struct VerifiedPrincipal {
    pub principal_id: Uuid,
    pub token_id: Option<Uuid>,
    pub token_policy: Option<authz::Policy>,
    pub mfa_verified: bool,
    pub mfa_age_seconds: Option<u32>,
    pub request_ip: Option<IpAddr>,
    pub request_id: String,
}

#[derive(Clone, Debug)]
pub struct BearerVerifier {
    pat_issuer: Arc<PatIssuer>,
    control_pg: Arc<compio_postgres::Client>,
    auth_provider: Arc<AuthProvider>,
    trusted_oauth_clients: HashSet<String>,
    expected_oauth_audience: String,
}

impl BearerVerifier {
    #[must_use]
    pub fn new(
        pat_issuer: Arc<PatIssuer>,
        control_pg: Arc<compio_postgres::Client>,
        auth_provider: Arc<AuthProvider>,
        trusted_oauth_clients: HashSet<String>,
        expected_oauth_audience: String,
    ) -> Self {
        Self {
            pat_issuer,
            control_pg,
            auth_provider,
            trusted_oauth_clients,
            expected_oauth_audience,
        }
    }

    #[must_use]
    pub fn trusted_oauth_clients(&self) -> &HashSet<String> {
        &self.trusted_oauth_clients
    }

    pub async fn verify_bearer(
        &self,
        token: &str,
        request_ip: Option<IpAddr>,
        request_id: String,
    ) -> Result<VerifiedPrincipal, HttpRejection> {
        let claims = match self.pat_issuer.verify(token) {
            Ok(claims) => claims,
            Err(err) => {
                tracing::debug!(
                    error = %err,
                    "control: bearer was not a valid PAT; trying OAuth introspection"
                );
                return self
                    .oauth_guard_from_bearer(token, request_ip, request_id)
                    .await;
            }
        };
        let token_id = Uuid::parse_str(&claims.jti)
            .map_err(|_| web::error::ErrorUnauthorized("invalid bearer token id"))?;
        let owner_id = Uuid::parse_str(&claims.owner)
            .map_err(|_| web::error::ErrorUnauthorized("invalid bearer owner"))?;

        let rows = self
            .control_pg
            .query(
                "SELECT owner_id FROM zeroship.permission_tokens \
                 WHERE id = $1 \
                   AND owner_id = $2 \
                   AND policy_hash = $3 \
                   AND kind = 'pat' \
                   AND revoked_at IS NULL \
                   AND (expires_at IS NULL OR expires_at > NOW())",
                &[&token_id, &owner_id, &claims.policy_hash],
            )
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "control: permission token lookup failed");
                web::error::ErrorInternalServerError("permission token lookup failed")
            })?;
        let row = rows
            .first()
            .ok_or_else(|| web::error::ErrorUnauthorized("permission token not active"))?;
        let principal_id: Uuid = row.get("owner_id");

        if let Err(err) = self
            .control_pg
            .execute(
                "UPDATE zeroship.permission_tokens SET last_used_at = NOW() WHERE id = $1",
                &[&token_id],
            )
            .await
        {
            tracing::warn!(error = %err, "control: permission token last_used_at update failed");
        }

        Ok(VerifiedPrincipal {
            principal_id,
            token_id: Some(token_id),
            token_policy: None,
            mfa_verified: false,
            mfa_age_seconds: None,
            request_ip,
            request_id,
        })
    }

    async fn cached_revoked_after_for(
        &self,
        client_id: &str,
        sub: &str,
    ) -> Result<Option<i64>, HttpRejection> {
        let cache = control_revocation_cache();
        let now = Instant::now();
        if let Some(cached) = cache.get(client_id, sub, now) {
            return Ok(cached);
        }

        let revoked_after = revoked_after_for(&self.control_pg, client_id, sub)
            .await
            .map_err(|err| {
                tracing::error!(
                    error = %err,
                    client_id,
                    sub,
                    "control: token revocation lookup failed"
                );
                unauthorized_json("revocation_check_failed")
            })?;
        cache.store(client_id, sub, revoked_after, now);
        Ok(revoked_after)
    }

    async fn reject_revoked_platform_token(
        &self,
        client_id: Option<&str>,
        sub: &str,
        iat: Option<u64>,
    ) -> Result<(), HttpRejection> {
        let (Some(client_id), Some(iat)) = (client_id, iat) else {
            return Err(unauthorized_json("token_revocation_claims_missing"));
        };
        let iat =
            i64::try_from(iat).map_err(|_| web::error::ErrorUnauthorized("invalid token iat"))?;
        let revoked_after = self.cached_revoked_after_for(client_id, sub).await?;
        if family_revoked_at(revoked_after, iat) {
            return Err(unauthorized_json("token_revoked"));
        }
        Ok(())
    }

    async fn oauth_guard_from_bearer(
        &self,
        token: &str,
        request_ip: Option<IpAddr>,
        request_id: String,
    ) -> Result<VerifiedPrincipal, HttpRejection> {
        let verified = self
            .auth_provider
            .verify_token(token)
            .await
            .map_err(|err| match err {
                VerifyTokenError::InactiveToken => unauthorized_json("inactive_token"),
                VerifyTokenError::MissingSubject => {
                    web::error::ErrorUnauthorized("missing oauth sub").into()
                }
                VerifyTokenError::MissingIssuer | VerifyTokenError::UnknownIssuer(_) => {
                    web::error::ErrorUnauthorized("unknown oauth issuer").into()
                }
                VerifyTokenError::PlatformVerification(err) => {
                    tracing::warn!(error = %err, "control: platform token verify failed");
                    web::error::ErrorUnauthorized("platform token verification failed").into()
                }
            })?;
        let (principal_id, token_policy) = match &verified.provider_authz {
            ProviderAuthz::OAuthScope(raw_scope) => {
                if !verified.aud.as_ref().is_some_and(|audiences| {
                    audiences
                        .iter()
                        .any(|audience| audience == &self.expected_oauth_audience)
                }) {
                    return Err(unauthorized_json("wrong_audience"));
                }
                self.reject_revoked_platform_token(
                    verified.client_id.as_deref(),
                    &verified.provider_subject,
                    verified.iat,
                )
                .await?;

                let principal_id = Uuid::parse_str(&verified.provider_subject)
                    .map_err(|_| web::error::ErrorUnauthorized("invalid oauth sub"))?;
                let token_policy = policy_from_scope_string(raw_scope, "invalid oauth scope")?;
                (principal_id, token_policy)
            }
            ProviderAuthz::GoTrueRole(role) => {
                if role != "authenticated" {
                    return Err(web::error::ErrorUnauthorized("unauthenticated gotrue role").into());
                }

                let principal_id =
                    self.resolve_supabase_principal(&verified.provider_subject).await?;
                let grants = self.load_principal_grants(principal_id).await?;
                let raw_scope = grants.join(" ");
                let token_policy = policy_from_scope_string(&raw_scope, "invalid principal grant")?;
                (principal_id, token_policy)
            }
        };

        Ok(VerifiedPrincipal {
            principal_id,
            token_id: None,
            token_policy: Some(token_policy),
            mfa_verified: false,
            mfa_age_seconds: None,
            request_ip,
            request_id,
        })
    }

    async fn resolve_supabase_principal(
        &self,
        provider_subject: &str,
    ) -> Result<Uuid, HttpRejection> {
        let rows = self
            .control_pg
            .query(
                "SELECT principal_id \
                 FROM zeroship.identity_links \
                 WHERE provider = 'supabase' AND provider_subject = $1",
                &[&provider_subject],
            )
            .await
            .map_err(|err| {
                tracing::error!(
                    error = %err,
                    "control: supabase identity link lookup failed"
                );
                web::error::ErrorInternalServerError("identity link lookup failed")
            })?;
        let row = rows
            .first()
            .ok_or_else(|| web::error::ErrorUnauthorized("unlinked supabase principal"))?;
        Ok(row.get("principal_id"))
    }

    async fn load_principal_grants(&self, principal_id: Uuid) -> Result<Vec<String>, HttpRejection> {
        let rows = self
            .control_pg
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
                    "control: principal grant lookup failed"
                );
                web::error::ErrorInternalServerError("principal grant lookup failed")
            })?;
        Ok(rows
            .iter()
            .map(|row| row.get::<_, String>("grant_name"))
            .collect())
    }
}

#[cfg(unix)]
fn reject_insecure_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;

    let mode = path
        .metadata()
        .map_err(|err| format!("stat PAT signing key {}: {err}", path.display()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(format!(
            "PAT signing key {} has insecure mode {mode:o}; group/world permissions must be zero",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn reject_insecure_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn policy_from_scope_string(
    raw_scope: &str,
    error: &'static str,
) -> Result<authz::Policy, web::Error> {
    let scopes =
        authz::parse_scope_string(raw_scope).map_err(|_| web::error::ErrorUnauthorized(error))?;
    Ok(authz::scopes_to_policy(&scopes))
}

fn unauthorized_json(error: &'static str) -> web::Error {
    web::error::InternalError::from_response(
        error,
        HttpResponse::Unauthorized().json(&json!({ "error": error })),
    )
    .into()
}

fn now_unix() -> Result<i64, String> {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|err| format!("clock: {err}"))?
            .as_secs(),
    )
    .map_err(|err| format!("clock overflow: {err}"))
}

fn jwk_thumbprint(key: &ed25519_dalek::SigningKey) -> String {
    use sha2::{Digest, Sha256};

    let x = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(key.verifying_key().to_bytes());
    let canonical = format!(r#"{{"crv":"Ed25519","kty":"OKP","x":"{x}"}}"#);
    let digest = Sha256::digest(canonical.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

const CONTROL_REVOCATION_CACHE_TTL_SECS: u64 = 10;

static CONTROL_REVOCATION_CACHE: OnceLock<RevocationCache> = OnceLock::new();

fn control_revocation_cache() -> &'static RevocationCache {
    CONTROL_REVOCATION_CACHE.get_or_init(|| {
        RevocationCache::with_ttl_and_capacity(
            CONTROL_REVOCATION_CACHE_TTL_SECS,
            REVOCATION_CACHE_MAX_ENTRIES,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_insecure_generates_fresh_key_per_issuer() {
        let first = PatIssuer::dev_insecure();
        let second = PatIssuer::dev_insecure();

        assert_ne!(
            first.kid, second.kid,
            "dev_insecure PAT issuers must not share one constant signing key"
        );
    }
}
