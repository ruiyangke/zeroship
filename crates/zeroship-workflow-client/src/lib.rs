//! Native coordination over the service API. No database capability
//! crosses this interface; assignment metadata does not authorize journal I/O.
#![allow(
    clippy::future_not_send,
    reason = "HTTP connections stay on their compio runtime"
)]

mod control;
mod jobs;
mod policy;
mod queue_holds;
mod schema_bundles;
mod transport;

pub use control::ControlCoordinator;
pub use jobs::LeasedJob;
pub use policy::LeasedPolicy;
pub use queue_holds::QueueDeploymentHolds;
pub use schema_bundles::{SchemaBundles, MAX_BUNDLE_BYTES};
pub use transport::Transport;

use std::{sync::Arc, time::Duration};
use zeroship_core::workflow_coordination::{
    AssignedScope, Assignment, FailureCode, RegisterWorker, RegisteredWorker, ReleaseScope,
    ScopePage, WorkerId, AUDIENCE,
};
use zeroship_core::{
    schema_bundle::{EnsureJournal, SchemaBundleOutcome},
    service_assertion::ServiceIssuer,
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
    #[error("workflow coordinator requires an authorized service identity")]
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
    signing_key_id: String,
    transport: Transport,
}

impl WorkerCoordinator {
    /// Remote coordinators require HTTPS with system certificate verification.
    /// HTTP is accepted only for literal loopback addresses used by local hosts.
    ///
    /// # Errors
    /// Rejects ambiguous endpoints, empty bounds and missing worker instance keys.
    pub fn new(url: &str, auth: Arc<ServiceAuth>, options: Options) -> Result<Self, Error> {
        let (issuer, key) = auth.signing_identity().ok_or(Error::Unauthenticated)?;
        let role = service_issuer(WORKER_SERVICE_NAME).map_err(|_| Error::InvalidConfig)?;
        if issuer.principal() != role.principal() {
            return Err(Error::Unauthenticated);
        }
        let worker_id = WorkerId::parse(issuer.instance().ok_or(Error::Unauthenticated)?)
            .map_err(|_| Error::Unauthenticated)?;
        let signing_key_id = key.key_id();
        Ok(Self {
            worker_id,
            signing_key_id,
            transport: Transport::new(
                url,
                auth,
                ServiceIssuer::parse(AUDIENCE).map_err(|_| Error::InvalidConfig)?,
                options,
            )?,
        })
    }

    #[must_use]
    pub const fn worker_id(&self) -> &WorkerId {
        &self.worker_id
    }

    /// Thumbprint of the immutable enrolled key used for this client's requests.
    #[must_use]
    pub fn signing_key_id(&self) -> &str {
        &self.signing_key_id
    }

    /// Report that this host REFUSED the journal it found, and wait for the
    /// manager to bring it to the version this build expects.
    ///
    /// A worker holds no DDL authority of its own - privilege follows the
    /// process - so all it can do is name the schema and ask. Before this, a
    /// refused journal was terminal: nothing in the system could move it
    /// forward, and the host simply stopped.
    ///
    /// Idempotent at the far end, so a host may call it on every refusal.
    ///
    /// # Errors
    /// Refuses failed exchanges and an outcome describing another schema.
    pub async fn ensure_journal(&self, schema: &str) -> Result<SchemaBundleOutcome, Error> {
        let outcome: SchemaBundleOutcome = self
            .transport
            .post(
                endpoints::WORKFLOW_JOURNAL_ENSURE,
                &EnsureJournal {
                    schema: schema.to_owned(),
                },
            )
            .await?;
        if outcome.schema != schema {
            return Err(Error::InvalidResponse);
        }
        Ok(outcome)
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

    /// Give up one of this worker's placements. Release never discharges the
    /// manager's recovery responsibility; a `refused` release tells the manager
    /// never to offer the app to this worker instance again.
    ///
    /// # Errors
    /// Refuses failed exchanges and stale or foreign placements.
    pub async fn release(&self, request: &ReleaseScope) -> Result<(), Error> {
        self.transport
            .post(endpoints::WORKFLOW_RELEASE, request)
            .await
    }
}
