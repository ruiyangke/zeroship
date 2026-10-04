//! Control's assertion of a deployment's workflow declarations, for a host that
//! holds no creator artifacts.
//!
//! What a journal's `deploys` row stores is not the artifact: it is a manifest
//! summary of workflow names and schedule declarations. Control parses the
//! bundle when it publishes, so it already holds that value, and asserting it
//! over one authenticated endpoint costs the asking process no blob-store client
//! and no artifact read. It is the same shape `POST /v1/app-facts` took for the
//! policy inputs, and the authority it asks for is strictly less: a manifest
//! listing rather than the policy the same service already takes from Control.
//!
//! A host that holds the artifacts derives the same value from the bytes and
//! needs none of this. Which arm a host takes is decided by what it holds, in
//! `AppDeployments`.

use crate::{service::DeployRegistration, WorkflowServiceError};
use std::sync::Arc;
use zeroship_core::{
    app_id::AppId,
    service_identity::endpoints,
    service_peers::{service_issuer, ServiceAuth, CONTROL_SERVICE_NAME, WORKFLOW_SERVICE_NAME},
    workflow_coordination::FailureCode,
    workflow_jobs::DeploymentId,
};
use zeroship_workflow_client::{self as coordination, Options, Transport};

/// Where a host that holds no creator artifacts gets a deployment's workflow
/// declarations.
///
/// One method, and it answers about one deployment of one app. The
/// implementation carries whatever credential the answer is asked under; this
/// interface carries none, so an engine operation holding it cannot widen what
/// it may ask about.
#[async_trait::async_trait(?Send)]
pub trait DeployRegistrationSource {
    /// The declarations Control records for `deployment` of `app`.
    ///
    /// # Errors
    /// Refuses an unknown deployment, an answer that is not about the
    /// deployment asked for, and failed exchanges.
    async fn registration(
        &self,
        app: &AppId,
        deployment: &DeploymentId,
    ) -> Result<DeployRegistration, WorkflowServiceError>;
}

/// The asserted registration client of the service that holds the journals.
///
/// The signer must be the workflow role itself, as on the journal-scoped hold
/// pair: an instance credential is refused, because the journal whose row this
/// value lands in belongs to the service rather than to any one replica.
#[derive(Clone, Debug)]
pub struct RemoteDeployRegistrations {
    transport: Transport,
}

impl RemoteDeployRegistrations {
    /// Bind Control's origin under the workflow service's own role.
    ///
    /// # Errors
    /// Rejects missing signers, any signer that is not the workflow role, and
    /// invalid origins or exchange bounds.
    pub fn asserted(
        url: &str,
        auth: Arc<ServiceAuth>,
        options: Options,
    ) -> Result<Self, WorkflowServiceError> {
        let (issuer, _) = auth
            .signing_identity()
            .ok_or(WorkflowServiceError::Unauthenticated)?;
        let role = service_issuer(WORKFLOW_SERVICE_NAME).map_err(|_| unavailable())?;
        if issuer != &role {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        let audience = service_issuer(CONTROL_SERVICE_NAME).map_err(|_| unavailable())?;
        Ok(Self {
            transport: Transport::new(url, auth, audience, options).map_err(transport_error)?,
        })
    }
}

#[async_trait::async_trait(?Send)]
impl DeployRegistrationSource for RemoteDeployRegistrations {
    async fn registration(
        &self,
        app: &AppId,
        deployment: &DeploymentId,
    ) -> Result<DeployRegistration, WorkflowServiceError> {
        let request = crate::service::DeployRegistrationRequest {
            app_id: app.clone(),
            deploy_id: deployment.clone(),
        };
        let registration: DeployRegistration = self
            .transport
            .post(endpoints::CONTROL_DEPLOY_REGISTRATION, &request)
            .await
            .map_err(transport_error)?;
        // An answer about another deployment is refused rather than returned,
        // the same way a hold receipt is checked against the request that asked
        // for it. The HASH is not checked here: this client does not know which
        // hash the caller holds, and the caller compares it against its own
        // hold before recording anything.
        if registration.id != deployment.as_str()
            || !zeroship_bundle::validate_hash_format(&registration.hash)
        {
            return Err(WorkflowServiceError::Conflict(
                "asserted deployment registration names another deployment".into(),
            ));
        }
        Ok(registration)
    }
}

fn unavailable() -> WorkflowServiceError {
    WorkflowServiceError::Unavailable("app deployment registration service unavailable".into())
}

fn transport_error(error: coordination::Error) -> WorkflowServiceError {
    match error {
        coordination::Error::InvalidConfig => WorkflowServiceError::InvalidRequest(
            "invalid deployment registration client configuration".into(),
        ),
        coordination::Error::Unauthenticated
        | coordination::Error::Refused(FailureCode::Unauthenticated) => {
            WorkflowServiceError::Unauthenticated
        }
        coordination::Error::RequestTooLarge
        | coordination::Error::Refused(FailureCode::RequestTooLarge) => {
            WorkflowServiceError::PayloadTooLarge
        }
        coordination::Error::Refused(FailureCode::Invalid) => WorkflowServiceError::InvalidRequest(
            "invalid deployment registration request".into(),
        ),
        coordination::Error::Refused(FailureCode::Denied) => WorkflowServiceError::PermissionDenied,
        coordination::Error::Refused(FailureCode::Conflict) => WorkflowServiceError::Conflict(
            "deployment registration request conflicts".into(),
        ),
        coordination::Error::Refused(FailureCode::Capacity) => {
            WorkflowServiceError::ResourceExhausted(
                "deployment registration service capacity exhausted".into(),
            )
        }
        coordination::Error::Timeout => WorkflowServiceError::Timeout,
        _ => unavailable(),
    }
}
