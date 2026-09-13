//! Workflow-manager access to Control's queue deployment retention ledger.

use super::{Error, Options, Transport};
use std::sync::Arc;
use zeroship_core::{
    service_identity::{ServiceEndpoint, endpoints},
    service_peers::{CONTROL_SERVICE_NAME, ServiceAuth, WORKFLOW_SERVICE_NAME, service_issuer},
    workflow_deployments::{HoldReceipt, HoldScope, HoldState, QueueHoldRequest},
};

/// Metadata client for the authoritative manager namespace, independent of workers.
/// Callers persist generation intents and retry the same request after uncertainty.
#[derive(Clone, Debug)]
pub struct QueueDeploymentHolds {
    transport: Transport,
}

impl QueueDeploymentHolds {
    /// Bind the workflow service's own signer and Control origin.
    ///
    /// # Errors
    /// Refuses missing or foreign signers, instance credentials, invalid origins
    /// and empty exchange bounds.
    pub fn new(url: &str, auth: Arc<ServiceAuth>, options: Options) -> Result<Self, Error> {
        let (issuer, _) = auth.signing_identity().ok_or(Error::Unauthenticated)?;
        let role = service_issuer(WORKFLOW_SERVICE_NAME).map_err(|_| Error::InvalidConfig)?;
        if issuer != &role {
            return Err(Error::Unauthenticated);
        }
        Ok(Self {
            transport: Transport::new(
                url,
                auth,
                service_issuer(CONTROL_SERVICE_NAME).map_err(|_| Error::InvalidConfig)?,
                options,
            )?,
        })
    }

    /// Acquire queue retention before admitting the deployment dependency.
    ///
    /// # Errors
    /// Refuses transport failures, Control errors and mismatched receipts.
    pub async fn acquire(&self, request: &QueueHoldRequest) -> Result<HoldReceipt, Error> {
        self.change(
            endpoints::CONTROL_QUEUE_DEPLOYMENT_HOLD_ACQUIRE,
            request,
            HoldState::Held,
        )
        .await
    }

    /// Release a generation after durably closing queue dependency admission.
    ///
    /// # Errors
    /// Refuses transport failures, Control errors and mismatched receipts.
    pub async fn release(&self, request: &QueueHoldRequest) -> Result<HoldReceipt, Error> {
        self.change(
            endpoints::CONTROL_QUEUE_DEPLOYMENT_HOLD_RELEASE,
            request,
            HoldState::Released,
        )
        .await
    }

    async fn change(
        &self,
        endpoint: ServiceEndpoint,
        request: &QueueHoldRequest,
        state: HoldState,
    ) -> Result<HoldReceipt, Error> {
        let receipt: HoldReceipt = self.transport.post(endpoint, request).await?;
        let scope = HoldScope::for_queue(request.app_id.clone());
        if receipt.app_id != request.app_id
            || receipt.deploy_id != request.deploy_id.as_str()
            || receipt.holder_id != scope.holder()
            || receipt.generation != request.generation
            || receipt.state != state
            || receipt.deploy_hash.len() != 64
            || !receipt
                .deploy_hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(Error::InvalidResponse);
        }
        Ok(receipt)
    }
}
