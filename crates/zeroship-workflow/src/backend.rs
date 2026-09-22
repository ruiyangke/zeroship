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

    async fn read_output(&self, run_id: String) -> Result<Vec<u8>, WorkflowServiceError>;
}

/// Resolves a completed step's recorded output to bytes.
///
/// This crate records which payload a step owns and proves that ownership; it
/// moves no payload bytes and holds no object store. The host that owns the
/// store supplies this, and `api` arrives already carrying the policy
/// generation the call is bound to.
#[async_trait(?Send)]
pub trait StepOutputReader: Send + Sync + std::fmt::Debug {
    async fn read(
        &self,
        api: &crate::service::AppWorkflows,
        run_id: &str,
        name: &str,
        occurrence: u32,
    ) -> Result<Vec<u8>, WorkflowServiceError>;

    /// Resolve the run's final output to bytes, under the same read budget.
    ///
    /// # Errors
    /// Reports a run whose output no object holds, and object failures.
    async fn read_output(
        &self,
        api: &crate::service::AppWorkflows,
        run_id: &str,
    ) -> Result<Vec<u8>, WorkflowServiceError>;
}

pub type SharedStepOutputs = Arc<dyn StepOutputReader>;

pub type SharedWorkflowBackend = Arc<dyn WorkflowBackend>;
