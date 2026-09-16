//! Customer-side deployment retention clients. Platform catalog storage belongs
//! to the workflow manager and is supplied through the host's scoped client.

use crate::WorkflowServiceError;
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
