//! Shared bearer authentication for control-like services.

pub mod service_replay;

use std::collections::HashSet;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;
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
use zeroship_core::device_grant::{PLATFORM_CLI_CLIENT_ID, PLATFORM_CLI_ISSUABLE_SCOPES};
use zeroship_authz::wrapper_revocation::{family_revoked_at, revoked_after_for};

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

    /// Creates an issuer backed by a fresh, process-local signing key.
    ///
    /// This is suitable for ephemeral fixtures. Long-lived services should use
    /// [`Self::new`] with operator-managed signing material so issued tokens
    /// remain valid across restarts.
    #[must_use]
    pub fn generate_ephemeral() -> Self {
        let key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        Self::new(&key).expect("generated ephemeral PAT key is valid")
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
    /// This bearer is a platform CLI token for a principal with no
    /// `zeroship.identity_links` seeding marker, so it was authorized against
    /// the DEFAULT CLI grant set rather than against stored rows
    /// (see [`BearerVerifier::platform_cli_entitlement`]).
    ///
    /// A caller that can write `zeroship.principal_grants` should materialize
    /// those defaults, because until it does the operator has no rows to
    /// delete and narrowing has nothing to bite on. A caller that cannot write
    /// them ignores this: the request itself was already authorized correctly,
    /// and control will materialize on its own first sight of the principal.
    pub seed_platform_cli_grants: bool,
}

/// What a platform CLI token may actually do, resolved from the live grant
/// rows rather than from the token.
struct PlatformCliEntitlement {
    scopes: HashSet<authz::Scope>,
    /// No `zeroship.identity_links` row at all. Deliberately NOT "holds zero
    /// grants": "the operator revoked everything" is a legitimate empty
    /// entitlement and must not be re-seeded.
    unseeded: bool,
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
        let principal = match self.pat_issuer.verify(token) {
            Err(err) => {
                tracing::debug!(
                    error = %err,
                    "control: bearer was not a valid PAT; trying OAuth introspection"
                );
                self
                    .oauth_guard_from_bearer(token, request_ip, request_id)
                    .await?
            }
            Ok(claims) => {
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

                VerifiedPrincipal {
                    principal_id,
                    token_id: Some(token_id),
                    token_policy: None,
                    mfa_verified: false,
                    mfa_age_seconds: None,
                    request_ip,
                    request_id,
                    // A PAT is not a CLI device-grant token; its authority
                    // comes from the wrapper policy, not from the CLI grant
                    // set, so it never triggers CLI grant materialization.
                    seed_platform_cli_grants: false,
                }
            }
        };

        self.require_active_principal(principal.principal_id).await?;

        if let Some(token_id) = principal.token_id {
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
        }

        Ok(principal)
    }

    async fn platform_revoked_after_for(
        &self,
        client_id: &str,
        sub: &str,
    ) -> Result<Option<i64>, HttpRejection> {
        // Control already performs a user-row lookup for every bearer. Read the
        // family marker in the same request rather than caching it: deletion
        // cancellation must never revive an older platform token through a
        // stale negative or earlier-positive cache entry.
        revoked_after_for(&self.control_pg, client_id, sub)
            .await
            .map_err(|err| {
                tracing::error!(
                    error = %err,
                    client_id,
                    sub,
                    "control: token revocation lookup failed"
                );
                unauthorized_json("revocation_check_failed")
            })
    }

    /// Refuse a platform bearer whose token family has been revoked.
    ///
    /// Public so that every path accepting a platform OAuth bearer runs THIS
    /// check rather than a second copy of it. Control's device-approval handler
    /// is the other caller; it used to accept `ProviderAuthz::OAuthScope`
    /// unconditionally, which let a revoked bearer approve a fresh device grant
    /// and mint a token with a new `iat` that outran the marker.
    ///
    /// Missing `client_id` or `iat` is a REFUSAL, not a pass: without them
    /// there is nothing to compare a marker against.
    ///
    /// # Errors
    ///
    /// A 401 rejection when the family is revoked, when the claims needed to
    /// decide are absent, or when the marker lookup itself fails (fail closed).
    pub async fn reject_revoked_platform_token(
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
        let revoked_after = self.platform_revoked_after_for(client_id, sub).await?;
        if family_revoked_at(revoked_after, iat) {
            return Err(unauthorized_json("token_revoked"));
        }
        Ok(())
    }

    /// Require a principal row that is still eligible to authenticate.
    ///
    /// This is public for the non-bearer device paths. Every bearer accepted by
    /// [`Self::verify_bearer`] passes this check at one shared convergence point,
    /// so a future bearer class cannot omit owner lifecycle validation.
    ///
    /// # Errors
    ///
    /// A 401 rejection when the principal is missing, disabled, pending
    /// deletion, or anonymized.
    /// A lookup failure is a 500 rejection and never authenticates the caller.
    pub async fn require_active_principal(
        &self,
        principal_id: Uuid,
    ) -> Result<(), HttpRejection> {
        let rows = self
            .control_pg
            .query(ACTIVE_PRINCIPAL_SQL, &[&principal_id])
            .await
            .map_err(|err| {
                tracing::error!(
                    error = %err,
                    principal_id = %principal_id,
                    "control: principal eligibility lookup failed"
                );
                web::error::ErrorInternalServerError("principal eligibility lookup failed")
            })?;
        if rows.is_empty() {
            return Err(unauthorized_json("principal_inactive"));
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
        let (principal_id, token_policy, seed_platform_cli_grants) = match &verified.provider_authz
        {
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
                let mut scopes = authz::parse_scope_string(raw_scope)
                    .map_err(|_| web::error::ErrorUnauthorized("invalid oauth scope"))?;
                let mut seed = false;
                if verified.client_id.as_deref() == Some(PLATFORM_CLI_CLIENT_ID) {
                    let entitlement = self.platform_cli_entitlement(principal_id).await?;
                    seed = entitlement.unseeded;
                    scopes.retain(|scope| entitlement.scopes.contains(scope));
                }
                (principal_id, authz::scopes_to_policy(&scopes), seed)
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
                (principal_id, token_policy, false)
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
            seed_platform_cli_grants,
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

    /// Resolve the live entitlement a platform CLI token is capped to.
    ///
    /// The OP caps a device-grant token to the client REGISTRATION and cannot
    /// do more: `db/migrations-ts/20260702000900_grants.ts:55` gives
    /// `zeroship_auth` SELECT on `zeroship.principal_grants` and no write
    /// anywhere near it. So the token's `scope` claim is a coarse ceiling, and
    /// this is where an operator's narrowing takes effect - per request, which
    /// means a DELETE reaches the token already in the creator's hand and not
    /// merely the next login.
    ///
    /// This is a pure read and needs no privilege beyond what
    /// `zeroship_control` already holds on both tables (`grants.ts:65`).
    ///
    /// An UNSEEDED principal falls back to the default CLI set rather than to
    /// nothing. `zeroship login` is an OP-only conversation, so control's
    /// first sight of a platform-native creator IS this request; intersecting
    /// with the empty table would 403 every creator's first command. The
    /// fallback is not a second source of truth - it is exactly the set
    /// `identity_bridge::ensure_platform_creator_grants` materializes, which
    /// the caller triggers on [`PlatformCliEntitlement::unseeded`].
    async fn platform_cli_entitlement(
        &self,
        principal_id: Uuid,
    ) -> Result<PlatformCliEntitlement, HttpRejection> {
        let row = self
            .control_pg
            .query_one(
                "SELECT EXISTS ( \
                     SELECT 1 FROM zeroship.identity_links WHERE principal_id = $1 \
                 ) AS seeded, \
                 ( \
                     SELECT string_agg(grant_name, ' ' ORDER BY grant_name) \
                     FROM zeroship.principal_grants WHERE principal_id = $1 \
                 ) AS granted",
                &[&principal_id],
            )
            .await
            .map_err(|err| {
                tracing::error!(
                    error = %err,
                    "control: platform CLI entitlement lookup failed"
                );
                web::error::ErrorInternalServerError("principal grant lookup failed")
            })?;

        if !row.get::<_, bool>("seeded") {
            return Ok(PlatformCliEntitlement {
                scopes: PLATFORM_CLI_ISSUABLE_SCOPES
                    .iter()
                    .filter_map(|grant| authz::Scope::parse(grant).ok())
                    .collect(),
                unseeded: true,
            });
        }

        // An unparseable grant name grants nothing rather than rejecting the
        // request. The rows are operator-written free text with no CHECK
        // constraint, so a typo must cost the creator that one scope, not
        // every scope they legitimately hold.
        let scopes = row
            .get::<_, Option<String>>("granted")
            .unwrap_or_default()
            .split_whitespace()
            .filter_map(|grant| match authz::Scope::parse(grant) {
                Ok(scope) => Some(scope),
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        principal_id = %principal_id,
                        "control: ignoring unknown principal grant"
                    );
                    None
                }
            })
            .collect();
        Ok(PlatformCliEntitlement {
            scopes,
            unseeded: false,
        })
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

const ACTIVE_PRINCIPAL_SQL: &str =
    "SELECT 1 FROM zeroship.users \
     WHERE id = $1 \
       AND disabled_at IS NULL \
       AND deletion_requested_at IS NULL \
       AND deletion_scheduled_for IS NULL \
       AND anonymized_at IS NULL";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_ephemeral_uses_fresh_key_per_issuer() {
        let first = PatIssuer::generate_ephemeral();
        let second = PatIssuer::generate_ephemeral();

        assert_ne!(
            first.kid, second.kid,
            "ephemeral PAT issuers must not share one constant signing key"
        );
    }

    #[test]
    fn active_principal_query_blocks_every_hard_lifecycle_state() {
        for column in [
            "disabled_at",
            "anonymized_at",
            "deletion_requested_at",
            "deletion_scheduled_for",
        ] {
            assert!(
                ACTIVE_PRINCIPAL_SQL.contains(&format!("{column} IS NULL")),
                "missing active lifecycle predicate for {column}"
            );
        }
        assert!(!ACTIVE_PRINCIPAL_SQL.contains("locked_until"));
    }
}
