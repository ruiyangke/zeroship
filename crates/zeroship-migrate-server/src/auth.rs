use std::net::IpAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use compio_postgres::Client;
use ntex::http::StatusCode;
use zeroship_authn::BearerVerifier;
use zeroship_authz::{self as authz, Action, AuthzContext, AuthzDecision, Resource, Scope};
use zeroship_id::{DatabaseId, UserId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedCaller {
    pub principal_id: UserId,
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

/// Whether a bearer may perform an action against ONE DATABASE.
///
/// The subject is the database because applying schema is not an app
/// operation: it is authorized by a qualifying seat on the project that owns
/// the database, which is the same authority that created it. A trait taking
/// an app id could not express that question, and an implementation handed one
/// could only answer a different one.
#[async_trait(?Send)]
pub trait Authenticator: Send + Sync {
    /// `request_id` is the caller-visible correlation id for this HTTP
    /// request. It is stamped onto the authz audit row, so it must be the id
    /// the rest of the platform knows this request by - not one minted here,
    /// which would correlate with nothing.
    async fn verify_action(
        &self,
        token: &str,
        database_id: &DatabaseId,
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
        database_id: &DatabaseId,
        required_scope: Scope,
        request_id: &str,
    ) -> Result<VerifiedCaller, AuthError> {
        self.verify_action(
            token,
            database_id,
            required_scope.action(),
            None,
            request_id,
        )
        .await
    }

    /// The whole fence, and there is exactly one.
    ///
    /// [`Resource::Database`] makes `zeroship_authz::authority::resolve` reach
    /// `zeroship.databases.project_id` directly and narrow the caller's
    /// organization seat by their project seat - the same
    /// `effective_project_rank` the control plane compares when it creates the
    /// database in the first place. The band that admits the action lives in
    /// `deploy/policies/creator/organization_develop.cedar`, so where the seat
    /// ladder puts `database:migrate` is a policy fact this service reads
    /// rather than a rank it names.
    ///
    /// There is no second, service-local rank comparison beside it. One would
    /// be a second spelling of an answer `zeroship-authz` already owns, and the
    /// two would drift the moment the ladder moved.
    async fn authorize(
        &self,
        seed: VerifiedSeed,
        database_id: &DatabaseId,
        required_action: Action,
    ) -> Result<VerifiedCaller, AuthError> {
        let resource = Resource::Database {
            id: database_id.clone(),
        };
        resource
            .validate_ids()
            .map_err(|message| AuthError::Infrastructure(message.to_owned()))?;

        let now = now_unix().map_err(AuthError::Infrastructure)?;
        let ctx = AuthzContext {
            principal_id: seed.principal_id.clone(),
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
        database_id: &DatabaseId,
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
                &verified.principal_id,
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
        self.authorize(seed, database_id, required_action).await
    }
}

#[derive(Debug)]
struct VerifiedSeed {
    principal_id: UserId,
    token_policy: Option<authz::Policy>,
    request_id: String,
    request_ip: Option<IpAddr>,
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
