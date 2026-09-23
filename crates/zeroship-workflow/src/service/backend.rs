//! App-scoped calls to the customer engine on its owning compio thread.

use super::{policy::PolicyAuthority, AppWorkflows, PolicyBinding, RequestId, WorkflowService};
use crate::{
    backend::{SharedInputStager, SharedStepOutputs, WorkflowBackend},
    operations::{
        DeliveredSignal, RestartOptions, RestartedRun, RunOperation, RunStatus, SignalOptions,
        StartOptions, StartedRun, TransitionedRun,
    },
    WorkflowServiceError,
};
use async_trait::async_trait;
use futures::{channel::oneshot, future::LocalBoxFuture, FutureExt, StreamExt};
use serde_json::Value;
use std::sync::Arc;
use zeroship_core::app_id::AppId;

const MAX_QUEUED_REQUESTS: usize = 64;
const MAX_ACTIVE_REQUESTS: usize = 16;
type Request = Box<dyn FnOnce(AppWorkflows) -> LocalBoxFuture<'static, ()> + Send>;

/// Tells the trusted host that a mutating call finished.
///
/// The call's committed publication intents may be pending, and the host can
/// publish them immediately; manager reconciliation still recovers any intent
/// the host misses. The hint carries no customer data and grants no authority.
pub type CommitHint = Arc<dyn Fn() + Send + Sync>;

/// A thread-safe client. The customer database remains on the engine's thread.
#[derive(Clone)]
pub struct AppBackend {
    app: AppId,
    binding: PolicyBinding,
    requests: flume::Sender<Request>,
    outputs: SharedStepOutputs,
    inputs: SharedInputStager,
    commit_hint: Option<CommitHint>,
}
impl std::fmt::Debug for AppBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppBackend")
            .field("app", &self.app)
            .finish_non_exhaustive()
    }
}
impl AppBackend {
    fn new(api: AppWorkflows, outputs: SharedStepOutputs, inputs: SharedInputStager) -> Self {
        let (requests, receiver) = flume::bounded::<Request>(MAX_QUEUED_REQUESTS);
        let backend = Self {
            app: api.app_id().clone(),
            binding: api.binding.clone(),
            requests,
            outputs,
            inputs,
            commit_hint: None,
        };
        compio::runtime::spawn(async move {
            receiver
                .into_stream()
                .for_each_concurrent(MAX_ACTIVE_REQUESTS, |request| request(api.clone()))
                .await;
        })
        .detach();
        backend
    }

    #[must_use]
    pub fn app_id(&self) -> &AppId {
        &self.app
    }

    /// The policy generation every call through this backend is bound to.
    pub const fn binding(&self) -> &PolicyBinding {
        &self.binding
    }

    /// Signal the host after every start, signal, transition and restart,
    /// including failed calls whose commit outcome may be uncertain. Clones
    /// share the hint; reads never trigger it.
    #[must_use]
    pub fn with_commit_hint(mut self, hint: CommitHint) -> Self {
        self.commit_hint = Some(hint);
        self
    }

    #[expect(
        clippy::future_not_send,
        reason = "caller cancellation is driven by its compio runtime"
    )]
    async fn mutate<T: Send + 'static>(
        &self,
        operation: impl FnOnce(AppWorkflows) -> LocalBoxFuture<'static, Result<T, WorkflowServiceError>>
            + Send
            + 'static,
    ) -> Result<T, WorkflowServiceError> {
        let result = self.call(operation).await;
        if let Some(hint) = &self.commit_hint {
            hint();
        }
        result
    }

    #[expect(
        clippy::future_not_send,
        reason = "caller cancellation is driven by its compio runtime"
    )]
    async fn call<T: Send + 'static>(
        &self,
        operation: impl FnOnce(AppWorkflows) -> LocalBoxFuture<'static, Result<T, WorkflowServiceError>>
            + Send
            + 'static,
    ) -> Result<T, WorkflowServiceError> {
        let authority = self.binding.authority()?;
        authority
            .run(self.dispatch(Some(authority.clone()), operation))
            .await
    }

    async fn dispatch<T: Send + 'static>(
        &self,
        authority: Option<PolicyAuthority>,
        operation: impl FnOnce(AppWorkflows) -> LocalBoxFuture<'static, Result<T, WorkflowServiceError>>
            + Send
            + 'static,
    ) -> Result<T, WorkflowServiceError> {
        let (mut reply, receive) = oneshot::channel();
        let request: Request = Box::new(move |api| {
            async move {
                if reply.is_canceled() {
                    return;
                }
                let result = {
                    let cancelled = reply.cancellation();
                    let operation = async move {
                        if let Some(authority) = authority {
                            let api = api.with_authority(authority.clone())?;
                            authority.run(operation(api)).await
                        } else {
                            operation(api).await
                        }
                    };
                    futures::pin_mut!(cancelled, operation);
                    match futures::future::select(cancelled, operation).await {
                        futures::future::Either::Right((result, _)) => result,
                        futures::future::Either::Left(_) => return,
                    }
                };
                let _ = reply.send(result);
            }
            .boxed_local()
        });
        self.requests
            .try_send(request)
            .map_err(|error| match error {
                flume::TrySendError::Full(_) => {
                    WorkflowServiceError::ResourceExhausted("workflow request queue is full".into())
                }
                flume::TrySendError::Disconnected(_) => unavailable(),
            })?;
        receive.await.map_err(|_| unavailable())?
    }
}
fn unavailable() -> WorkflowServiceError {
    WorkflowServiceError::Unavailable("workflow engine is unavailable".into())
}
impl AppWorkflows {
    /// Bind the native interface to this engine thread with a bounded request
    /// queue, reading and writing the journal `journal` was opened over.
    ///
    /// Which store the backend reaches is a choice its construction site
    /// makes, not one the handle carries: every call through the returned
    /// backend goes to `journal`'s store, while the app identity, its policy
    /// binding, its ingress, deployments and signal authority stay as this
    /// handle holds them, `outputs` resolves a step's stored output to bytes
    /// against the object store its host owns, and `inputs` writes the object a
    /// started run's value becomes into that same store. Naming the service
    /// this handle was
    /// bound to keeps the creator seam on the same database as the app's
    /// execution; naming another service's puts it on that one. A
    /// [`WorkflowService`] exists only over a journal it verified as it
    /// opened, so no unverified store reaches a backend this way.
    ///
    /// # Errors
    /// Rejects a journal opened over a different policy registry than this
    /// handle's binding.
    pub fn into_backend(
        mut self,
        journal: &WorkflowService,
        outputs: SharedStepOutputs,
        inputs: SharedInputStager,
    ) -> Result<AppBackend, WorkflowServiceError> {
        if !self.binding.belongs_to(&journal.policies) {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        self.service.store = journal.store.clone();
        Ok(AppBackend::new(self, outputs, inputs))
    }
}
#[async_trait(?Send)]
impl WorkflowBackend for AppBackend {
    async fn start(
        &self,
        workflow_name: String,
        input: Value,
        mut options: StartOptions,
    ) -> Result<StartedRun, WorkflowServiceError> {
        let inputs = self.inputs.clone();
        self.mutate(move |api| {
            async move {
                // One identity for the whole start: the object the value became
                // and the run that names it are both keyed by it, so a retried
                // start restages the same object and replays the same receipt.
                // Staging comes first because it is object I/O, and the
                // transaction it precedes holds the app lock.
                let request = RequestId::mint();
                let bound = api.service.policy_for(&api.app)?.max_input_bytes;
                options.input_ref = super::payloads::stage_start_input(
                    inputs.as_ref(),
                    &api,
                    &request,
                    &input,
                    bound,
                )
                .await?;
                api.start(&request, &workflow_name, options).await
            }
            .boxed_local()
        })
        .await
    }
    async fn status(&self, run_id: String) -> Result<RunStatus, WorkflowServiceError> {
        // Status reads existing app history and cannot grant mutation authority.
        self.dispatch(None, move |api| {
            async move { api.status(&run_id).await }.boxed_local()
        })
        .await
    }
    async fn signal(
        &self,
        run_id: String,
        options: SignalOptions,
    ) -> Result<DeliveredSignal, WorkflowServiceError> {
        self.mutate(move |api| {
            async move { api.signal(&RequestId::mint(), &run_id, options).await }.boxed_local()
        })
        .await
    }
    async fn transition(
        &self,
        run_id: String,
        op: RunOperation,
    ) -> Result<TransitionedRun, WorkflowServiceError> {
        self.mutate(move |api| {
            async move { api.transition(&RequestId::mint(), &run_id, op).await }.boxed_local()
        })
        .await
    }
    async fn restart(
        &self,
        run_id: String,
        options: RestartOptions,
    ) -> Result<RestartedRun, WorkflowServiceError> {
        self.mutate(move |api| {
            async move { api.restart(&RequestId::mint(), &run_id, options).await }.boxed_local()
        })
        .await
    }
    async fn read_step_output(
        &self,
        run_id: String,
        name: String,
        occurrence: u32,
    ) -> Result<Vec<u8>, WorkflowServiceError> {
        let outputs = self.outputs.clone();
        self.call(move |api| {
            async move { outputs.read(&api, &run_id, &name, occurrence).await }.boxed_local()
        })
        .await
    }
    async fn read_output(&self, run_id: String) -> Result<Vec<u8>, WorkflowServiceError> {
        let outputs = self.outputs.clone();
        self.call(move |api| {
            async move { outputs.read_output(&api, &run_id).await }.boxed_local()
        })
        .await
    }
}
