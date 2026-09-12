//! Worker-initiated coordination over the service API. No database capability
//! crosses this interface; assignment metadata does not authorize journal I/O.
#![allow(
    clippy::future_not_send,
    reason = "HTTP connections stay on their compio runtime"
)]

mod transport;

use std::{collections::HashSet, sync::Arc, time::Duration};
use transport::Transport;
use zeroship_core::workflow_coordination::{
    AcknowledgeManagement, AssignedScope, Assignment, FailureCode, ManageRun, ManagementReceipt,
    PublishWakeHint, RegisterWorker, RegisteredWorker, ReleaseScope, ScopePage, WakeHintReceipt,
    WorkerId,
};
use zeroship_core::{
    service_identity::endpoints,
    service_peers::{service_issuer, ServiceAuth, WORKER_SERVICE_NAME},
};

/// Bounds the complete exchange, including streamed error bodies.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub timeout: Duration,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            max_request_bytes: 64 * 1024,
            max_response_bytes: 1024 * 1024,
        }
    }
}

/// Errors contain no assertion, remote URL, response body or customer data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("invalid workflow coordinator client configuration")]
    InvalidConfig,
    #[error("workflow coordinator requires an enrolled worker identity")]
    Unauthenticated,
    #[error("workflow coordinator request exceeds its bound")]
    RequestTooLarge,
    #[error("workflow coordinator response exceeds its bound")]
    ResponseTooLarge,
    #[error("invalid workflow coordinator response")]
    InvalidResponse,
    #[error("workflow coordinator transport is unavailable")]
    Unavailable,
    #[error("workflow coordinator request timed out")]
    Timeout,
    #[error("workflow coordinator refused the request: {0:?}")]
    Refused(FailureCode),
}

/// A runtime-local client bound to the host's enrolled worker signer.
///
/// Each call mints a fresh assertion. Callers retry durable commands with their
/// original request identity; transport failure does not establish whether a
/// mutation committed. The client neither retries mutations nor follows redirects.
#[derive(Clone, Debug)]
pub struct WorkerCoordinator {
    worker_id: WorkerId,
    transport: Transport,
}

impl WorkerCoordinator {
    /// Remote coordinators require HTTPS with system certificate verification.
    /// HTTP is accepted only for literal loopback addresses used by local hosts.
    ///
    /// # Errors
    /// Rejects ambiguous endpoints, empty bounds and missing worker instance keys.
    pub fn new(url: &str, auth: Arc<ServiceAuth>, options: Options) -> Result<Self, Error> {
        let (issuer, _) = auth.signing_identity().ok_or(Error::Unauthenticated)?;
        let role = service_issuer(WORKER_SERVICE_NAME).map_err(|_| Error::InvalidConfig)?;
        if issuer.principal() != role.principal() {
            return Err(Error::Unauthenticated);
        }
        let worker_id = WorkerId::parse(issuer.instance().ok_or(Error::Unauthenticated)?)
            .map_err(|_| Error::Unauthenticated)?;
        Ok(Self {
            worker_id,
            transport: Transport::new(url, auth, options)?,
        })
    }

    #[must_use]
    pub const fn worker_id(&self) -> &WorkerId {
        &self.worker_id
    }

    /// # Errors
    /// Refuses failed exchanges and registration receipts for another identity.
    pub async fn register(&self, request: &RegisterWorker) -> Result<RegisteredWorker, Error> {
        let registered: RegisteredWorker = self
            .transport
            .post(endpoints::WORKFLOW_REGISTER, request)
            .await?;
        if registered.worker_id != self.worker_id
            || registered.capacity != request.capacity
            || registered.state != request.state
        {
            return Err(Error::InvalidResponse);
        }
        Ok(registered)
    }

    /// # Errors
    /// Refuses failed exchanges, foreign workers and unordered/repeated page entries.
    pub async fn assignments(&self, request: &ScopePage) -> Result<Vec<Assignment>, Error> {
        let assignments: Vec<Assignment> = self
            .transport
            .post(endpoints::WORKFLOW_ASSIGNMENTS, request)
            .await?;
        let mut previous = request.after.as_ref();
        for assignment in &assignments {
            if assignment.worker_id != self.worker_id
                || previous.is_some_and(|app| app.as_str() >= assignment.app_id.as_str())
            {
                return Err(Error::InvalidResponse);
            }
            previous = Some(&assignment.app_id);
        }
        Ok(assignments)
    }

    /// # Errors
    /// Refuses failed exchanges and renewals that change the requested authority.
    pub async fn renew(&self, request: &AssignedScope) -> Result<Assignment, Error> {
        let assignment: Assignment = self
            .transport
            .post(endpoints::WORKFLOW_RENEW, request)
            .await?;
        if (
            &assignment.worker_id,
            &assignment.app_id,
            assignment.revision,
        ) != (
            &self.worker_id,
            &request.app_id,
            request.assignment_revision,
        ) {
            return Err(Error::InvalidResponse);
        }
        Ok(assignment)
    }

    /// Publish only after persisting the hint in the customer's journal.
    ///
    /// # Errors
    /// Refuses failed exchanges and acknowledgements for another hint or assignment.
    pub async fn publish_wake(&self, request: &PublishWakeHint) -> Result<WakeHintReceipt, Error> {
        let receipt: WakeHintReceipt = self
            .transport
            .post(endpoints::WORKFLOW_WAKE, request)
            .await?;
        if receipt.app_id != request.app_id
            || receipt.assignment_revision != request.assignment_revision
            || receipt.revision != request.revision
        {
            return Err(Error::InvalidResponse);
        }
        Ok(receipt)
    }

    /// Release after quiescing local scheduling and confirming its final wake hint.
    /// The coordinator also requires a ready peer assigned to the same app.
    ///
    /// # Errors
    /// Refuses failed exchanges, stale assignments and unconfirmed wake hints.
    pub async fn release(&self, request: &ReleaseScope) -> Result<(), Error> {
        self.transport
            .post(endpoints::WORKFLOW_RELEASE, request)
            .await
    }

    /// Delivery can repeat after restart or reassignment. Apply commands through
    /// the customer's app-bound engine using their stable request identity.
    ///
    /// # Errors
    /// Refuses failed exchanges, foreign app commands and repeated request identities.
    pub async fn pending_management(
        &self,
        request: &AssignedScope,
    ) -> Result<Vec<ManageRun>, Error> {
        let commands: Vec<ManageRun> = self
            .transport
            .post(endpoints::WORKFLOW_MANAGEMENT_POLL, request)
            .await?;
        let mut seen = HashSet::new();
        for command in &commands {
            if command.app_id != request.app_id || !seen.insert(&command.request_id) {
                return Err(Error::InvalidResponse);
            }
        }
        Ok(commands)
    }

    /// Acknowledge only an outcome already persisted by the customer's engine.
    ///
    /// # Errors
    /// Refuses failed exchanges, changed outcomes and foreign request receipts.
    pub async fn acknowledge_management(
        &self,
        request: &AcknowledgeManagement,
    ) -> Result<ManagementReceipt, Error> {
        let receipt: ManagementReceipt = self
            .transport
            .post(endpoints::WORKFLOW_MANAGEMENT_ACK, request)
            .await?;
        if receipt.app_id != request.app_id
            || receipt.request_id != request.request_id
            || receipt.outcome != Some(request.outcome)
        {
            return Err(Error::InvalidResponse);
        }
        Ok(receipt)
    }
}
