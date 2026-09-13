//! Ordered policy refresh for a fixed enrolled worker and app assignment.

use super::{HostPolicies, PolicyBinding, PolicyRefresh, PolicySnapshot};
use crate::WorkflowServiceError;
use std::sync::Arc;
use zeroship_core::workflow_coordination::AssignedScope;
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
        let ticket = self.binding.begin_refresh()?;
        let lease = self
            .client
            .policy_lease(&self.scope)
            .await
            .map_err(transport_error)?;
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
        ticket.install(PolicySnapshot::lease(
            lease.revision(),
            lease.policy().clone(),
            lease.expires_at(),
        )?)
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
