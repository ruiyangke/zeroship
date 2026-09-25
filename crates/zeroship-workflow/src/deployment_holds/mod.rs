//! Customer-side deployment retention clients. Platform catalog storage belongs
//! to the workflow manager and is supplied through the host's scoped client.

use crate::WorkflowServiceError;
use std::rc::Rc;
use zeroship_core::app_id::AppId;
pub use zeroship_core::workflow_deployments::{
    HoldGeneration, HoldReceipt, HoldRequest, HoldScope, HoldState,
};

mod remote;
pub use remote::RemoteDeploymentHolds;

/// Host-authenticated access to deployment metadata for a single app journal.
/// A customer worker receives this client, never the platform database.
#[async_trait::async_trait(?Send)]
pub trait DeploymentHoldClient {
    fn scope(&self) -> &HoldScope;
    async fn acquire(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError>;
    async fn release(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError>;
}

/// The deployment retention authority a host grants its workflow service. The
/// host states which apps it serves here and nowhere else, and the service
/// reaches a hold client only by asking it for one.
pub trait DeploymentHoldAuthority {
    /// The retention client for one app.
    ///
    /// # Errors
    /// Refuses an app this host does not serve.
    fn client(&self, app: &AppId) -> Result<Rc<dyn DeploymentHoldClient>, WorkflowServiceError>;
}

/// The authority of a host that runs one assigned app.
///
/// A customer worker on an app placement holds this, and so does a local host
/// on the app it serves. The client's own scope names that app, so the scoping
/// has one statement and no roster to widen.
pub struct AssignedHolds(Rc<dyn DeploymentHoldClient>);

impl std::fmt::Debug for AssignedHolds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssignedHolds")
            .field("app", self.0.scope().app())
            .finish()
    }
}

impl AssignedHolds {
    /// Serve exactly the app `client` is scoped to.
    #[must_use]
    pub fn new(client: Rc<dyn DeploymentHoldClient>) -> Self {
        Self(client)
    }
}

impl DeploymentHoldAuthority for AssignedHolds {
    fn client(&self, app: &AppId) -> Result<Rc<dyn DeploymentHoldClient>, WorkflowServiceError> {
        if self.0.scope().app() == app {
            Ok(Rc::clone(&self.0))
        } else {
            Err(WorkflowServiceError::PermissionDenied)
        }
    }
}
