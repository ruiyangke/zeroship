//! Ordered policy refresh for a fixed enrolled worker and app assignment.

use super::{HostPolicies, PolicyBinding, PolicyRefresh, PolicySnapshot};
use crate::WorkflowServiceError;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use zeroship_core::{
    workflow_coordination::{AssignedScope, Revision},
    workflow_policy::PolicyLeaseRequest,
};
use zeroship_workflow_client::{LeasedPolicy, WorkerCoordinator};

/// A host-owned policy binding for the client's exact signer and app assignment.
///
/// Clones retain the same generation. Construct a replacement when placement or
/// signer changes; refreshing never retargets the existing binding.
#[derive(Clone, Debug)]
pub struct AssignedPolicies {
    binding: PolicyBinding,
    client: WorkerCoordinator,
    scope: AssignedScope,
    ingress_used: Arc<AtomicBool>,
}

impl AssignedPolicies {
    /// Explicitly replace this app's host binding with an uninitialized remote
    /// generation.
    ///
    /// Existing configured or remote handles are retired immediately.
    ///
    /// # Errors
    /// Refuses unavailable registry state or exhausted binding identity.
    pub fn new(
        registry: &Arc<HostPolicies>,
        client: WorkerCoordinator,
        scope: AssignedScope,
    ) -> Result<Self, WorkflowServiceError> {
        let binding = registry.bind(scope.app_id.clone())?;
        Ok(Self {
            binding,
            client,
            scope,
            ingress_used: Arc::new(AtomicBool::new(false)),
        })
    }

    #[must_use]
    pub const fn binding(&self) -> &PolicyBinding {
        &self.binding
    }

    #[must_use]
    pub const fn scope(&self) -> &AssignedScope {
        &self.scope
    }

    /// Reserve response ordering before I/O and install only into this generation.
    ///
    /// Failed or cancelled exchanges preserve the prior snapshot and its original
    /// expiry. They never revoke a newer refresh or install configured defaults.
    ///
    /// # Errors
    /// Refuses retired or superseded bindings, unavailable remote authority,
    /// invalid responses and conflicting source policy revisions.
    #[expect(
        clippy::future_not_send,
        reason = "the metadata client stays on its owning compio runtime"
    )]
    pub async fn refresh(&self) -> Result<(), WorkflowServiceError> {
        self.exchange(None).await
    }

    /// Obtain an open ingress epoch above `after`, the epoch the journal refused.
    /// The manager commits recovery responsibility before replying, so the
    /// installed epoch covers every acceptance that captures it.
    ///
    /// # Errors
    /// As [`Self::refresh`]; the manager also refuses establishment while
    /// policy disables admission.
    #[expect(
        clippy::future_not_send,
        reason = "the metadata client stays on its owning compio runtime"
    )]
    pub async fn establish(&self, after: Revision) -> Result<(), WorkflowServiceError> {
        self.exchange(Some(after)).await
    }

    /// Report that this host accepted ingress since its previous exchange.
    pub fn note_ingress(&self) {
        self.ingress_used.store(true, Ordering::Relaxed);
    }

    #[expect(
        clippy::future_not_send,
        reason = "the metadata client stays on its owning compio runtime"
    )]
    async fn exchange(&self, establish_after: Option<Revision>) -> Result<(), WorkflowServiceError> {
        let ticket = self.binding.begin_refresh()?;
        let ingress_used = self.ingress_used.swap(false, Ordering::Relaxed);
        let request = PolicyLeaseRequest {
            scope: self.scope.clone(),
            establish_after,
            ingress_used,
        };
        let lease = match self.client.policy_lease(&request).await {
            Ok(lease) => lease,
            Err(error) => {
                // An unacknowledged report is repeated by the next exchange.
                if ingress_used {
                    self.ingress_used.store(true, Ordering::Relaxed);
                }
                return Err(transport_error(error));
            }
        };
        self.install(ticket, &lease)
    }

    fn install(
        &self,
        ticket: PolicyRefresh,
        lease: &LeasedPolicy,
    ) -> Result<(), WorkflowServiceError> {
        if lease.app_id() != self.binding.app_id()
            || lease.app_id() != &self.scope.app_id
            || lease.worker_id() != self.client.worker_id()
            || lease.signing_key_id() != self.client.signing_key_id()
            || lease.assignment_revision() != self.scope.assignment_revision
        {
            return Err(super::unavailable());
        }
        lease.remaining().map_err(transport_error)?;
        ticket.install(
            PolicySnapshot::lease(lease.revision(), lease.policy().clone(), lease.expires_at())?
                .with_ingress_epoch(lease.ingress_epoch()),
        )
    }
}

fn transport_error(error: zeroship_workflow_client::Error) -> WorkflowServiceError {
    match error {
        zeroship_workflow_client::Error::Timeout => WorkflowServiceError::Timeout,
        _ => WorkflowServiceError::Unavailable("workflow policy lease unavailable".into()),
    }
}

#[cfg(test)]
mod tests;
