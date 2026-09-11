//! Control-plane client for the `env.workflows` native binding.
//!
//! This module owns the HTTP wire contract to the M2-1 workflow instance API.
//! The V8 classes validate JS arguments synchronously, then hand one of these
//! request objects to a spawned compio/cyper future.

use std::time::Duration;

use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};

use crate::errors::WorkflowServiceError;
use crate::operations::{RestartOptions, RunOperation, SignalOptions, StartOptions};

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
    .add(b'\\')
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

fn path_segment(raw: &str) -> String {
    utf8_percent_encode(raw, PATH_SEGMENT_ENCODE_SET).to_string()
}

fn request(
    config: &WorkflowClientConfig,
    method: WorkflowHttpMethod,
    path: String,
    body: Option<Value>,
) -> Result<WorkflowHttpRequest, WorkflowServiceError> {
    if config.control_url.is_empty() {
        return Err(WorkflowServiceError::InvalidRequest(
            "workflows: control URL is not configured".to_string(),
        ));
    }
    if config.app_id.is_empty() {
        return Err(WorkflowServiceError::InvalidRequest(
            "workflows: app id is not configured".to_string(),
        ));
    }
    if config.token.is_empty() {
        return Err(WorkflowServiceError::InvalidRequest(
            "workflows: app-scoped control token is not configured".to_string(),
        ));
    }
    let body =
        match body {
            Some(value) => Some(serde_json::to_vec(&value).map_err(|e| {
                WorkflowServiceError::InvalidRequest(format!("serialize body: {e}"))
            })?),
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
    options: StartOptions,
) -> Result<WorkflowHttpRequest, WorkflowServiceError> {
    request(
        config,
        WorkflowHttpMethod::Post,
        format!("/internal/workflows/{}/runs", path_segment(workflow_name)),
        Some(encode_options(options)?),
    )
}

pub fn build_get_status_request(
    config: &WorkflowClientConfig,
    run_id: &str,
) -> Result<WorkflowHttpRequest, WorkflowServiceError> {
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
    options: SignalOptions,
) -> Result<WorkflowHttpRequest, WorkflowServiceError> {
    request(
        config,
        WorkflowHttpMethod::Post,
        format!("/internal/workflows/runs/{}/signal", path_segment(run_id)),
        Some(encode_options(options)?),
    )
}

pub fn build_transition_request(
    config: &WorkflowClientConfig,
    run_id: &str,
    op: RunOperation,
) -> Result<WorkflowHttpRequest, WorkflowServiceError> {
    request(
        config,
        WorkflowHttpMethod::Post,
        format!(
            "/internal/workflows/runs/{}/{}",
            path_segment(run_id),
            op.as_str()
        ),
        Some(json!({})),
    )
}

pub fn build_restart_request(
    config: &WorkflowClientConfig,
    run_id: &str,
    options: RestartOptions,
) -> Result<WorkflowHttpRequest, WorkflowServiceError> {
    request(
        config,
        WorkflowHttpMethod::Post,
        format!("/internal/workflows/runs/{}/restart", path_segment(run_id)),
        Some(encode_options(options)?),
    )
}

pub fn build_read_step_output_request(
    config: &WorkflowClientConfig,
    run_id: &str,
    name: &str,
    occurrence: u32,
) -> Result<WorkflowHttpRequest, WorkflowServiceError> {
    if matches!(run_id, "" | "." | "..") || matches!(name, "" | "." | "..") {
        return Err(WorkflowServiceError::InvalidRequest(
            "workflow output requires a run id and step name".into(),
        ));
    }
    request(
        config,
        WorkflowHttpMethod::Get,
        format!(
            "/internal/workflows/runs/{}/steps/{}/output?occurrence={occurrence}",
            path_segment(run_id),
            path_segment(name),
        ),
        None,
    )
}

fn encode_options(options: impl Serialize) -> Result<Value, WorkflowServiceError> {
    serde_json::to_value(options)
        .map_err(|error| WorkflowServiceError::InvalidRequest(error.to_string()))
}

pub async fn execute_json<T: DeserializeOwned>(
    req: WorkflowHttpRequest,
) -> Result<T, WorkflowServiceError> {
    let bytes = execute_bytes(req).await?;
    serde_json::from_slice(&bytes)
        .map_err(|e| WorkflowServiceError::Internal(format!("decode workflow response JSON: {e}")))
}

pub async fn execute_bytes(req: WorkflowHttpRequest) -> Result<Vec<u8>, WorkflowServiceError> {
    let client = this_thread_client();
    let mut builder = match req.method {
        WorkflowHttpMethod::Get => client.get(&req.url),
        WorkflowHttpMethod::Post => client.post(&req.url),
    }
    .map_err(|e| {
        WorkflowServiceError::InvalidRequest(format!("invalid workflow control URL: {e}"))
    })?;

    builder = builder
        .header("authorization", &req.authorization)
        .map_err(|e| WorkflowServiceError::InvalidRequest(format!("invalid auth header: {e}")))?;
    builder = builder
        .header("x-zeroship-app-id", &req.app_id_header)
        .map_err(|e| WorkflowServiceError::InvalidRequest(format!("invalid app header: {e}")))?;
    if req.body.is_some() {
        builder = builder
            .header("content-type", "application/json")
            .map_err(|e| {
                WorkflowServiceError::InvalidRequest(format!("invalid content-type header: {e}"))
            })?;
    }

    let exchange = async {
        let response = match req.body {
            Some(body) => builder.body(body).send().await,
            None => builder.send().await,
        }
        .map_err(|e| WorkflowServiceError::Unavailable(e.to_string()))?;

        let status = response.status().as_u16();
        let bytes = response.bytes().await.map_err(|e| {
            WorkflowServiceError::Unavailable(format!("read workflow response body: {e}"))
        })?;
        if !(200..300).contains(&status) {
            return Err(response_error(status, &bytes));
        }
        Ok(bytes.to_vec())
    };
    compio::time::timeout(WORKFLOW_CONTROL_TIMEOUT, exchange)
        .await
        .map_err(|_| WorkflowServiceError::Timeout)?
}

fn response_error(status: u16, body: &[u8]) -> WorkflowServiceError {
    // Only structured client-error messages may cross the adapter. Proxy HTML,
    // database failures and arbitrary server bodies are not domain messages.
    let message = || {
        let value: Value = serde_json::from_slice(body).ok()?;
        value
            .get("message")
            .or_else(|| value.get("error"))?
            .as_str()
            .map(str::to_owned)
    };
    match status {
        400 | 422 => WorkflowServiceError::InvalidRequest(
            message().unwrap_or_else(|| "invalid workflow request".into()),
        ),
        401 => WorkflowServiceError::Unauthenticated,
        403 => WorkflowServiceError::PermissionDenied,
        404 => WorkflowServiceError::NotFound("workflow resource not found".into()),
        409 => WorkflowServiceError::Conflict(
            message().unwrap_or_else(|| "workflow operation conflicts with current state".into()),
        ),
        413 => WorkflowServiceError::PayloadTooLarge,
        429 => WorkflowServiceError::ResourceExhausted(
            message().unwrap_or_else(|| "workflow capacity is exhausted".into()),
        ),
        408 | 504 => WorkflowServiceError::Timeout,
        500..=599 => WorkflowServiceError::Unavailable("workflow service is unavailable".into()),
        _ => {
            WorkflowServiceError::Internal(format!("unexpected workflow response status: {status}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_failures_have_domain_types() {
        assert!(matches!(
            response_error(401, b""),
            WorkflowServiceError::Unauthenticated
        ));
        assert!(matches!(
            response_error(403, b""),
            WorkflowServiceError::PermissionDenied
        ));
        assert!(matches!(
            response_error(404, b""),
            WorkflowServiceError::NotFound(_)
        ));
        assert_eq!(
            response_error(
                409,
                br#"{"error":"RunConflict","message":"key already exists"}"#
            ),
            WorkflowServiceError::Conflict("key already exists".into())
        );
        assert!(matches!(
            response_error(429, b""),
            WorkflowServiceError::ResourceExhausted(_)
        ));
    }

    #[test]
    fn server_details_and_non_json_proxy_bodies_are_not_exposed() {
        let secret = b"database password=sensitive";
        assert!(!response_error(500, secret)
            .to_string()
            .contains("sensitive"));
        assert!(!response_error(409, secret)
            .to_string()
            .contains("sensitive"));
        assert_eq!(response_error(404, secret), response_error(404, b""));
    }
}
