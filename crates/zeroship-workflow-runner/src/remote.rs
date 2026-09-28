//! The `env.workflows` backend for a host that holds no journal.
//!
//! [`AppBackend`](zeroship_workflow::service::AppBackend) reaches the journal in
//! process, over a handle the host opened. This one reaches the workflow service
//! over HTTP, through the same [`WorkerCoordinator`] the host already registers
//! and claims jobs with, so a creator call costs one authenticated request and
//! the journal it lands in is the service's own.
//!
//! # Where this lives, and why
//!
//! [`WorkflowBackend`] is declared in `zeroship-workflow`, which itself declares
//! `zeroship-workflow-client` in `[dependencies]` -- so the client naming that
//! trait is a Cargo CYCLE before it is anything else, and
//! `workflow_process_dependencies_follow_crate_ownership`
//! (`xtask/tests/workflow_architecture.rs`) additionally forbids the client that
//! edge in the dev and transitive spellings Cargo would tolerate. Three reasons,
//! one answer. This crate depends on both, and it already holds the payload
//! object store the two reads open.
//!
//! # The two reads are two-phase, and the trait is unchanged
//!
//! `read_step_output` and `read_output` return `Vec<u8>`, and payload bytes
//! cannot cross this transport: one payload answers to the platform's payload
//! ceiling while a reply answers to its journal ceiling, which is smaller, and
//! the transport buffers JSON with no byte-stream path. So the JOURNAL LOOKUP
//! crosses and the bytes do not. The service proves which object the step owns
//! and answers with the key and the descriptor; this opens that object from the
//! store it already holds and verifies the stream against that descriptor. The
//! method keeps its signature and becomes two-phase inside, which is what stops
//! an opener being pushed into the in-process implementor that needs none.

#![allow(
    clippy::future_not_send,
    reason = "the coordinator client and the payload store stay on one compio runtime"
)]

#[cfg(test)]
mod tests;

use crate::payloads::{PayloadObjects, PayloadRead};
use async_trait::async_trait;
use serde_json::Value;
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{
        AssignedScope, PayloadLocation, ReadStepOutput, RequestId, RestartRun, RunId, RunScope,
        SignalRun, StartRun, StepOutputLocation, TransitionRun,
    },
};
use zeroship_workflow::{
    backend::WorkflowBackend,
    engine::WorkflowOutputRef,
    operations::{
        DeliveredSignal, RestartOptions, RestartedRun, RunOperation, RunStatus, SignalOptions,
        StartOptions, StartedRun, TransitionedRun,
    },
    WorkflowServiceError,
};
use zeroship_workflow_client::{RunError, WorkerCoordinator};

/// Creator calls for one app and one placement generation, served remotely.
///
/// The scope is fixed for the life of this backend, the way an
/// [`AppBackend`](zeroship_workflow::service::AppBackend)'s policy binding is:
/// an assignment whose revision moves is a different generation, and the host
/// replaces the entry rather than retargeting it.
///
/// # No local policy binding, and that is the decision
///
/// `AppWorkflows::into_backend` pairs a backend with the journal it will reach
/// and refuses a journal from another policy registry. There is no journal here
/// to pair with, and the binding a host holds is not the authority over a call
/// this service serves: `RunService::app` observes the app's policy and installs
/// it into the SERVICE's own registry on every request, and `require_open_epoch`
/// rechecks the journal's closed epoch inside the caller's transaction. So the
/// generation a call is admitted under is decided at the far end, and carrying a
/// second one here would be a claim nothing rechecks. What this side does carry
/// is the placement the far end authorizes against, which is `scope`.
#[derive(Clone, Debug)]
pub struct RemoteBackend {
    client: WorkerCoordinator,
    scope: AssignedScope,
    objects: PayloadObjects,
    /// Ceiling on the bytes one creator read may hold resident at once. The
    /// descriptor's own size is checked against it before the body is drained,
    /// so an oversized payload is refused without being read.
    read_limit: usize,
}

impl RemoteBackend {
    /// Serve `scope`'s app over `client`, reading payload objects from `objects`.
    ///
    /// # Errors
    /// Rejects an empty read budget.
    pub fn new(
        client: WorkerCoordinator,
        scope: AssignedScope,
        objects: PayloadObjects,
        read_limit: usize,
    ) -> Result<Self, WorkflowServiceError> {
        if read_limit == 0 {
            return Err(WorkflowServiceError::InvalidRequest(
                "workflow output read limit must be positive".into(),
            ));
        }
        Ok(Self {
            client,
            scope,
            objects,
            read_limit,
        })
    }

    /// The one app every call through this backend acts for.
    #[must_use]
    pub const fn app_id(&self) -> &AppId {
        &self.scope.app_id
    }

    /// The placement generation every call names.
    #[must_use]
    pub const fn scope(&self) -> &AssignedScope {
        &self.scope
    }

    fn run(run_id: &str) -> Result<RunId, WorkflowServiceError> {
        RunId::parse(run_id)
            .map_err(|_| WorkflowServiceError::InvalidRequest("invalid workflow run id".into()))
    }

    fn scoped(&self, run_id: &str) -> Result<RunScope, WorkflowServiceError> {
        Ok(RunScope {
            scope: self.scope.clone(),
            run_id: Self::run(run_id)?,
        })
    }

    /// Open the object a located read named, and collect it within the budget.
    async fn collect(&self, located: &PayloadLocation) -> Result<Vec<u8>, WorkflowServiceError> {
        self.read(&located.payload_id, &located.reference)
            .await?
            .into_bytes(self.read_limit)
            .await
    }

    async fn read(
        &self,
        payload_id: &str,
        reference: &WorkflowOutputRef,
    ) -> Result<PayloadRead, WorkflowServiceError> {
        self.objects
            .open_located(self.app_id(), payload_id, reference)
            .await
    }
}

/// Carry a remote run refusal into the engine's contract.
///
/// A REFUSAL is the engine answering, already in these terms, so it crosses back
/// unchanged -- which is what keeps the code a creator branches on and the message
/// a creator reads the same on both paths. A TRANSPORT failure is this host's
/// view of the exchange and establishes nothing about whether the call took
/// effect, so it becomes `Unavailable` or `Timeout` and never a durable refusal.
///
/// `IngressFenced` reaches a creator as `Unavailable`. It is the service telling
/// its caller to establish an epoch, and this caller holds no recovery scope to
/// establish one in: the service establishes its own, inside the request, and a
/// fence that survives that is an outage rather than anything creator code can
/// act on.
///
/// The match is wildcard-free over both enums, so a new refusal or transport
/// failure stops compiling here rather than folding into a neighbour.
fn refusal(error: RunError) -> WorkflowServiceError {
    use zeroship_core::workflow_coordination::RunFailure;
    use zeroship_workflow_client::Error as Transport;
    match error {
        RunError::Refused(failure) => match failure {
            RunFailure::InvalidRequest { message } => WorkflowServiceError::InvalidRequest(message),
            RunFailure::Unauthenticated {} => WorkflowServiceError::Unauthenticated,
            RunFailure::PermissionDenied {} => WorkflowServiceError::PermissionDenied,
            RunFailure::NotFound { message } => WorkflowServiceError::NotFound(message),
            RunFailure::Conflict { message } => WorkflowServiceError::Conflict(message),
            RunFailure::ResourceExhausted { message } => {
                WorkflowServiceError::ResourceExhausted(message)
            }
            RunFailure::PayloadTooLarge {} => WorkflowServiceError::PayloadTooLarge,
            RunFailure::Timeout {} => WorkflowServiceError::Timeout,
            RunFailure::IngressFenced { .. } | RunFailure::Unavailable {} => {
                WorkflowServiceError::Unavailable("workflow service is unavailable".into())
            }
            RunFailure::Internal {} => {
                WorkflowServiceError::Internal("workflow service failed the call".into())
            }
        },
        RunError::Transport(transport) => match transport {
            Transport::Timeout => WorkflowServiceError::Timeout,
            Transport::RequestTooLarge | Transport::ResponseTooLarge => {
                WorkflowServiceError::PayloadTooLarge
            }
            // An unauthenticated or misconfigured client is this host's own
            // defect, not something a creator call can be told to fix, and a
            // reply this client will not believe is the same. None of them is a
            // durable refusal of the operation.
            Transport::InvalidConfig
            | Transport::Unauthenticated
            | Transport::InvalidResponse
            | Transport::Unavailable
            | Transport::Refused(_) => {
                WorkflowServiceError::Unavailable("workflow service is unavailable".into())
            }
        },
    }
}

#[async_trait(?Send)]
impl WorkflowBackend for RemoteBackend {
    /// The VALUE crosses and no descriptor does.
    ///
    /// `StartOptions` carries `input_ref`, and this narrows it to the subset a
    /// creator may name, REFUSING one that already names an object rather than
    /// dropping it. The service stages the value into the store and names the
    /// object itself, so a run here cannot be pointed at bytes its caller did not
    /// supply.
    ///
    /// The request identity is minted on this side because it is the idempotency
    /// of the whole start: it keys the staged object as well as the receipt, so a
    /// retry under the same identity restages the same object.
    async fn start(
        &self,
        workflow_name: String,
        input: Value,
        options: StartOptions,
    ) -> Result<StartedRun, WorkflowServiceError> {
        let options = options.creator_subset().map_err(|error| {
            WorkflowServiceError::InvalidRequest(format!("invalid workflow start: {error}"))
        })?;
        self.client
            .start_run(&StartRun {
                request_id: RequestId::mint(),
                scope: self.scope.clone(),
                workflow_name,
                input,
                options,
            })
            .await
            .map_err(refusal)
    }

    async fn status(&self, run_id: String) -> Result<RunStatus, WorkflowServiceError> {
        self.client
            .run_status(&self.scoped(&run_id)?)
            .await
            .map_err(refusal)
    }

    async fn signal(
        &self,
        run_id: String,
        options: SignalOptions,
    ) -> Result<DeliveredSignal, WorkflowServiceError> {
        self.client
            .signal_run(&SignalRun {
                request_id: RequestId::mint(),
                scope: self.scope.clone(),
                run_id: Self::run(&run_id)?,
                options,
            })
            .await
            .map_err(refusal)
    }

    async fn transition(
        &self,
        run_id: String,
        op: RunOperation,
    ) -> Result<TransitionedRun, WorkflowServiceError> {
        self.client
            .transition_run(&TransitionRun {
                request_id: RequestId::mint(),
                scope: self.scope.clone(),
                run_id: Self::run(&run_id)?,
                operation: op,
            })
            .await
            .map_err(refusal)
    }

    async fn restart(
        &self,
        run_id: String,
        options: RestartOptions,
    ) -> Result<RestartedRun, WorkflowServiceError> {
        self.client
            .restart_run(&RestartRun {
                request_id: RequestId::mint(),
                scope: self.scope.clone(),
                run_id: Self::run(&run_id)?,
                options,
            })
            .await
            .map_err(refusal)
    }

    /// Two phases: the service locates the output, this host opens it.
    ///
    /// An output the journal kept inline was never an object, so the value is what
    /// the service answers with and serializing it here is the whole read. One
    /// that became an object comes back as a key and a descriptor, and the stream
    /// this opens verifies the bytes against that descriptor as they are consumed.
    async fn read_step_output(
        &self,
        run_id: String,
        name: String,
        occurrence: u32,
    ) -> Result<Vec<u8>, WorkflowServiceError> {
        let located = self
            .client
            .read_step_output(&ReadStepOutput {
                scope: self.scope.clone(),
                run_id: Self::run(&run_id)?,
                name,
                occurrence,
            })
            .await
            .map_err(refusal)?;
        match located {
            StepOutputLocation::Object { payload } => self.collect(&payload).await,
            StepOutputLocation::Inline { value } => serde_json::to_vec(&value)
                .map_err(|_| WorkflowServiceError::Internal("invalid step output".into())),
        }
    }

    /// Same two phases, minus the inline arm: a run's own output is always an
    /// object, and a run that returned nothing is reported as missing by the
    /// service.
    async fn read_output(&self, run_id: String) -> Result<Vec<u8>, WorkflowServiceError> {
        let located = self
            .client
            .read_run_output(&self.scoped(&run_id)?)
            .await
            .map_err(refusal)?;
        self.collect(&located).await
    }
}
