//! V8 lifecycle adapter for the shared workflow runner.

use async_trait::async_trait;
use std::{future::Future, rc::Rc, task::Poll, time::Duration};
use zeroship_bundle::LoadedWorker;
use zeroship_runtime::{CancelFlag, EnvSnapshot, RequestCtx, Runtime, WorkflowOutcome};
use zeroship_workflow_runner::{
    ExecutionBudget, PreparedExecution, TaskExecution, TaskExecutor, TaskPayloadLimits,
    TaskPayloadReader, TaskPayloads,
};
use zeroship_workflow::{
    service::TaskAssignment,
    WorkflowExecution, WorkflowInvocation, WorkflowServiceError,
};

/// A trusted host binds the retained executable and the assignment's app context.
///
/// The module graph and runtime descriptor must come from the supplied executable;
/// runtime variables and native handles come from the host's trusted app binding.
/// Loading constructs the isolate synchronously from inputs already read by the
/// executor. It must not evaluate creator code or start its runtime pump.
pub trait WorkflowRuntimeLoader {
    /// Build an exclusive runtime using the assignment's authorized app context.
    ///
    /// # Errors
    /// Refuse missing or mismatched app context and invalid runtime inputs.
    fn load(
        &self,
        assignment: &TaskAssignment,
        executable: &LoadedWorker,
    ) -> Result<LoadedWorkflow, WorkflowServiceError>;
}

/// Owns an exclusive workflow isolate.
///
/// Its pump must not have been started and the runtime must be newly built,
/// with its initial isolate entry still active.
/// The loader must not initialize modules or evaluate app code before returning.
pub struct LoadedWorkflow {
    runtime: Runtime,
    env: EnvSnapshot,
}
impl std::fmt::Debug for LoadedWorkflow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedWorkflow").finish_non_exhaustive()
    }
}
impl LoadedWorkflow {
    #[must_use]
    pub fn new(runtime: Runtime, env: EnvSnapshot) -> Self {
        // Restore the enclosing host isolate before returning to the executor.
        runtime.exit_isolate();
        Self { runtime, env }
    }
}
impl Drop for LoadedWorkflow {
    fn drop(&mut self) {
        self.runtime.quarantine();
    }
}

pub struct V8TaskExecutor {
    loader: Rc<dyn WorkflowRuntimeLoader>,
    payloads: Rc<dyn TaskPayloads>,
    payload_limits: TaskPayloadLimits,
}
impl std::fmt::Debug for V8TaskExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("V8TaskExecutor").finish_non_exhaustive()
    }
}
impl V8TaskExecutor {
    /// Construct a host with bounded task-scoped payload reads and writes.
    ///
    /// # Errors
    /// Rejects invalid payload limits.
    pub fn new(
        loader: Rc<dyn WorkflowRuntimeLoader>,
        payloads: Rc<dyn TaskPayloads>,
        payload_limits: TaskPayloadLimits,
    ) -> Result<Self, WorkflowServiceError> {
        payload_limits.validate()?;
        Ok(Self {
            loader,
            payloads,
            payload_limits,
        })
    }
}
impl TaskExecutor for V8TaskExecutor {
    fn start(
        &self,
        assignment: &TaskAssignment,
        budget: ExecutionBudget,
    ) -> Result<Box<dyn TaskExecution>, WorkflowServiceError> {
        Ok(Box::new(V8Execution {
            invocation: assignment.invocation.clone(),
            loader: (self.loader.clone(), assignment.clone()),
            loaded: None,
            cancel: CancelFlag::new(),
            started: false,
            budget,
            payloads: Rc::new(TaskPayloadReader::new(
                self.payloads.clone(),
                assignment,
                self.payload_limits.max_payload_bytes,
            )?),
            output: (self.payloads.clone(), self.payload_limits),
        }))
    }
}

struct V8Execution {
    invocation: WorkflowInvocation,
    loader: (Rc<dyn WorkflowRuntimeLoader>, TaskAssignment),
    loaded: Option<LoadedWorkflow>,
    cancel: CancelFlag,
    started: bool,
    budget: ExecutionBudget,
    payloads: Rc<TaskPayloadReader>,
    output: (Rc<dyn TaskPayloads>, TaskPayloadLimits),
}
#[async_trait(?Send)]
impl TaskExecution for V8Execution {
    async fn wait(&mut self) -> Result<WorkflowExecution, WorkflowServiceError> {
        if self.started || self.cancel.is_cancelled() {
            return Err(WorkflowServiceError::Conflict(
                "workflow execution is no longer available".into(),
            ));
        }
        self.started = true;
        self.budget.check()?;
        if let Some(input) = self.payloads.input().await? {
            self.invocation.trigger.input = Some(input);
            self.invocation.trigger.input_ref = None;
        }
        let (factory, assignment) = &self.loader;
        let executable = self.payloads.executable().await?;
        self.budget.check()?;
        self.loaded = Some(factory.load(assignment, &executable)?);
        let loaded = self.loaded.as_ref().expect("loaded workflow runtime");
        let interrupt = loaded.runtime.interrupt_handle();
        self.budget.on_interrupt(move || interrupt.cancel())?;
        self.budget.check()?;
        if loaded.runtime.app_id() != Some(self.payloads.app_id()) {
            return Err(WorkflowServiceError::InvalidRequest(
                "workflow loader returned another app's runtime".into(),
            ));
        }
        let interrupt = loaded.runtime.interrupt_handle();
        loaded.runtime.with_scope(|scope| {
            scope.set_slot(crate::v8_class::TaskOutputReader {
                reader: self.payloads.clone(),
                interrupt,
            });
        });
        let envelope = serde_json::to_string(&self.invocation)
            .map_err(|_| WorkflowServiceError::Internal("invalid workflow invocation".into()))?;
        loaded.runtime.start_pump();
        // Dispatch encodes a cached startup failure in the normal workflow outcome.
        let _ = loaded.runtime.initialize(&loaded.env).await;
        self.budget.check()?;
        self.payloads.check()?;
        loaded.runtime.enter_isolate();
        let outcome = loaded.runtime.call_workflow_dispatch(
            &envelope,
            &loaded.env,
            RequestCtx::new(self.cancel.clone()),
        );
        loaded.runtime.exit_isolate();
        let json = match outcome {
            WorkflowOutcome::Response { json, .. } => json,
            WorkflowOutcome::Pending { rx, .. } => {
                loaded.runtime.notify_pump();
                let mut received = std::pin::pin!(rx.recv());
                let mut failed = std::pin::pin!(self.payloads.failed());
                let result = std::future::poll_fn(|cx| {
                    if let Poll::Ready(error) = failed.as_mut().poll(cx) {
                        return Poll::Ready(Err(error));
                    }
                    received.as_mut().poll(cx).map(|result| {
                        result.map_err(|error| WorkflowServiceError::Unavailable(error.message))
                    })
                })
                .await;
                self.budget.check()?;
                self.payloads.check()?;
                result?.json
            }
        };
        self.budget.check()?;
        self.payloads.check()?;
        // Freeze app effects before any host upload can yield. The task lease
        // remains active while staging; app code has finished its frontier.
        self.stop().await;
        // Shutdown holds the frontier that app code already produced. Only a
        // revoked authority withdraws the right to hand it to the runner.
        self.budget.check_authority()?;
        let (transport, limits) = &self.output;
        let (_, assignment) = &self.loader;
        let prepared = PreparedExecution::from_runtime_json(assignment, &json, *limits)?;
        drop(json);
        loop {
            // An unstaged frontier is incomplete: its payload refs name bytes
            // that never landed, so expiry here must refuse it and re-execute.
            self.budget.check()?;
            match prepared.stage(transport.as_ref()).await {
                Err(WorkflowServiceError::Unavailable(_) | WorkflowServiceError::Timeout) => {
                    // Retain the prepared bytes and request IDs when the upload
                    // response is lost. The runner bounds retries by its lease
                    // and execution timeout without invoking app code again.
                    compio::time::sleep(Duration::from_millis(100)).await;
                }
                // The referenced bytes have landed. The frontier is complete,
                // and expiry no longer justifies throwing it away.
                result => {
                    self.budget.check_authority()?;
                    return result;
                }
            }
        }
    }

    fn cancel(&mut self) {
        self.cancel.cancel();
        if let Some(loaded) = &self.loaded {
            loaded.runtime.quarantine();
        }
    }

    async fn stop(&mut self) {
        self.cancel();
        if let Some(loaded) = &self.loaded {
            loaded.runtime.shutdown().await;
        }
        self.loaded = None;
    }
}
impl Drop for V8Execution {
    fn drop(&mut self) {
        self.cancel();
    }
}
