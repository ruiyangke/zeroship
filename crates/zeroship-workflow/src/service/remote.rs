use super::{
    capability::{CapabilityToken, SignalTarget, WORKFLOW_AUDIENCE},
    wire::{CompleteTask, Failure, Mutation, TaskCredential},
    AcceptedBroadcast, CompletionReceipt, Heartbeat, RequestId, RevokedSignals, SignalTokenRequest,
    TaskAssignment, TaskToken,
};
use crate::{
    operations::{
        DeliveredSignal, RestartOptions, RestartedRun, RunOperation, RunStatus, SignalOptions,
        StartOptions, StartedRun, TransitionedRun,
    },
    WorkflowExecution, WorkflowServiceError,
};
use futures::StreamExt;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde::{de::DeserializeOwned, Serialize};
use std::{sync::Arc, time::Duration};
use zeroship_core::{app_id::AppId, service_assertion::ServiceIssuer, service_peers::ServiceAuth};

thread_local! {
    static CLIENT: cyper::Client = cyper::Client::new();
}

#[derive(Debug, Clone)]
pub struct WorkflowEndpoint {
    base: String,
    timeout: Duration,
    max_response_bytes: usize,
}
impl WorkflowEndpoint {
    pub fn new(base: &str) -> Result<Self, WorkflowServiceError> {
        let url = url::Url::parse(base).map_err(|_| {
            WorkflowServiceError::InvalidRequest("invalid workflow service URL".into())
        })?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(WorkflowServiceError::InvalidRequest("workflow service URL requires an HTTP origin without credentials, query or fragment".into()));
        }
        Ok(Self {
            base: url.as_str().trim_end_matches('/').into(),
            timeout: Duration::from_secs(30),
            max_response_bytes: 32 * 1024 * 1024,
        })
    }
    pub fn with_limits(
        mut self,
        timeout: Duration,
        max_response_bytes: usize,
    ) -> Result<Self, WorkflowServiceError> {
        if timeout.is_zero() || max_response_bytes == 0 {
            return Err(WorkflowServiceError::InvalidRequest(
                "workflow transport limits must be positive".into(),
            ));
        }
        self.timeout = timeout;
        self.max_response_bytes = max_response_bytes;
        Ok(self)
    }
    #[must_use]
    pub fn for_app(&self, app: AppId, token: CapabilityToken) -> RemoteAppWorkflows {
        RemoteAppWorkflows {
            endpoint: self.clone(),
            app,
            token,
        }
    }
    #[must_use]
    pub fn tasks(&self, identity: Arc<ServiceAuth>) -> RemoteTasks {
        RemoteTasks {
            endpoint: self.clone(),
            identity,
        }
    }
    async fn request<T: DeserializeOwned>(
        &self,
        path: &str,
        authorization: &str,
        body: Option<serde_json::Value>,
    ) -> Result<T, WorkflowServiceError> {
        let client = CLIENT.with(Clone::clone);
        let url = format!("{}{path}", self.base);
        let builder = if body.is_some() {
            client.post(&url)
        } else {
            client.get(&url)
        }
        .map_err(|_| WorkflowServiceError::InvalidRequest("invalid workflow request URL".into()))?
        .header("authorization", authorization)
        .map_err(|_| WorkflowServiceError::Unauthenticated)?;
        let builder = if let Some(body) = body {
            let body = serde_json::to_vec(&body).map_err(|_| {
                WorkflowServiceError::InvalidRequest("invalid workflow request body".into())
            })?;
            builder
                .header("content-type", "application/json")
                .map_err(|_| {
                    WorkflowServiceError::Internal("invalid workflow content type".into())
                })?
                .body(body)
        } else {
            builder
        };
        compio::time::timeout(self.timeout, async {
            // An ambiguous response is returned to the caller. It may retry
            // with the same request identity; the transport never invents one.
            let mut response = builder.send().await.map_err(|_| {
                WorkflowServiceError::Unavailable("workflow service unreachable".into())
            })?;
            let status = response.status();
            let mut bytes = Vec::new();
            while let Some(chunk) = response.next().await {
                let chunk = chunk.map_err(|_| {
                    WorkflowServiceError::Unavailable("workflow response interrupted".into())
                })?;
                if bytes
                    .len()
                    .checked_add(chunk.len())
                    .is_none_or(|size| size > self.max_response_bytes)
                {
                    return Err(WorkflowServiceError::PayloadTooLarge);
                }
                bytes.extend_from_slice(&chunk);
            }
            if !status.is_success() {
                return Err(serde_json::from_slice::<Failure>(&bytes)
                    .map(Failure::into_error)
                    .unwrap_or_else(|_| {
                        WorkflowServiceError::Unavailable("invalid workflow error response".into())
                    }));
            }
            serde_json::from_slice(&bytes)
                .map_err(|_| WorkflowServiceError::Unavailable("invalid workflow response".into()))
        })
        .await
        .map_err(|_| WorkflowServiceError::Timeout)?
    }
}

#[derive(Debug, Clone)]
pub struct RemoteAppWorkflows {
    endpoint: WorkflowEndpoint,
    app: AppId,
    token: CapabilityToken,
}
impl RemoteAppWorkflows {
    #[must_use]
    pub fn app_id(&self) -> &AppId {
        &self.app
    }
    pub async fn start(
        &self,
        request: &RequestId,
        workflow: &str,
        options: StartOptions,
    ) -> Result<StartedRun, WorkflowServiceError> {
        self.mutate(
            &format!("workflows/{}/runs", segment(workflow)),
            request,
            options,
        )
        .await
    }
    pub async fn status(&self, run_id: &str) -> Result<RunStatus, WorkflowServiceError> {
        self.endpoint
            .request(
                &self.path(&format!("workflow-runs/{}", segment(run_id))),
                &self.authorization(),
                None,
            )
            .await
    }
    pub async fn signal(
        &self,
        request: &RequestId,
        run_id: &str,
        options: SignalOptions,
    ) -> Result<DeliveredSignal, WorkflowServiceError> {
        self.mutate(
            &format!("workflow-runs/{}/signals", segment(run_id)),
            request,
            options,
        )
        .await
    }
    pub async fn transition(
        &self,
        request: &RequestId,
        run_id: &str,
        operation: RunOperation,
    ) -> Result<TransitionedRun, WorkflowServiceError> {
        self.mutate(
            &format!("workflow-runs/{}/transition", segment(run_id)),
            request,
            operation,
        )
        .await
    }
    pub async fn restart(
        &self,
        request: &RequestId,
        run_id: &str,
        options: RestartOptions,
    ) -> Result<RestartedRun, WorkflowServiceError> {
        self.mutate(
            &format!("workflow-runs/{}/restart", segment(run_id)),
            request,
            options,
        )
        .await
    }
    pub async fn broadcast(
        &self,
        request: &RequestId,
        topic: &str,
        options: SignalOptions,
    ) -> Result<AcceptedBroadcast, WorkflowServiceError> {
        self.mutate(
            &format!("workflow-topics/{}", segment(topic)),
            request,
            options,
        )
        .await
    }
    pub async fn issue_signal_token(
        &self,
        request: &RequestId,
        options: SignalTokenRequest,
    ) -> Result<CapabilityToken, WorkflowServiceError> {
        self.mutate("workflow-signal-tokens", request, options)
            .await
    }
    pub async fn revoke_signal_tokens(
        &self,
        request: &RequestId,
        target: Option<SignalTarget>,
    ) -> Result<RevokedSignals, WorkflowServiceError> {
        self.mutate("workflow-signal-tokens/revoke", request, target)
            .await
    }
    async fn mutate<T: Serialize, R: DeserializeOwned>(
        &self,
        suffix: &str,
        request: &RequestId,
        options: T,
    ) -> Result<R, WorkflowServiceError> {
        let body = value(Mutation {
            request_id: request.clone(),
            options,
        })?;
        self.endpoint
            .request(&self.path(suffix), &self.authorization(), Some(body))
            .await
    }
    fn path(&self, suffix: &str) -> String {
        format!("/v1/apps/{}/{suffix}", self.app.as_str())
    }
    fn authorization(&self) -> String {
        format!("Bearer {}", self.token.as_str())
    }
}

#[derive(Clone)]
pub struct RemoteTasks {
    endpoint: WorkflowEndpoint,
    identity: Arc<ServiceAuth>,
}
impl std::fmt::Debug for RemoteTasks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteTasks")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}
impl RemoteTasks {
    pub async fn poll(&self) -> Result<Option<TaskAssignment>, WorkflowServiceError> {
        self.endpoint
            .request(
                "/v1/tasks/poll",
                &self.authorization()?,
                Some(serde_json::json!({})),
            )
            .await
    }
    pub async fn heartbeat(
        &self,
        task_id: &str,
        token: &TaskToken,
    ) -> Result<Heartbeat, WorkflowServiceError> {
        self.task_request(
            task_id,
            "heartbeat",
            TaskCredential {
                token: token.clone(),
            },
        )
        .await
    }
    pub async fn complete(
        &self,
        task_id: &str,
        token: &TaskToken,
        execution: WorkflowExecution,
    ) -> Result<CompletionReceipt, WorkflowServiceError> {
        self.task_request(
            task_id,
            "complete",
            CompleteTask {
                token: token.clone(),
                execution,
            },
        )
        .await
    }
    pub async fn release(
        &self,
        task_id: &str,
        token: &TaskToken,
    ) -> Result<(), WorkflowServiceError> {
        self.task_request(
            task_id,
            "release",
            TaskCredential {
                token: token.clone(),
            },
        )
        .await
    }
    async fn task_request<T: Serialize, R: DeserializeOwned>(
        &self,
        task: &str,
        action: &str,
        body: T,
    ) -> Result<R, WorkflowServiceError> {
        self.endpoint
            .request(
                &format!("/v1/tasks/{}/{action}", segment(task)),
                &self.authorization()?,
                Some(value(body)?),
            )
            .await
    }
    fn authorization(&self) -> Result<String, WorkflowServiceError> {
        let audience = ServiceIssuer::parse(WORKFLOW_AUDIENCE)
            .map_err(|_| WorkflowServiceError::Internal("invalid workflow audience".into()))?;
        self.identity
            .authorization_for(&audience)
            .ok_or(WorkflowServiceError::Unauthenticated)
    }
}
fn value<T: Serialize>(value: T) -> Result<serde_json::Value, WorkflowServiceError> {
    serde_json::to_value(value)
        .map_err(|_| WorkflowServiceError::InvalidRequest("invalid workflow request".into()))
}
fn segment(value: &str) -> String {
    utf8_percent_encode(value, NON_ALPHANUMERIC).to_string()
}
