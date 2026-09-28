//! Native coordination over the service API. No database capability
//! crosses this interface; assignment metadata does not authorize journal I/O.
#![allow(
    clippy::future_not_send,
    reason = "HTTP connections stay on their compio runtime"
)]

mod app_facts;
mod control;
mod jobs;
mod journal;
mod policy;
mod queue_holds;
mod schema_bundles;
mod transport;

pub use app_facts::ControlAppFacts;
pub use control::ControlCoordinator;
pub use jobs::{ClaimedJob, LeasedJob, RenewedJob};
pub use journal::{
    ClaimedDelivery, Exclusive, JobJournal, JobReceiptQuery, ReleaseDelivery, RenewDelivery,
    RenewedDelivery, Reported, SettleDelivery,
};
pub use policy::LeasedPolicy;
pub use queue_holds::QueueDeploymentHolds;
pub use schema_bundles::{SchemaBundles, MAX_BUNDLE_BYTES};
pub use transport::Transport;

use std::{sync::Arc, time::Duration};
use zeroship_core::workflow_coordination::{
    AssignedScope, Assignment, DeliveredSignal, FailureCode, PayloadLocation, ReadStepOutput,
    RegisterWorker, RegisteredWorker, ReleaseScope, RestartRun, RestartedRun, RunFailure, RunId,
    RunScope, RunStatus, ScopePage, SignalRun, StartRun, StartedRun, StepOutputLocation,
    TransitionRun, TransitionedRun, WorkerId, AUDIENCE,
};
use zeroship_core::{
    config::PlaintextPeers,
    service_assertion::ServiceIssuer,
    service_identity::endpoints,
    service_peers::{service_issuer, ServiceAuth, WORKER_SERVICE_NAME},
    typed_id,
    workflow_policy::{MAX_INPUT_BYTES_CEILING, MAX_JOURNAL_BYTES_CEILING},
};

/// Bounds the complete exchange, including streamed error bodies, and the
/// peers this process may reach over plaintext HTTP.
#[derive(Clone, Debug)]
pub struct Options {
    pub timeout: Duration,
    pub max_request_bytes: usize,
    /// Bound for a request whose body carries a JOURNAL quantity rather than a
    /// creator input: a settlement reporting an outcome batch. That batch has to
    /// fit the run's replay journal, so it answers to `max_journal_bytes` and
    /// not to `max_input_bytes`, which governs one value at a time.
    pub max_journal_request_bytes: usize,
    pub max_response_bytes: usize,
    /// Origins an operator named as reachable in clear. Empty by default, and
    /// the default is the whole deployment that configures nothing: the fence
    /// in [`Transport`] then admits HTTPS and literal loopback alone.
    pub plaintext_peers: PlaintextPeers,
}

impl Default for Options {
    /// The byte bounds derive from the platform ceilings for the quantities
    /// this transport would carry: a request carries what `max_input_bytes`
    /// governs, and a response carries what `max_journal_bytes` governs.
    /// Deriving them is what stops this client refusing, on its own account,
    /// something admission admitted. What a peer will accept is that peer's
    /// own bound, declared where the peer configures its body limit.
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            max_request_bytes: MAX_INPUT_BYTES_CEILING,
            max_journal_request_bytes: MAX_JOURNAL_BYTES_CEILING,
            max_response_bytes: MAX_JOURNAL_BYTES_CEILING,
            plaintext_peers: PlaintextPeers::default(),
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

/// How a creator-facing run call failed.
///
/// Two arms, because a caller acts on them differently. A transport failure
/// establishes nothing about whether the call took effect, so a durable command
/// is retried under its original request identity. A refusal is the engine
/// answering, and it carries the code a creator branches on and the message a
/// creator reads.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Transport(#[from] Error),
    #[error("workflow refused the run call: {0:?}")]
    Refused(RunFailure),
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
    /// HTTP reaches a literal loopback address, or an origin the options named
    /// in `plaintext_peers`.
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

    /// Admit a run of one workflow from the value a creator supplied.
    ///
    /// The VALUE crosses and no descriptor does: the service stages the value
    /// into the object store it owns and names the object itself, which is what
    /// stops a caller pointing a run at bytes it never supplied.
    /// [`StartRun::options`] has no field for one, so this client cannot send one
    /// even by mistake.
    ///
    /// The request identity is the caller's, because it is the idempotency of the
    /// start: a retry after an uncertain reply sends the same one and replays the
    /// receipt the first attempt stored.
    ///
    /// # Errors
    /// Refuses failed exchanges, the engine own refusals, and a receipt naming an
    /// id that is not a workflow run.
    pub async fn start_run(&self, request: &StartRun) -> Result<StartedRun, RunError> {
        let started: StartedRun = self
            .transport
            .post_run(endpoints::WORKFLOW_RUN_START, request)
            .await?;
        // A reply's id becomes the run every later call of this creator names, so
        // it is parsed against the one prefix a run may carry rather than taken
        // as text. A peer answering another entity's id is refused here.
        if RunId::parse(&started.id).is_err() {
            return Err(RunError::Transport(Error::InvalidResponse));
        }
        Ok(started)
    }

    /// Read a run current state and, once it has settled, what it left behind.
    ///
    /// The reply LOCATES a run output rather than carrying it, so this exchange
    /// stays small whatever the run returned.
    ///
    /// # Errors
    /// Refuses failed exchanges and the engine own refusals.
    pub async fn run_status(&self, request: &RunScope) -> Result<RunStatus, RunError> {
        self.transport
            .post_run(endpoints::WORKFLOW_RUN_STATUS, request)
            .await
    }

    /// Deliver a signal to a waiting run.
    ///
    /// # Errors
    /// Refuses failed exchanges and the engine own refusals.
    pub async fn signal_run(&self, request: &SignalRun) -> Result<DeliveredSignal, RunError> {
        self.transport
            .post_run(endpoints::WORKFLOW_RUN_SIGNAL, request)
            .await
    }

    /// Move a run through a lifecycle transition.
    ///
    /// # Errors
    /// Refuses failed exchanges and the engine own refusals.
    pub async fn transition_run(
        &self,
        request: &TransitionRun,
    ) -> Result<TransitionedRun, RunError> {
        self.transport
            .post_run(endpoints::WORKFLOW_RUN_TRANSITION, request)
            .await
    }

    /// Restart a run, retaining whatever journal prefix the options name.
    ///
    /// # Errors
    /// Refuses failed exchanges, the engine own refusals, and a receipt for a
    /// run other than the one asked for.
    pub async fn restart_run(&self, request: &RestartRun) -> Result<RestartedRun, RunError> {
        let restarted: RestartedRun = self
            .transport
            .post_run(endpoints::WORKFLOW_RUN_RESTART, request)
            .await?;
        if restarted.run_id != request.run_id.as_str() {
            return Err(RunError::Transport(Error::InvalidResponse));
        }
        Ok(restarted)
    }

    /// Locate what one completed step of a run recorded.
    ///
    /// The reply LOCATES the output. A payload answers to the platform's payload
    /// ceiling and a reply to its journal ceiling, which is smaller, and this
    /// transport buffers JSON with no byte-stream path -- so an object's bytes
    /// cannot cross here at all, and the caller opens the object itself from the
    /// store it holds. An output the journal kept inline is journal content and
    /// comes back as the value.
    ///
    /// # Errors
    /// Refuses failed exchanges, the engine own refusals, and a location whose
    /// payload key is not a workflow payload id.
    pub async fn read_step_output(
        &self,
        request: &ReadStepOutput,
    ) -> Result<StepOutputLocation, RunError> {
        let located: StepOutputLocation = self
            .transport
            .post_run(endpoints::WORKFLOW_RUN_STEP_OUTPUT, request)
            .await?;
        if let StepOutputLocation::Object { payload } = &located {
            validate_payload_key(payload)?;
        }
        Ok(located)
    }

    /// Locate what a settled run returned.
    ///
    /// Same shape as [`Self::read_step_output`] and for the same reason, minus the
    /// inline arm: a run's own output is always an object, so a run that returned
    /// nothing has no location to answer with and is refused as missing.
    ///
    /// # Errors
    /// Refuses failed exchanges, the engine own refusals, and a location whose
    /// payload key is not a workflow payload id.
    pub async fn read_run_output(&self, request: &RunScope) -> Result<PayloadLocation, RunError> {
        let located: PayloadLocation = self
            .transport
            .post_run(endpoints::WORKFLOW_RUN_OUTPUT, request)
            .await?;
        validate_payload_key(&located)?;
        Ok(located)
    }
}

/// A payload key addresses an object store, so it is parsed against the one
/// prefix a payload may carry before any caller uses it as a key. A peer naming
/// another entity's id, or free text, is refused here rather than reaching the
/// store.
fn validate_payload_key(located: &PayloadLocation) -> Result<(), RunError> {
    typed_id::parse_with_prefix(&located.payload_id, typed_id::WORKFLOW_PAYLOAD_PREFIX)
        .map(|_| ())
        .map_err(|_| RunError::Transport(Error::InvalidResponse))
}
