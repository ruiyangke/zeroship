//! Control-plane client for the `env.workflows` native binding.
//!
//! This module owns the HTTP wire contract to the M2-1 workflow instance API.
//! The V8 classes validate JS arguments synchronously, then hand one of these
//! request objects to a spawned compio/cyper future.

use std::time::Duration;

use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use serde_json::{json, Value};
use zeroship_runtime::state::OpError;

const WORKFLOW_CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

const PATH_SEGMENT_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'/');

thread_local! {
    static WORKFLOW_CONTROL_CLIENT: cyper::Client = cyper::Client::new();
}

fn this_thread_client() -> cyper::Client {
    WORKFLOW_CONTROL_CLIENT.with(|c| c.clone())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowClientConfig {
    control_url: String,
    app_id: String,
    token: String,
}

impl WorkflowClientConfig {
    #[must_use]
    pub fn new(
        control_url: impl Into<String>,
        app_id: impl Into<String>,
        token: impl Into<String>,
    ) -> Self {
        Self {
            control_url: control_url.into().trim_end_matches('/').to_string(),
            app_id: app_id.into(),
            token: token.into(),
        }
    }

    #[must_use]
    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkflowHttpMethod {
    Get,
    Post,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowHttpRequest {
    pub method: WorkflowHttpMethod,
    pub url: String,
    pub authorization: String,
    pub app_id_header: String,
    pub body: Option<Vec<u8>>,
}

#[derive(Debug)]
pub enum WorkflowRpcError {
    InvalidRequest(String),
    Transport(String),
    Timeout,
    Http { status: u16, body: String },
    Decode(String),
}

impl WorkflowRpcError {
    #[must_use]
    pub fn to_op_error(&self) -> OpError {
        match self {
            Self::InvalidRequest(msg) => OpError::type_error(msg.clone()),
            Self::Transport(msg) => {
                OpError::coded("workflow_transport_error", msg.clone(), None::<String>)
            }
            Self::Timeout => OpError::coded(
                "workflow_timeout",
                format!(
                    "workflow control request timed out after {}s",
                    WORKFLOW_CONTROL_TIMEOUT.as_secs()
                ),
                None::<String>,
            ),
            Self::Http { status, body } => OpError::coded(
                "workflow_http_error",
                format!("workflow control request failed with HTTP {status}: {body}"),
                None::<String>,
            ),
            Self::Decode(msg) => {
                OpError::coded("workflow_decode_error", msg.clone(), None::<String>)
            }
        }
    }
}

fn path_segment(raw: &str) -> String {
    utf8_percent_encode(raw, PATH_SEGMENT_ENCODE_SET).to_string()
}

fn request(
    config: &WorkflowClientConfig,
    method: WorkflowHttpMethod,
    path: String,
    body: Option<Value>,
) -> Result<WorkflowHttpRequest, WorkflowRpcError> {
    if config.control_url.is_empty() {
        return Err(WorkflowRpcError::InvalidRequest(
            "workflows: control URL is not configured".to_string(),
        ));
    }
    if config.app_id.is_empty() {
        return Err(WorkflowRpcError::InvalidRequest(
            "workflows: app id is not configured".to_string(),
        ));
    }
    if config.token.is_empty() {
        return Err(WorkflowRpcError::InvalidRequest(
            "workflows: app-scoped control token is not configured".to_string(),
        ));
    }
    let body = match body {
        Some(value) => Some(
            serde_json::to_vec(&value)
                .map_err(|e| WorkflowRpcError::InvalidRequest(format!("serialize body: {e}")))?,
        ),
        None => None,
    };
    Ok(WorkflowHttpRequest {
        method,
        url: format!("{}{}", config.control_url, path),
        authorization: format!("Bearer {}", config.token),
        app_id_header: config.app_id.clone(),
        body,
    })
}

#[must_use]
pub fn app_scoped_token(control_key: &str, app_id: &str) -> String {
    zeroship_core::auth::derive_app_scoped_control_token(control_key, app_id)
}

pub fn build_start_request(
    config: &WorkflowClientConfig,
    workflow_name: &str,
    body: Value,
) -> Result<WorkflowHttpRequest, WorkflowRpcError> {
    request(
        config,
        WorkflowHttpMethod::Post,
        format!("/internal/workflows/{}/runs", path_segment(workflow_name)),
        Some(body),
    )
}

pub fn build_get_status_request(
    config: &WorkflowClientConfig,
    run_id: &str,
) -> Result<WorkflowHttpRequest, WorkflowRpcError> {
    request(
        config,
        WorkflowHttpMethod::Get,
        format!("/internal/workflows/runs/{}", path_segment(run_id)),
        None,
    )
}

pub fn build_signal_request(
    config: &WorkflowClientConfig,
    run_id: &str,
    body: Value,
) -> Result<WorkflowHttpRequest, WorkflowRpcError> {
    request(
        config,
        WorkflowHttpMethod::Post,
        format!("/internal/workflows/runs/{}/signal", path_segment(run_id)),
        Some(body),
    )
}

pub fn build_transition_request(
    config: &WorkflowClientConfig,
    run_id: &str,
    op: &'static str,
) -> Result<WorkflowHttpRequest, WorkflowRpcError> {
    request(
        config,
        WorkflowHttpMethod::Post,
        format!("/internal/workflows/runs/{}/{}", path_segment(run_id), op),
        Some(json!({})),
    )
}

pub fn build_restart_request(
    config: &WorkflowClientConfig,
    run_id: &str,
    body: Value,
) -> Result<WorkflowHttpRequest, WorkflowRpcError> {
    request(
        config,
        WorkflowHttpMethod::Post,
        format!("/internal/workflows/runs/{}/restart", path_segment(run_id)),
        Some(body),
    )
}

pub async fn execute_json(req: WorkflowHttpRequest) -> Result<Value, WorkflowRpcError> {
    let client = this_thread_client();
    let mut builder = match req.method {
        WorkflowHttpMethod::Get => client.get(&req.url),
        WorkflowHttpMethod::Post => client.post(&req.url),
    }
    .map_err(|e| {
        WorkflowRpcError::InvalidRequest(format!("invalid workflow control URL: {e}"))
    })?;

    builder = builder
        .header("authorization", &req.authorization)
        .map_err(|e| WorkflowRpcError::InvalidRequest(format!("invalid auth header: {e}")))?;
    builder = builder
        .header("x-zeroship-app-id", &req.app_id_header)
        .map_err(|e| WorkflowRpcError::InvalidRequest(format!("invalid app header: {e}")))?;
    if req.body.is_some() {
        builder = builder
            .header("content-type", "application/json")
            .map_err(|e| {
                WorkflowRpcError::InvalidRequest(format!("invalid content-type header: {e}"))
            })?;
    }

    let send = async {
        match req.body {
            Some(body) => builder.body(body).send().await,
            None => builder.send().await,
        }
    };
    let response = compio::time::timeout(WORKFLOW_CONTROL_TIMEOUT, send)
        .await
        .map_err(|_| WorkflowRpcError::Timeout)?
        .map_err(|e| WorkflowRpcError::Transport(e.to_string()))?;

    let status = response.status().as_u16();
    let bytes = response
        .bytes()
        .await
        .map_err(|e| WorkflowRpcError::Transport(format!("read workflow response body: {e}")))?;
    let body = String::from_utf8_lossy(&bytes).to_string();
    if !(200..300).contains(&status) {
        return Err(WorkflowRpcError::Http { status, body });
    }
    serde_json::from_str(&body).map_err(|e| {
        WorkflowRpcError::Decode(format!("decode workflow response JSON: {e}; body={body}"))
    })
}
