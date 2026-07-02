use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use compio_postgres::Client;
use ntex::http::StatusCode;
use uuid::Uuid;
use zeroship_authz::{self as authz, Action, AuthzContext, AuthzDecision, Resource, Scope};
use zeroship_authn::BearerVerifier;

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
    bearer_verifier: BearerVerifier,
}

impl ControlPlaneAuthenticator {
    #[must_use]
    pub fn new(
        control_pg: Arc<Client>,
        static_policies: authz::PolicySet,
        bearer_verifier: BearerVerifier,
    ) -> Self {
        Self {
            control_pg,
            static_policies,
            bearer_verifier,
        }
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
        let ctx = AuthzContext {
            principal_id: seed.principal_id,
            token_id: seed.token_id,
            token_policy: seed.token_policy,
            action: required_action,
            resource,
            now,
            request_ip: None,
            mfa_verified: seed.mfa_verified,
            mfa_age_seconds: seed.mfa_age_seconds,
            request_id: Some(seed.request_id.as_str()),
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
    async fn verify_action(
        &self,
        token: &str,
        app_id: Uuid,
        required_action: Action,
    ) -> Result<VerifiedCaller, AuthError> {
        let verified = self
            .bearer_verifier
            .verify_bearer(token, None, Uuid::new_v4().to_string())
            .await
            .map_err(map_bearer_error)?;
        let seed = VerifiedSeed {
            principal_id: verified.principal_id,
            token_id: verified.token_id,
            token_policy: verified.token_policy,
            request_id: verified.request_id,
            mfa_verified: verified.mfa_verified,
            mfa_age_seconds: verified.mfa_age_seconds,
        };
        self.authorize(seed, app_id, required_action).await
    }
}

#[derive(Debug)]
struct VerifiedSeed {
    principal_id: Uuid,
    token_id: Option<Uuid>,
    token_policy: Option<authz::Policy>,
    request_id: String,
    mfa_verified: bool,
    mfa_age_seconds: Option<u32>,
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

fn map_bearer_error(err: ntex::web::Error) -> AuthError {
    let status = err.as_response_error().status_code();
    if status == StatusCode::UNAUTHORIZED
        || err.to_string().contains("platform token verification failed")
    {
        AuthError::Unauthorized
    } else {
        AuthError::Infrastructure(err.to_string())
    }
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
