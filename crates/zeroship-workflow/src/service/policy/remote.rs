//! Ordered policy refresh for a fixed enrolled worker and app assignment.

use super::{HostPolicies, IngressEpochs, PolicyBinding, PolicyRefresh, PolicySnapshot};
use crate::WorkflowServiceError;
use futures::future::LocalBoxFuture;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use zeroship_core::{
    workflow_coordination::{AssignedScope, FailureCode, Revision},
    workflow_policy::{EstablishIngress, PolicyLeaseRequest},
};
use zeroship_workflow_client::{LeasedPolicy, WorkerCoordinator};

/// A host-owned policy binding for the client's exact signer and app assignment.
///
/// Clones retain the same generation. Construct a replacement when placement or
/// signer changes; refreshing never retargets the existing binding. Clones
/// share one establishment at a time, so concurrent fenced acceptances ask the
/// manager once.
#[derive(Clone, Debug)]
pub struct AssignedPolicies {
    binding: PolicyBinding,
    client: WorkerCoordinator,
    scope: AssignedScope,
    ingress_used: Arc<AtomicBool>,
    establishing: Arc<futures::lock::Mutex<()>>,
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
            establishing: Arc::new(futures::lock::Mutex::new(())),
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

    /// Obtain an open ingress epoch above `after`, the epoch the journal
    /// refused, or any open epoch when it names none, as at startup. The
    /// manager commits recovery responsibility before replying, so the
    /// installed epoch covers every acceptance that captures it. A newer epoch
    /// another exchange already installed satisfies the call without I/O.
    /// A concurrent refresh can supersede this exchange's refresh ticket after
    /// the manager committed the epoch; one more exchange then returns it.
    ///
    /// # Errors
    /// As [`Self::refresh`]; the manager refuses establishment with
    /// `PermissionDenied` while policy disables admission or placement is
    /// revoked, and with `Conflict` before activation created responsibility.
    #[expect(
        clippy::future_not_send,
        reason = "the metadata client stays on its owning compio runtime"
    )]
    pub async fn establish(&self, after: Option<Revision>) -> Result<(), WorkflowServiceError> {
        let _establishing = self.establishing.lock().await;
        let mut result = Ok(());
        for _ in 0..2 {
            if self.holds_above(after) {
                return Ok(());
            }
            result = self.exchange(Some(EstablishIngress { after })).await;
            if !matches!(result, Err(WorkflowServiceError::Unavailable(_))) {
                return result;
            }
        }
        if self.holds_above(after) {
            return Ok(());
        }
        result
    }

    /// Whether the installed snapshot holds an epoch above `after`, or any
    /// epoch when `after` names none.
    fn holds_above(&self, after: Option<Revision>) -> bool {
        self.binding
            .ingress_epoch()
            .is_some_and(|held| after.is_none_or(|after| held > after))
    }

    /// Report that this host accepted ingress since its previous exchange.
    pub fn note_ingress(&self) {
        self.ingress_used.store(true, Ordering::Relaxed);
    }

    #[expect(
        clippy::future_not_send,
        reason = "the metadata client stays on its owning compio runtime"
    )]
    async fn exchange(
        &self,
        establish: Option<EstablishIngress>,
    ) -> Result<(), WorkflowServiceError> {
        let ticket = self.binding.begin_refresh()?;
        let ingress_used = self.ingress_used.swap(false, Ordering::Relaxed);
        let request = PolicyLeaseRequest {
            scope: self.scope.clone(),
            establish,
            ingress_used,
        };
        let lease = match self.client.policy_lease(&request).await {
            Ok(lease) => lease,
            Err(error) => {
                // An unacknowledged report is repeated by the next exchange.
                if ingress_used {
                    self.ingress_used.store(true, Ordering::Relaxed);
                }
                return Err(match (establish, error) {
                    (Some(_), zeroship_workflow_client::Error::Refused(FailureCode::Denied)) => {
                        WorkflowServiceError::PermissionDenied
                    }
                    (Some(_), zeroship_workflow_client::Error::Refused(FailureCode::Conflict)) => {
                        WorkflowServiceError::Conflict(
                            "workflow ingress epoch cannot be established".into(),
                        )
                    }
                    (_, error) => transport_error(error),
                });
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
            return Err(super::unavailable(
                "the manager's lease does not match this binding's app, worker, key or assignment revision",
            ));
        }
        lease.remaining().map_err(transport_error)?;
        ticket.install(
            PolicySnapshot::lease(lease.revision(), lease.policy().clone(), lease.expires_at())?
                .with_anchor_slack(lease.anchor_slack())
                .with_ingress_epoch(lease.ingress_epoch()),
        )
    }
}

impl IngressEpochs for AssignedPolicies {
    fn establish(
        &self,
        after: Option<Revision>,
    ) -> LocalBoxFuture<'_, Result<(), WorkflowServiceError>> {
        Box::pin(Self::establish(self, after))
    }

    fn accepted(&self) {
        self.note_ingress();
    }
}

/// Carry a coordinator answer to the caller WITHOUT flattening a refusal into
/// an outage.
///
/// A refusal and an outage demand opposite responses: the first is durable and
/// wants an operator, the second is transient and wants a retry. Mapping every
/// non-timeout answer to `Unavailable` made them indistinguishable, and because
/// the consumer logs only `code()`, it also erased the reason before anything
/// could read it - a worker refused its lease reported `workflow_unavailable`
/// and nothing else. `establish` above already documents that callers see
/// `PermissionDenied` while policy disables admission or placement is revoked,
/// so the flattening also failed the contract stated one screen up.
fn transport_error(error: zeroship_workflow_client::Error) -> WorkflowServiceError {
    use zeroship_workflow_client::Error as Wire;
    match error {
        Wire::Timeout => WorkflowServiceError::Timeout,
        Wire::Unauthenticated | Wire::Refused(FailureCode::Unauthenticated) => {
            WorkflowServiceError::Unauthenticated
        }
        Wire::Refused(FailureCode::Denied) => WorkflowServiceError::PermissionDenied,
        Wire::Refused(FailureCode::Conflict) => {
            WorkflowServiceError::Conflict("workflow policy lease conflicts with the manager".into())
        }
        Wire::Refused(FailureCode::Capacity) => WorkflowServiceError::ResourceExhausted(
            "workflow policy lease refused for capacity".into(),
        ),
        Wire::Refused(FailureCode::Invalid) | Wire::Refused(FailureCode::RequestTooLarge) => {
            WorkflowServiceError::InvalidRequest("workflow policy lease request refused".into())
        }
        Wire::Refused(FailureCode::Unavailable)
        | Wire::Unavailable
        | Wire::InvalidConfig
        | Wire::InvalidResponse
        | Wire::RequestTooLarge
        | Wire::ResponseTooLarge => {
            WorkflowServiceError::Unavailable("workflow policy lease unavailable".into())
        }
    }
}

#[cfg(test)]
mod tests;
