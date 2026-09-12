//! App-scoped service operations behind the native binding's Rust interface.

use super::{AppWorkflows, RequestId};
use crate::{
    backend::WorkflowBackend,
    operations::{
        DeliveredSignal, RestartOptions, RestartedRun, RunOperation, RunStatus, SignalOptions,
        StartOptions, StartedRun, TransitionedRun,
    },
    WorkflowServiceError,
};
use async_trait::async_trait;
use zeroship_core::app_id::AppId;

/// A binding-ready service client whose app identity is fixed by its host.
#[derive(Clone, Debug)]
pub struct AppBackend {
    api: AppWorkflows,
    max_output_bytes: usize,
}
impl AppBackend {
    fn new(api: AppWorkflows, max_output_bytes: usize) -> Result<Self, WorkflowServiceError> {
        if max_output_bytes == 0 {
            return Err(WorkflowServiceError::InvalidRequest(
                "workflow output read limit must be positive".into(),
            ));
        }
        Ok(Self {
            api,
            max_output_bytes,
        })
    }

    #[must_use]
    pub fn app_id(&self) -> &AppId {
        self.api.app_id()
    }
}
impl AppWorkflows {
    /// Bind the native operation interface with an explicit output memory limit.
    ///
    /// # Errors
    /// Rejects an empty read limit.
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
        let request = RequestId::mint();
        self.api.start(&request, &workflow_name, options).await
    }

    async fn status(&self, run_id: String) -> Result<RunStatus, WorkflowServiceError> {
        self.api.status(&run_id).await
    }

    async fn signal(
        &self,
        run_id: String,
        options: SignalOptions,
    ) -> Result<DeliveredSignal, WorkflowServiceError> {
        let request = RequestId::mint();
        self.api.signal(&request, &run_id, options).await
    }

    async fn transition(
        &self,
        run_id: String,
        op: RunOperation,
    ) -> Result<TransitionedRun, WorkflowServiceError> {
        let request = RequestId::mint();
        self.api.transition(&request, &run_id, op).await
    }

    async fn restart(
        &self,
        run_id: String,
        options: RestartOptions,
    ) -> Result<RestartedRun, WorkflowServiceError> {
        let request = RequestId::mint();
        self.api.restart(&request, &run_id, options).await
    }

    async fn read_step_output(
        &self,
        run_id: String,
        name: String,
        occurrence: u32,
    ) -> Result<Vec<u8>, WorkflowServiceError> {
        let read = self.api.read_step_output(&run_id, &name, occurrence).await?;
        read.into_bytes(self.max_output_bytes).await
    }
}
