//! V8 lifecycle adapter for the shared workflow runner.

use async_trait::async_trait;
use std::{cell::RefCell, future::Future, rc::Rc, task::Poll, time::Duration};
use zeroship_bundle::LoadedWorker;
use zeroship_runtime::{
    CancelFlag, EnvSnapshot, RequestCtx, Runtime, WorkflowOutcome,
    runtime::{COMMIT_FAILED_INDETERMINATE, DispatchError},
};
use zeroship_workflow_runner::{
    ExecutionBudget, PreparedExecution, TaskExecution, TaskExecutor, TaskPayloadLimits,
    TaskPayloadReader, TaskPayloads,
};
use zeroship_workflow::{
    service::{delivery::PayloadConfirmation, TaskAssignment},
    WorkflowExecution, WorkflowServiceError,
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
            owed: RefCell::default(),
        }))
    }
}

/// Turn the dispatch's own error into the execution's outcome.
///
/// A commit the request asked for may have been in flight at the stop, so its
/// wire code (`error.code`) is kept, not folded into a generic unavailability:
/// unlike every other dispatch failure here, re-running the task could
/// re-apply a commit that already landed. `Internal` is never retried by the
/// delivery loop (`zeroship_workflow_runner::delivery::retryable`), where
/// `Unavailable` is.
fn dispatch_wait_error(error: DispatchError) -> WorkflowServiceError {
    match error.code {
        Some(code) if code == COMMIT_FAILED_INDETERMINATE => {
            WorkflowServiceError::Internal(format!("{code}: {}", error.message))
        }
        _ => WorkflowServiceError::Unavailable(error.message),
    }
}

struct V8Execution {
    loader: (Rc<dyn WorkflowRuntimeLoader>, TaskAssignment),
    loaded: Option<LoadedWorkflow>,
    cancel: CancelFlag,
    started: bool,
    budget: ExecutionBudget,
    payloads: Rc<TaskPayloadReader>,
    output: (Rc<dyn TaskPayloads>, TaskPayloadLimits),
    owed: RefCell<Vec<PayloadConfirmation>>,
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
        // Built before any isolate exists, so a spliced child output this host
        // cannot read stops the task before creator code runs. The reader
        // hydrates the trigger's referenced input as it builds the envelope.
        let envelope = self.payloads.replay_envelope(self.output.1).await?;
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
                    received.as_mut().poll(cx).map(|result| result.map_err(dispatch_wait_error))
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
        // Shutdown holds the frontier that app code already produced, and expiry
        // does not withdraw the right to hand it to the runner: the journal fences
        // the settlement against the task lease.
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
                Ok((execution, owed)) => {
                    // Held rather than returned, because `TaskExecution::wait`
                    // answers with the frontier alone: what the settlement owes
                    // is not part of the frontier and must not travel inside it.
                    *self.owed.borrow_mut() = owed;
                    return Ok(execution);
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn owed_confirmations(&self) -> Vec<PayloadConfirmation> {
        self.owed.borrow().clone()
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

#[cfg(test)]
mod dispatch_wait_error_tests {
    use super::{COMMIT_FAILED_INDETERMINATE, DispatchError, WorkflowServiceError, dispatch_wait_error};

    /// The guard this test fails without: a commit left in flight at the stop
    /// must not come back as the retried-by-default `Unavailable` - the
    /// delivery loop would re-run the task over a commit that may have
    /// already landed.
    #[test]
    fn an_indeterminate_commit_is_reported_opaquely_and_is_not_retried() {
        let error = DispatchError::coded(
            "CPU time limit exceeded; a transaction commit was in flight, so whether \
             it applied is unknown",
            500,
            COMMIT_FAILED_INDETERMINATE,
        );
        let mapped = dispatch_wait_error(error);
        assert!(
            matches!(mapped, WorkflowServiceError::Internal(ref message) if message.contains(COMMIT_FAILED_INDETERMINATE)),
            "the code must survive in the execution's error, not be dropped: {mapped:?}"
        );
    }

    /// The control beside it: a dispatch error with no code - a plain stop or
    /// host cancellation - keeps its retryable classification.
    #[test]
    fn an_uncoded_dispatch_error_stays_a_retryable_unavailability() {
        let error = DispatchError::new("request timed out", 504);
        assert_eq!(
            dispatch_wait_error(error),
            WorkflowServiceError::Unavailable("request timed out".to_string())
        );
    }
}
