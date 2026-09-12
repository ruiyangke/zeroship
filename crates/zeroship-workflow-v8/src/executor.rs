//! V8 lifecycle adapter for the shared workflow runner.

use async_trait::async_trait;
use std::rc::Rc;
use zeroship_runtime::{CancelFlag, EnvSnapshot, RequestCtx, Runtime, WorkflowOutcome};
use zeroship_workflow::{
    service::{
        runner::{TaskExecution, TaskExecutor},
        TaskAssignment,
    },
    WorkflowExecution, WorkflowInvocation, WorkflowServiceError,
};

/// A trusted host loads the assignment's immutable deployment and app context.
#[async_trait(?Send)]
pub trait WorkflowRuntimeLoader {
    async fn load(
        &self,
        assignment: &TaskAssignment,
    ) -> Result<LoadedWorkflow, WorkflowServiceError>;
}

/// Owns an exclusive workflow isolate. Its pump must not have been started and
/// the runtime must be newly built, with its initial isolate entry still active.
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
        // Restore the enclosing host isolate before the loader can yield.
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
}
impl std::fmt::Debug for V8TaskExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("V8TaskExecutor").finish_non_exhaustive()
    }
}
impl V8TaskExecutor {
    #[must_use]
    pub fn new(loader: Rc<dyn WorkflowRuntimeLoader>) -> Self {
        Self { loader }
    }
}
impl TaskExecutor for V8TaskExecutor {
    fn start(
        &self,
        assignment: &TaskAssignment,
    ) -> Result<Box<dyn TaskExecution>, WorkflowServiceError> {
        Ok(Box::new(V8Execution {
            invocation: assignment.invocation.clone(),
            loader: Some((self.loader.clone(), assignment.clone())),
            loaded: None,
            cancel: CancelFlag::new(),
            started: false,
        }))
    }
}

pub(crate) struct V8Execution {
    invocation: WorkflowInvocation,
    loader: Option<(Rc<dyn WorkflowRuntimeLoader>, TaskAssignment)>,
    loaded: Option<LoadedWorkflow>,
    cancel: CancelFlag,
    started: bool,
}
impl V8Execution {
    pub(crate) fn loaded(invocation: WorkflowInvocation, loaded: LoadedWorkflow) -> Self {
        Self {
            invocation,
            loader: None,
            loaded: Some(loaded),
            cancel: CancelFlag::new(),
            started: false,
        }
    }
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
        if self.loaded.is_none() {
            let (loader, assignment) = self.loader.as_ref().ok_or_else(|| {
                WorkflowServiceError::Internal("workflow runtime loader is absent".into())
            })?;
            self.loaded = Some(loader.load(assignment).await?);
        }
        let loaded = self.loaded.as_ref().expect("loaded workflow runtime");
        let envelope = serde_json::to_string(&self.invocation)
            .map_err(|_| WorkflowServiceError::Internal("invalid workflow invocation".into()))?;
        loaded.runtime.start_pump();
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
                rx.recv()
                    .await
                    .map_err(|error| WorkflowServiceError::Unavailable(error.message))?
                    .json
            }
        };
        WorkflowExecution::from_runtime_json(&json)
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
