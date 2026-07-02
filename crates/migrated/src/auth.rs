use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use compio_postgres::Client;
use uuid::Uuid;
use zeroship_authz::{self as authz, Action, AuthzContext, AuthzDecision, Resource, Scope};
use zeroship_control::token_handlers::PatIssuer;
use zeroship_core::hydra::HydraIntrospector;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedCaller {
    pub principal_id: Uuid,
    pub token_id: Option<Uuid>,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("unauthenticated")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("auth infrastructure error: {0}")]
    Infrastructure(String),
}

#[async_trait(?Send)]
pub trait Authenticator: Send + Sync {
    async fn verify_action(
        &self,
        token: &str,
        app_id: Uuid,
        required_action: Action,
    ) -> Result<VerifiedCaller, AuthError>;
}

#[derive(Debug)]
pub struct ControlPlaneAuthenticator {
    control_pg: Arc<Client>,
    static_policies: authz::PolicySet,
    pat_issuer: Arc<PatIssuer>,
    hydra_introspector: Arc<HydraIntrospector>,
    expected_oauth_audience: String,
}

impl ControlPlaneAuthenticator {
    #[must_use]
    pub fn new(
        control_pg: Arc<Client>,
        static_policies: authz::PolicySet,
        pat_issuer: Arc<PatIssuer>,
        hydra_introspector: Arc<HydraIntrospector>,
        expected_oauth_audience: String,
    ) -> Self {
        Self {
            control_pg,
            static_policies,
            pat_issuer,
            hydra_introspector,
            expected_oauth_audience,
        }
    }

    async fn verify_pat(&self, token: &str) -> Result<Option<VerifiedSeed>, AuthError> {
        let claims = match self.pat_issuer.verify(token) {
            Ok(claims) => claims,
            Err(err) => {
                tracing::debug!(
                    error = %err,
                    "migrated: bearer was not a valid PAT; trying OAuth introspection"
                );
                return Ok(None);
            }
        };

        let token_id = Uuid::parse_str(&claims.jti).map_err(|_| AuthError::Unauthorized)?;
        let owner_id = Uuid::parse_str(&claims.owner).map_err(|_| AuthError::Unauthorized)?;
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
                AuthError::Infrastructure(format!("permission token lookup failed: {err}"))
            })?;
        let row = rows.first().ok_or(AuthError::Unauthorized)?;
        let principal_id: Uuid = row.get("owner_id");

        if let Err(err) = self
            .control_pg
            .execute(
                "UPDATE zeroship.permission_tokens SET last_used_at = NOW() WHERE id = $1",
                &[&token_id],
            )
            .await
        {
            tracing::warn!(
                error = %err,
                "migrated: permission token last_used_at update failed"
            );
        }

        Ok(Some(VerifiedSeed {
            principal_id,
            token_id: Some(token_id),
            token_policy: None,
        }))
    }

    async fn verify_oauth(&self, token: &str) -> Result<VerifiedSeed, AuthError> {
        let result = self
            .hydra_introspector
            .introspect(token)
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "migrated: hydra introspect failed");
                AuthError::Unauthorized
            })?;
        if !result.active {
            return Err(AuthError::Unauthorized);
        }
        if !result.aud.as_ref().is_some_and(|audiences| {
            audiences
                .iter()
                .any(|audience| audience == &self.expected_oauth_audience)
        }) {
            return Err(AuthError::Unauthorized);
        }

        let sub = result.sub.ok_or(AuthError::Unauthorized)?;
        let principal_id = Uuid::parse_str(&sub).map_err(|_| AuthError::Unauthorized)?;
        let raw_scope = result.scope.unwrap_or_default();
        let scopes =
            parse_resource_server_scopes(&raw_scope).map_err(|_| AuthError::Unauthorized)?;
        let token_policy = authz::scopes_to_policy(&scopes);

        Ok(VerifiedSeed {
            principal_id,
            token_id: None,
            token_policy: Some(token_policy),
        })
    }

    pub async fn verify_bearer(
        &self,
        token: &str,
        app_id: Uuid,
        required_scope: Scope,
    ) -> Result<VerifiedCaller, AuthError> {
        self.verify_action(token, app_id, required_scope.action())
            .await
    }

    async fn authorize(
        &self,
        seed: VerifiedSeed,
        app_id: Uuid,
        required_action: Action,
    ) -> Result<VerifiedCaller, AuthError> {
        let resource = Resource::App {
            id: app_id.to_string(),
        };
        resource
            .validate_ids()
            .map_err(|message| AuthError::Infrastructure(message.to_owned()))?;

        let now = now_unix().map_err(AuthError::Infrastructure)?;
        let request_id = Uuid::new_v4().to_string();
        let ctx = AuthzContext {
            principal_id: seed.principal_id,
            token_id: seed.token_id,
            token_policy: seed.token_policy,
            action: required_action,
            resource,
            now,
            request_ip: None,
            mfa_verified: false,
            mfa_age_seconds: None,
            request_id: Some(request_id.as_str()),
        };

        match authz::enforce(&self.control_pg, &self.static_policies, &ctx).await {
            Ok(AuthzDecision::Allow) => {}
            Ok(AuthzDecision::Deny) => return Err(AuthError::Forbidden),
            Err(err) => return Err(AuthError::Infrastructure(err.to_string())),
        }

        if requires_app_owner(required_action)
            && !caller_owns_app(&self.control_pg, seed.principal_id, app_id).await?
        {
            return Err(AuthError::Forbidden);
        }

        Ok(VerifiedCaller {
            principal_id: seed.principal_id,
            token_id: seed.token_id,
        })
    }
}

#[async_trait(?Send)]
impl Authenticator for ControlPlaneAuthenticator {
    // TODO(convergence): replace with zeroship_authn::BearerVerifier once auth-providers lands on main (A2).
    async fn verify_action(
        &self,
        token: &str,
        app_id: Uuid,
        required_action: Action,
    ) -> Result<VerifiedCaller, AuthError> {
        let seed = match self.verify_pat(token).await? {
            Some(seed) => seed,
            None => self.verify_oauth(token).await?,
        };
        self.authorize(seed, app_id, required_action).await
    }
}

#[derive(Debug)]
struct VerifiedSeed {
    principal_id: Uuid,
    token_id: Option<Uuid>,
    token_policy: Option<authz::Policy>,
}

async fn caller_owns_app(pg: &Client, principal_id: Uuid, app_id: Uuid) -> Result<bool, AuthError> {
    let rows = pg
        .query(
            "SELECT 1 FROM zeroship.app_members \
             WHERE app_id = $1 AND user_id = $2 AND role = 'owner'",
            &[&app_id, &principal_id],
        )
        .await
        .map_err(|err| AuthError::Infrastructure(format!("app ownership lookup failed: {err}")))?;
    Ok(!rows.is_empty())
}

fn requires_app_owner(action: Action) -> bool {
    !matches!(action, Action::AppsApproveMigration)
}

fn parse_resource_server_scopes(raw: &str) -> Result<Vec<Scope>, authz::ParseScopeError> {
    raw.split_whitespace()
        .filter(|scope| !is_standard_oidc_scope(scope))
        .map(Scope::parse)
        .collect()
}

fn is_standard_oidc_scope(scope: &str) -> bool {
    matches!(
        scope,
        "openid" | "offline_access" | "profile" | "email" | "address" | "phone"
    )
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
