//! App-scoped calls to the customer engine on its owning compio thread.

use super::{policy::PolicyAuthority, AppWorkflows, PolicyBinding, RequestId};
use crate::{
    backend::WorkflowBackend,
    operations::{
        DeliveredSignal, RestartOptions, RestartedRun, RunOperation, RunStatus, SignalOptions,
        StartOptions, StartedRun, TransitionedRun,
    },
    WorkflowServiceError,
};
use async_trait::async_trait;
use futures::{channel::oneshot, future::LocalBoxFuture, FutureExt, StreamExt};
use zeroship_core::app_id::AppId;

const MAX_QUEUED_REQUESTS: usize = 64;
const MAX_ACTIVE_REQUESTS: usize = 16;
type Request = Box<dyn FnOnce(AppWorkflows) -> LocalBoxFuture<'static, ()> + Send>;

/// A thread-safe client. The customer database remains on the engine's thread.
#[derive(Clone)]
pub struct AppBackend {
    app: AppId,
    binding: PolicyBinding,
    requests: flume::Sender<Request>,
    max_output_bytes: usize,
}
impl std::fmt::Debug for AppBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppBackend")
            .field("app", &self.app)
            .finish_non_exhaustive()
    }
}
impl AppBackend {
    fn new(api: AppWorkflows, max_output_bytes: usize) -> Result<Self, WorkflowServiceError> {
        if max_output_bytes == 0 {
            return Err(WorkflowServiceError::InvalidRequest(
                "workflow output read limit must be positive".into(),
            ));
        }
        let (requests, receiver) = flume::bounded::<Request>(MAX_QUEUED_REQUESTS);
        let backend = Self {
            app: api.app_id().clone(),
            binding: api.binding.clone(),
            requests,
            max_output_bytes,
        };
        compio::runtime::spawn(async move {
            receiver
                .into_stream()
                .for_each_concurrent(MAX_ACTIVE_REQUESTS, |request| request(api.clone()))
                .await;
        })
        .detach();
        Ok(backend)
    }

    #[must_use]
    pub fn app_id(&self) -> &AppId {
        &self.app
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
    /// Bind the native interface to this engine thread with a bounded request queue.
    ///
    /// # Errors
    /// Rejects an empty output read limit.
    pub fn into_backend(self, max_output_bytes: usize) -> Result<AppBackend, WorkflowServiceError> {
        AppBackend::new(self, max_output_bytes)
    }
}
#[async_trait(?Send)]
impl WorkflowBackend for AppBackend {
    async fn start(
        &self,
        workflow_name: String,
        options: StartOptions,
    ) -> Result<StartedRun, WorkflowServiceError> {
        self.call(move |api| {
            async move { api.start(&RequestId::mint(), &workflow_name, options).await }
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
        self.call(move |api| {
            async move { api.signal(&RequestId::mint(), &run_id, options).await }.boxed_local()
        })
        .await
    }
    async fn transition(
        &self,
        run_id: String,
        op: RunOperation,
    ) -> Result<TransitionedRun, WorkflowServiceError> {
        self.call(move |api| {
            async move { api.transition(&RequestId::mint(), &run_id, op).await }.boxed_local()
        })
        .await
    }
    async fn restart(
        &self,
        run_id: String,
        options: RestartOptions,
    ) -> Result<RestartedRun, WorkflowServiceError> {
        self.call(move |api| {
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
        let limit = self.max_output_bytes;
        self.call(move |api| {
            async move {
                api.read_step_output(&run_id, &name, occurrence)
                    .await?
                    .into_bytes(limit)
                    .await
            }
            .boxed_local()
        })
        .await
    }
}
