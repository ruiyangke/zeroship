//! Backend seam for the `env.workflows` native binding.
//!
//! The customer engine implements it in-process; the local dev tier plugs its
//! own backend into the same V8 classes.

use std::sync::Arc;

use crate::errors::WorkflowServiceError;
use crate::operations::{
    DeliveredSignal, RestartOptions, RestartedRun, RunOperation, RunStatus, SignalOptions,
    StartOptions, StartedRun, TransitionedRun,
};
use async_trait::async_trait;

#[async_trait(?Send)]
pub trait WorkflowBackend: Send + Sync + std::fmt::Debug {
    async fn start(
        &self,
        workflow_name: String,
        options: StartOptions,
    ) -> Result<StartedRun, WorkflowServiceError>;

    async fn status(&self, run_id: String) -> Result<RunStatus, WorkflowServiceError>;

    async fn signal(
        &self,
        run_id: String,
        options: SignalOptions,
    ) -> Result<DeliveredSignal, WorkflowServiceError>;

    async fn transition(
        &self,
        run_id: String,
        op: RunOperation,
    ) -> Result<TransitionedRun, WorkflowServiceError>;

    async fn restart(
        &self,
        run_id: String,
        options: RestartOptions,
    ) -> Result<RestartedRun, WorkflowServiceError>;

    async fn read_step_output(
        &self,
        run_id: String,
        name: String,
        occurrence: u32,
    ) -> Result<Vec<u8>, WorkflowServiceError>;
}

pub type SharedWorkflowBackend = Arc<dyn WorkflowBackend>;
