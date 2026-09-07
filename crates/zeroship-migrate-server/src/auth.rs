use std::net::IpAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use compio_postgres::Client;
use ntex::http::StatusCode;
use uuid::Uuid;
use zeroship_authn::BearerVerifier;
use zeroship_authz::{self as authz, Action, AuthzContext, AuthzDecision, Resource, Scope};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedCaller {
    pub principal_id: Uuid,
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
    /// `request_id` is the caller-visible correlation id for this HTTP
    /// request. It is stamped onto the authz audit row, so it must be the id
    /// the rest of the platform knows this request by - not one minted here,
    /// which would correlate with nothing.
    async fn verify_action(
        &self,
        token: &str,
        app_id: Uuid,
        required_action: Action,
        request_ip: Option<IpAddr>,
        request_id: &str,
    ) -> Result<VerifiedCaller, AuthError>;
}

#[derive(Debug)]
pub struct ControlPlaneAuthenticator {
    control_pg: Arc<Client>,
    static_policies: authz::PlatformPolicies,
    bearer_verifier: BearerVerifier,
}

impl ControlPlaneAuthenticator {
    #[must_use]
    pub fn new(
        control_pg: Arc<Client>,
        static_policies: authz::PlatformPolicies,
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
        request_id: &str,
    ) -> Result<VerifiedCaller, AuthError> {
        self.verify_action(token, app_id, required_scope.action(), None, request_id)
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
            token_policy: seed.token_policy,
            action: required_action,
            resource,
            now,
            request_ip: seed.request_ip,
            request_id: Some(seed.request_id.as_str()),
        };

        match authz::enforce(&self.control_pg, &self.static_policies, &ctx).await {
            Ok(AuthzDecision::Allow) => {}
            Ok(AuthzDecision::Deny) => return Err(AuthError::Forbidden),
            Err(err) => return Err(AuthError::Infrastructure(err.to_string())),
        }

        if requires_organization_owner(required_action)
            && !caller_holds_organization_ownership(
                &self.control_pg,
                seed.principal_id,
                app_id,
            )
            .await?
        {
            return Err(AuthError::Forbidden);
        }

        Ok(VerifiedCaller {
            principal_id: seed.principal_id,
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
        request_ip: Option<IpAddr>,
        request_id: &str,
    ) -> Result<VerifiedCaller, AuthError> {
        let mut verified = self
            .bearer_verifier
            .verify_bearer(token, request_ip, request_id.to_owned())
            .await
            .map_err(map_bearer_error)?;
        if verified.seed_platform_cli_grants {
            let materialization = zeroship_authn::platform_cli::materialize_default_grants(
                self.control_pg.as_ref(),
                verified.principal_id,
            )
            .await
            .map_err(|error| {
                AuthError::Infrastructure(format!(
                    "platform CLI default-grant materialization failed: {error}"
                ))
            })?;
            if materialization.requires_entitlement_refresh() {
                verified = self
                    .bearer_verifier
                    .verify_bearer(token, request_ip, request_id.to_owned())
                    .await
                    .map_err(map_bearer_error)?;
                if verified.seed_platform_cli_grants {
                    return Err(AuthError::Infrastructure(
                        "platform CLI entitlement remained unseeded after a raced materialization"
                            .to_string(),
                    ));
                }
            }
        }
        let seed = VerifiedSeed {
            principal_id: verified.principal_id,
            token_policy: verified.token_policy,
            request_id: verified.request_id,
            request_ip: verified.request_ip,
        };
        self.authorize(seed, app_id, required_action).await
    }
}

#[derive(Debug)]
struct VerifiedSeed {
    principal_id: Uuid,
    token_policy: Option<authz::Policy>,
    request_id: String,
    request_ip: Option<IpAddr>,
}

/// The second fence, beside Cedar: applying a migration needs OWNER authority
/// in the organization that owns the app's project.
///
/// # It reads the ladder rather than the word "owner"
///
/// `zeroship.app_members` had exactly one privileged role and this asked for it
/// by name. Organization authority is two integers on a closed ladder, so the
/// question is now "does the caller's rank reach the owner rank" - which stays
/// true if a migration ever moves `owner` up or down, and which a hardcoded
/// number would not.
///
/// # There is no per-project narrowing here, and that is deliberate
///
/// A migration rewrites the app's schema, which is the least reversible thing
/// the platform lets a creator do. Narrowing would let a project seat reach it;
/// requiring the ORGANIZATION rank means only somebody who answers for the whole
/// organization can. Read this as the ceiling being organization-level on
/// purpose, not as an oversight about `project_members`.
async fn caller_holds_organization_ownership(
    pg: &Client,
    principal_id: Uuid,
    app_id: Uuid,
) -> Result<bool, AuthError> {
    let rows = pg
        .query(
            "SELECT 1 \
               FROM zeroship.apps a \
               JOIN zeroship.projects p ON p.id = a.project_id \
               JOIN zeroship.organization_members m \
                    ON m.organization_id = p.organization_id AND m.user_id = $2 \
               JOIN zeroship.organization_roles r ON r.role = m.role \
              WHERE a.id = $1 \
                AND r.rank >= (SELECT rank FROM zeroship.organization_roles WHERE role = 'owner')",
            &[&app_id, &principal_id],
        )
        .await
        .map_err(|err| {
            AuthError::Infrastructure(format!("organization ownership lookup failed: {err}"))
        })?;
    Ok(!rows.is_empty())
}

fn requires_organization_owner(action: Action) -> bool {
    !matches!(action, Action::AppsApproveMigration)
}

/// Classify a rejection from [`BearerVerifier`] by the status it reports.
///
/// This used to also match the message `"platform token verification failed"`.
/// That was a patch on one symptom of a construction bug in `zeroship-authn`:
/// ntex's `InternalError` discarded the 401 it was built with, so EVERY authn
/// rejection arrived here as a 500 and the one message someone happened to hit
/// got special-cased. `zeroship_authn::AuthnRejection` now reports the status
/// it was constructed with, so the status alone is sufficient and matching on
/// prose is not.
fn map_bearer_error(err: ntex::web::Error) -> AuthError {
    if err.as_response_error().status_code() == StatusCode::UNAUTHORIZED {
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

#[cfg(test)]
mod tests {
    use super::*;
    use zeroship_authn::AuthnRejection;

    /// A malformed bearer is a REJECTED CREDENTIAL, and the classifier must say
    /// so from the status alone.
    ///
    /// `"not-a-jwt"` carries no `iss`, so `AuthProvider::verify_token` returns
    /// `VerifyTokenError::MissingIssuer` and `zeroship-authn` answers the
    /// `unknown_oauth_issuer` rejection built here. Before the fix that
    /// rejection read back as 500 and this arrived as `Infrastructure`, telling
    /// a client with a permanently bad token to retry forever.
    ///
    /// This pins the classifier against a rejection carrying a 401. It does NOT
    /// exercise `AuthProvider`, so it cannot catch authn choosing the wrong
    /// rejection for `MissingIssuer`; the live-database
    /// `real_delegating_authenticator_rejects_malformed_bearer` walks that path.
    #[test]
    fn malformed_bearer_rejection_is_unauthorized() {
        let rejection: ntex::web::Error =
            AuthnRejection::unauthorized("unknown_oauth_issuer").into();
        assert_eq!(
            rejection.as_response_error().status_code(),
            StatusCode::UNAUTHORIZED,
            "authn must construct a malformed bearer as a 401, not a 500"
        );
        assert!(
            matches!(map_bearer_error(rejection), AuthError::Unauthorized),
            "a 401 rejection must classify as Unauthorized"
        );
    }

    /// The counterpart: authn's own 500s still mean the service could not
    /// decide, and must stay `Infrastructure` so a caller retries.
    ///
    /// Paired with the test above, this is what shows the classifier reads the
    /// status rather than passing everything through one arm.
    #[test]
    fn authn_lookup_failure_stays_infrastructure() {
        let rejection: ntex::web::Error =
            AuthnRejection::internal("principal_grant_lookup_failed").into();
        assert!(
            matches!(map_bearer_error(rejection), AuthError::Infrastructure(_)),
            "a 500 rejection must not be reported as a rejected credential"
        );
    }

    /// The deleted special-case matched the message `"platform token
    /// verification failed"`. Its replacement is the status, so the rejection
    /// that message came from must classify correctly under its NEW snake_case
    /// code, which no string match would have caught.
    #[test]
    fn platform_verification_failure_needs_no_message_match() {
        let rejection: ntex::web::Error =
            AuthnRejection::unauthorized("platform_token_verification_failed").into();
        assert!(
            matches!(map_bearer_error(rejection), AuthError::Unauthorized),
            "classification must not depend on the rejection's prose"
        );
    }
}
