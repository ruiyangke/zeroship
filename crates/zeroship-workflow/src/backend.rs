//! Backend seam for the `env.workflows` native binding.
//!
//! The customer engine implements it in-process; the local dev tier plugs its
//! own backend into the same V8 classes.

use std::sync::Arc;

use crate::engine::WorkflowOutputRef;
use crate::errors::WorkflowServiceError;
use crate::operations::{
    DeliveredSignal, RestartOptions, RestartedRun, RunOperation, RunStatus, SignalOptions,
    StartOptions, StartedRun, TransitionedRun,
};
use async_trait::async_trait;
use serde_json::Value;

#[async_trait(?Send)]
pub trait WorkflowBackend: Send + Sync + std::fmt::Debug {
    /// Start a run of `workflow_name` from the value `input` carries.
    ///
    /// The value is the caller's. A generation row keeps no inline slot for a
    /// run's input, so the host stages it and the run names the object; that is
    /// why the caller supplies a value here and never a descriptor.
    async fn start(
        &self,
        workflow_name: String,
        input: Value,
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

/// Turns a value the platform accepted into the payload object a run starts
/// from.
///
/// A generation row keeps no inline slot for a run's input, so every host that
/// takes a start value stages it before any run can name it. This crate records
/// which payload a run owns and proves that ownership; it moves no payload
/// bytes and holds no object store, so the host that owns the store supplies
/// this, and `api` arrives already carrying the policy generation the call is
/// bound to.
#[async_trait(?Send)]
pub trait InputStager: Send + Sync + std::fmt::Debug {
    /// Stage `input` for `api`'s app and name the object it became.
    ///
    /// `request` is the idempotency key of the operation the input belongs to:
    /// staging deduplicates on it, so an operation retried after an uncommitted
    /// attempt restages to the object that attempt already wrote.
    ///
    /// # Errors
    /// Reports withdrawn admission, an input over the app's payload bound, an
    /// exhausted payload quota and store failures.
    async fn stage_input(
        &self,
        api: &crate::service::AppWorkflows,
        request: &crate::service::RequestId,
        input: &Value,
    ) -> Result<WorkflowOutputRef, WorkflowServiceError>;
}

pub type SharedInputStager = Arc<dyn InputStager>;

pub type SharedWorkflowBackend = Arc<dyn WorkflowBackend>;
