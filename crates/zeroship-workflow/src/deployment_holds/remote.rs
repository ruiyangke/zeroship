//! Journal-holder access to Control's normal app deployment holds.
//!
//! Two kinds of host take a journal hold, and they differ only in what
//! authorizes them. A customer worker holds a placement and names its revision,
//! which Control verifies with the coordinator on every call. The workflow
//! service holds the journal itself and has no placement to name; it asserts its
//! own role, which Control admits by name.

#![expect(
    clippy::future_not_send,
    reason = "HTTP exchanges stay on their compio thread"
)]

use super::{DeploymentHoldClient, HoldGeneration, HoldReceipt, HoldRequest, HoldScope, HoldState};
use crate::WorkflowServiceError;
use std::sync::Arc;
use zeroship_core::{
    app_id::AppId,
    service_identity::{endpoints, ServiceEndpoint},
    service_peers::{
        service_issuer, ServiceAuth, CONTROL_SERVICE_NAME, WORKER_SERVICE_NAME,
        WORKFLOW_SERVICE_NAME,
    },
    typed_id,
    workflow_coordination::{AssignedScope, FailureCode, Revision, WorkerId},
};
use zeroship_workflow_client::{self as coordination, Options, Transport};

/// Immutable app-scoped client with no platform database capability.
#[derive(Clone, Debug)]
pub struct RemoteDeploymentHolds {
    scope: HoldScope,
    /// The placement this client names, and `None` for a host whose authority
    /// is its own role. Control refuses either one presented by the other's
    /// principal, so this is the client's half of one comparison.
    revision: Option<Revision>,
    transport: Transport,
}

impl RemoteDeploymentHolds {
    /// Bind retention to an app assignment of the enrolled worker `auth` signs
    /// for. The assignment is named only by its scope: its worker is the
    /// signer's own instance, and no expiry is claimed locally. Control
    /// verifies the actual current placement on every call.
    ///
    /// # Errors
    /// Rejects invalid endpoints and signers that are not an enrolled worker
    /// instance.
    pub fn new(
        url: &str,
        auth: Arc<ServiceAuth>,
        scope: &AssignedScope,
        options: Options,
    ) -> Result<Self, WorkflowServiceError> {
        let (issuer, _) = auth
            .signing_identity()
            .ok_or(WorkflowServiceError::Unauthenticated)?;
        let worker_role = service_issuer(WORKER_SERVICE_NAME).map_err(|_| unavailable())?;
        if issuer.principal() != worker_role.principal()
            || issuer
                .instance()
                .and_then(|id| WorkerId::parse(id).ok())
                .is_none()
        {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        let audience = service_issuer(CONTROL_SERVICE_NAME).map_err(|_| unavailable())?;
        Ok(Self {
            scope: HoldScope::for_app(scope.app_id.clone()),
            revision: Some(scope.assignment_revision),
            transport: Transport::new(url, auth, audience, options).map_err(transport_error)?,
        })
    }

    /// Bind retention to `app` on the strength of the workflow service's own
    /// role, for the host that holds that app's journal.
    ///
    /// There is no placement to name, and the signer must be the role itself:
    /// an instance credential is refused here exactly as it is on the
    /// queue-scoped pair, because the journal a hold protects belongs to the
    /// service rather than to any one of its replicas.
    ///
    /// # Errors
    /// Rejects invalid endpoints, missing signers and any signer that is not
    /// the workflow service's role.
    pub fn asserted(
        url: &str,
        auth: Arc<ServiceAuth>,
        app: AppId,
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
            scope: HoldScope::for_app(app),
            revision: None,
            transport: Transport::new(url, auth, audience, options).map_err(transport_error)?,
        })
    }

    async fn change(
        &self,
        endpoint: ServiceEndpoint,
        deployment: &str,
        generation: HoldGeneration,
        state: HoldState,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        typed_id::parse_with_prefix(deployment, "dep")
            .map_err(|_| WorkflowServiceError::InvalidRequest("invalid app deployment".into()))?;
        let request = HoldRequest {
            app_id: self.scope.app().clone(),
            assignment_revision: self.revision,
            deploy_id: deployment.to_owned(),
            generation,
        };
        let receipt: HoldReceipt = self
            .transport
            .post(endpoint, &request)
            .await
            .map_err(transport_error)?;
        if receipt.app_id != request.app_id
            || receipt.deploy_id != request.deploy_id
            || receipt.holder_id != self.scope.holder()
            || receipt.generation != generation
            || receipt.state != state
            || !zeroship_bundle::validate_hash_format(&receipt.deploy_hash)
        {
            return Err(unavailable());
        }
        Ok(receipt)
    }
}

#[async_trait::async_trait(?Send)]
impl DeploymentHoldClient for RemoteDeploymentHolds {
    fn scope(&self) -> &HoldScope {
        &self.scope
    }

    async fn acquire(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        self.change(
            endpoints::CONTROL_DEPLOYMENT_HOLD_ACQUIRE,
            deployment,
            generation,
            HoldState::Held,
        )
        .await
    }

    async fn release(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        self.change(
            endpoints::CONTROL_DEPLOYMENT_HOLD_RELEASE,
            deployment,
            generation,
            HoldState::Released,
        )
        .await
    }
}

fn unavailable() -> WorkflowServiceError {
    WorkflowServiceError::Unavailable("app deployment hold service unavailable".into())
}
fn transport_error(error: coordination::Error) -> WorkflowServiceError {
    match error {
        coordination::Error::InvalidConfig => WorkflowServiceError::InvalidRequest(
            "invalid deployment hold client configuration".into(),
        ),
        coordination::Error::Unauthenticated
        | coordination::Error::Refused(FailureCode::Unauthenticated) => {
            WorkflowServiceError::Unauthenticated
        }
        coordination::Error::RequestTooLarge
        | coordination::Error::Refused(FailureCode::RequestTooLarge) => {
            WorkflowServiceError::PayloadTooLarge
        }
        coordination::Error::Refused(FailureCode::Invalid) => {
            WorkflowServiceError::InvalidRequest("invalid deployment hold request".into())
        }
        coordination::Error::Refused(FailureCode::Denied) => WorkflowServiceError::PermissionDenied,
        coordination::Error::Refused(FailureCode::Conflict) => {
            WorkflowServiceError::Conflict("deployment hold request conflicts".into())
        }
        coordination::Error::Refused(FailureCode::Capacity) => {
            WorkflowServiceError::ResourceExhausted(
                "deployment hold service capacity exhausted".into(),
            )
        }
        coordination::Error::Timeout => WorkflowServiceError::Timeout,
        _ => unavailable(),
    }
}
