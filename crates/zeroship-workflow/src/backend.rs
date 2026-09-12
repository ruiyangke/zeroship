//! Backend seam for the `env.workflows` native binding.
//!
//! Production keeps using the HTTP control-plane backend. The local dev tier
//! plugs an in-process backend into the same V8 classes.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::client::{
    build_get_status_request, build_read_step_output_request, build_restart_request,
    build_signal_request, build_start_request, build_transition_request, execute_bytes,
    execute_json, WorkflowClientConfig, WorkflowRpcError,
};

#[async_trait(?Send)]
pub trait WorkflowBackend: Send + Sync + std::fmt::Debug {
    async fn start(&self, workflow_name: String, body: Value) -> Result<Value, WorkflowRpcError>;

    async fn status(&self, run_id: String) -> Result<Value, WorkflowRpcError>;

    async fn signal(&self, run_id: String, body: Value) -> Result<Value, WorkflowRpcError>;

    async fn transition(&self, run_id: String, op: &'static str)
        -> Result<Value, WorkflowRpcError>;

    async fn restart(&self, run_id: String, body: Value) -> Result<Value, WorkflowRpcError>;

    async fn read_step_output(
        &self,
        run_id: String,
        name: String,
        occurrence: u32,
    ) -> Result<Vec<u8>, WorkflowRpcError>;
}

pub type SharedWorkflowBackend = Arc<dyn WorkflowBackend>;

#[derive(Clone, Debug)]
pub struct HttpWorkflowBackend {
    client: WorkflowClientConfig,
}

impl HttpWorkflowBackend {
    #[must_use]
    pub fn new(client: WorkflowClientConfig) -> Self {
        Self { client }
    }
}

#[async_trait(?Send)]
impl WorkflowBackend for HttpWorkflowBackend {
    async fn start(&self, workflow_name: String, body: Value) -> Result<Value, WorkflowRpcError> {
        execute_json(build_start_request(&self.client, &workflow_name, body)?).await
    }

    async fn status(&self, run_id: String) -> Result<Value, WorkflowRpcError> {
        execute_json(build_get_status_request(&self.client, &run_id)?).await
    }

    async fn signal(&self, run_id: String, body: Value) -> Result<Value, WorkflowRpcError> {
        execute_json(build_signal_request(&self.client, &run_id, body)?).await
    }

    async fn transition(
        &self,
        run_id: String,
        op: &'static str,
    ) -> Result<Value, WorkflowRpcError> {
        execute_json(build_transition_request(&self.client, &run_id, op)?).await
    }

    async fn restart(&self, run_id: String, body: Value) -> Result<Value, WorkflowRpcError> {
        execute_json(build_restart_request(&self.client, &run_id, body)?).await
    }

    async fn read_step_output(
        &self,
        run_id: String,
        name: String,
        occurrence: u32,
    ) -> Result<Vec<u8>, WorkflowRpcError> {
        execute_bytes(build_read_step_output_request(
            &self.client,
            &run_id,
            &name,
            occurrence,
        )?)
        .await
    }
}
