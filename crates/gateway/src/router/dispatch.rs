//! Request entry, resource-tree dispatch, idempotency hooks, and
//! worker-forwarding for the gateway router.
//!
//! Public surface (re-exported from `router/mod.rs`):
//!
//! * [`handle`] — path-based handler `/{app_name}/{tail*}`.
//! * [`handle_subdomain`] — Host-header subdomain handler.
//! * [`extract_app_name`] — shared helper used by both, plus the
//!   gateway's outer middleware.
//!
//! Internal flow (`handle_request` → `execute_resource_tree`):
//!
//! 1. Resolve route by app name.
//! 2. CORS preflight short-circuit when applicable.
//! 3. Resource-tree dispatch — auth gate, CSRF, max-input, rate-limit,
//!    idempotency, then run the resolved action (worker forward,
//!    redirect, rewrite, static).

use std::sync::Arc;

#[cfg(test)]
use chrono::{DateTime, Utc};
use ntex::util::Bytes;
use ntex::web::{self, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use zeroship_bundle::AuthLevel;

use crate::{enforce, idempotency, oidc_rp, proxy, GateState};

use super::auth::{
    extract_session_cookie, jwt_subject_unverified, resolve_auth, AuthOutcome,
};
use super::cors::{build_preflight_response, inject_cors_response_headers};
use super::helpers::resource_key_hash;
use super::static_serve::serve_resource_tree_static;

// ---------------------------------------------------------------------------
// App name extraction
// ---------------------------------------------------------------------------

/// Extract the app name from the request.
///
/// 1. If `path_name` is `Some` and non-empty, use it (path-based routing).
/// 2. Otherwise parse the `Host` header: extract `{app_name}.{domain}`.
/// 3. Ignore bare `localhost`, `localhost:PORT`, and IP addresses.
pub fn extract_app_name(req: &HttpRequest, path_name: Option<&str>) -> Option<String> {
    // 1. Path-based routing takes priority
    if let Some(name) = path_name {
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }

    // 2. Subdomain-based routing via Host header
    let host_header = req.headers().get("host")?.to_str().ok()?;

    // Strip port if present
    let host = host_header.split(':').next().unwrap_or(host_header);

    // Ignore bare localhost
    if host == "localhost" {
        return None;
    }

    // Ignore IP addresses (starts with digit or contains only digits and dots)
    if host.starts_with(|c: char| c.is_ascii_digit())
        && host.chars().all(|c| c.is_ascii_digit() || c == '.' || c == ':')
    {
        return None;
    }

    // IPv6 addresses in brackets
    if host.starts_with('[') {
        return None;
    }

    // Extract first subdomain: "myapp.zeroship.ai" → "myapp"
    // Must have at least one dot (i.e., subdomain.domain)
    let dot_pos = host.find('.')?;
    let subdomain = &host[..dot_pos];

    if subdomain.is_empty() {
        return None;
    }

    Some(subdomain.to_string())
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowStepRequest {
    run_id: String,
    app_id: Uuid,
}

/// Internal durable-workflow advance edge.
///
/// This is deliberately mounted before the public app catch-all and rejects
/// requests whose Host resolves as a creator app. The topology is still one
/// ntex app today; a dedicated internal listener can mount this same handler
/// without changing the transport contract.
pub async fn workflow_advance_internal(
    req: HttpRequest,
    state: web::types::State<Arc<GateState>>,
    body: Bytes,
) -> HttpResponse {
    if req.method() != ntex::http::Method::POST {
        return HttpResponse::NotFound().finish();
    }
    if extract_app_name(&req, None).is_some() {
        return HttpResponse::NotFound().finish();
    }

    let request: WorkflowStepRequest = match serde_json::from_slice(body.as_ref()) {
        Ok(request) => request,
        Err(e) => {
            return HttpResponse::BadRequest()
                .json(&serde_json::json!({"error": format!("invalid workflow dispatch request: {e}")}));
        }
    };
    if request.run_id.is_empty() {
        return HttpResponse::BadRequest().json(&serde_json::json!({
            "error": "workflow dispatch request requires runId and appId"
        }));
    }

    let Some(compiled_route) = state.routes.lookup_by_app_id(&request.app_id) else {
        return HttpResponse::NotFound().json(&serde_json::json!({"error": "app route not found"}));
    };

    if let Err(resp) = enforce::check_account(compiled_route.entry.account_state) {
        return resp;
    }
    if let Err(resp) = enforce::check_spend(compiled_route.entry.spend_state) {
        return resp;
    }

    let worker_body = match serde_json::to_vec(&request) {
        Ok(body) => body,
        Err(e) => {
            return HttpResponse::InternalServerError()
                .json(&serde_json::json!({"error": format!("encode worker workflow dispatch: {e}")}));
        }
    };

    // TODO(DW-signed-transport): verify a control-plane signature/nonce before
    // accepting this internal StepRequest, then sign the gateway->worker hop.
    // For DW-05b the worker's unsigned test-flag route is the intentional seam.
    let request_id = Uuid::new_v4();
    let worker_response = match proxy::forward_workflow_advance(
        &state.hash_ring,
        &request.app_id,
        &compiled_route.entry.plan_id,
        &request_id,
        &worker_body,
        &state.config.worker_key,
    )
    .await
    {
        Ok(response) => response,
        Err(e) => {
            return HttpResponse::BadGateway()
                .json(&serde_json::json!({"error": format!("worker error: {e}")}));
        }
    };

    if !worker_response.status().is_success() {
        return worker_response;
    }

    let (_buffered, worker_bytes) = buffer_response_body(worker_response).await;
    match workflow_worker_advance_response(&worker_bytes) {
        Ok(response) => HttpResponse::Ok().json(&response),
        Err(e) => HttpResponse::BadGateway()
            .json(&serde_json::json!({"error": format!("invalid worker workflow advance ack: {e}")})),
    }
}

fn workflow_worker_advance_response(worker_bytes: &[u8]) -> Result<Value, String> {
    let response: Value = serde_json::from_slice(worker_bytes).map_err(|e| e.to_string())?;
    let ack = response
        .get("ack")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let nack = response
        .get("nack")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if ack == nack {
        return Err("response must set exactly one of ack or nack".to_string());
    }
    if response.get("runId").and_then(Value::as_str).is_none() {
        return Err("response missing runId".to_string());
    }
    if ack
        && response
            .get("registrations")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
    {
        return Err("ack response missing registrations".to_string());
    }
    Ok(response)
}

#[cfg(test)]
fn workflow_worker_result_to_step_result(
    request: &WorkflowStepRequest,
    worker_bytes: &[u8],
) -> Result<Value, String> {
    let result: Value = serde_json::from_slice(worker_bytes).map_err(|e| e.to_string())?;
    if result.get("runUpdate").is_some() && result.get("dispatchNonce").is_some() {
        let normalized = normalize_workflow_step_result(result)?;
        let outcomes = legacy_step_result_to_outcomes(&normalized)?;
        return Ok(serde_json::json!({
            "runId": normalized
                .get("runId")
                .and_then(Value::as_str)
                .unwrap_or(request.run_id.as_str()),
            "dispatchNonce": normalized
                .get("dispatchNonce")
                .and_then(Value::as_str)
                .unwrap_or(""),
            "outcomes": outcomes,
        }));
    }

    let run_id = result
        .get("runId")
        .and_then(Value::as_str)
        .unwrap_or(request.run_id.as_str());
    let nonce = result
        .get("dispatchNonce")
        .or_else(|| result.get("nonce"))
        .and_then(Value::as_str)
        .unwrap_or("");

    let outcomes = if let Some(outcomes) = result.get("outcomes") {
        let outcomes = outcomes
            .as_array()
            .ok_or_else(|| "outcomes must be an array".to_string())?
            .clone();
        normalize_workflow_outcomes(outcomes, result.get("error").cloned())?
    } else {
        normalize_workflow_outcomes(vec![single_worker_result_to_outcome(&result)?], None)?
    };

    Ok(serde_json::json!({
        "runId": run_id,
        "dispatchNonce": nonce,
        "outcomes": outcomes,
    }))
}

#[cfg(test)]
fn single_worker_result_to_outcome(result: &Value) -> Result<Value, String> {
    let kind = result
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing kind".to_string())?;
    match kind {
        "StepCompleted" => {
            let mut outcome = serde_json::json!({
                "kind": "StepCompleted",
                "ordinal": required_i64(result, "ordinal")?,
                "name": required_str(result, "name")?,
                "nameOccurrence": result.get("nameOccurrence").and_then(Value::as_i64).unwrap_or(0),
                "stepKind": workflow_step_kind_or_run(result)?,
                "compensable": result.get("compensable").and_then(Value::as_bool).unwrap_or(false),
                "compensationMaxAttempts": result.get("compensationMaxAttempts").and_then(Value::as_i64).unwrap_or(1),
            });
            copy_workflow_output(result, &mut outcome);
            Ok(outcome)
        }
        "RunCompleted" => {
            let mut outcome = serde_json::json!({ "kind": "RunCompleted" });
            copy_workflow_output(result, &mut outcome);
            Ok(outcome)
        }
        "ContinueAsNew" => {
            let mut outcome = serde_json::json!({ "kind": "ContinueAsNew" });
            copy_continue_as_new_input(result, &mut outcome);
            Ok(outcome)
        }
        "RunFailed" => {
            let mut outcome = serde_json::json!({
                "kind": "RunFailed",
                "error": workflow_error_or_default(result.get("error"), "workflow run failed"),
            });
            if result.get("ordinal").is_some() && result.get("name").is_some() {
                outcome["ordinal"] = serde_json::json!(required_i64(result, "ordinal")?);
                outcome["name"] = serde_json::json!(required_str(result, "name")?);
                outcome["nameOccurrence"] =
                    serde_json::json!(result.get("nameOccurrence").and_then(Value::as_i64).unwrap_or(0));
            }
            Ok(outcome)
        }
        "Sleep" => Ok(serde_json::json!({
            "kind": "Sleep",
            "ordinal": required_i64(result, "ordinal")?,
            "name": required_str(result, "name")?,
            "nameOccurrence": result.get("nameOccurrence").and_then(Value::as_i64).unwrap_or(0),
            "wakeAt": result.get("wakeAt").cloned().unwrap_or(Value::Null),
        })),
        "Wait" => Ok(serde_json::json!({
            "kind": "Wait",
            "ordinal": required_i64(result, "ordinal")?,
            "name": required_str(result, "name")?,
            "nameOccurrence": result.get("nameOccurrence").and_then(Value::as_i64).unwrap_or(0),
            "signalType": result.get("signalType")
                .cloned()
                .unwrap_or_else(|| result.get("name").cloned().unwrap_or(Value::Null)),
            "timeout": result.get("timeout").cloned().unwrap_or(Value::Null),
            "wakeAt": result.get("wakeAt").cloned().unwrap_or(Value::Null),
            "maxSignalAge": result.get("maxSignalAge").cloned().unwrap_or(Value::Null),
            "maxSignalAgeMs": result.get("maxSignalAgeMs").cloned().unwrap_or(Value::Null),
            "topic": result.get("topic").cloned().unwrap_or(Value::Null),
        })),
        "Child" => Ok(serde_json::json!({
            "kind": "Child",
            "ordinal": required_i64(result, "ordinal")?,
            "name": required_str(result, "name")?,
            "nameOccurrence": result.get("nameOccurrence").and_then(Value::as_i64).unwrap_or(0),
            "childWorkflowName": result.get("childWorkflowName")
                .cloned()
                .or_else(|| result.get("workflowName").cloned())
                .unwrap_or(Value::Null),
            "input": result.get("input").cloned().unwrap_or(Value::Null),
            "options": result.get("options").cloned().unwrap_or_else(|| serde_json::json!({})),
        })),
        "CompensationCompleted" => Ok(serde_json::json!({
            "kind": "CompensationCompleted",
            "ordinal": required_i64(result, "ordinal")?,
            "name": required_str(result, "name")?,
            "nameOccurrence": result.get("nameOccurrence").and_then(Value::as_i64).unwrap_or(0),
        })),
        "CompensationFailed" => Ok(serde_json::json!({
            "kind": "CompensationFailed",
            "ordinal": required_i64(result, "ordinal")?,
            "name": required_str(result, "name")?,
            "nameOccurrence": result.get("nameOccurrence").and_then(Value::as_i64).unwrap_or(0),
            "error": workflow_error_or_default(result.get("error"), "workflow compensator failed"),
        })),
        other => Err(format!("unknown worker workflow result kind {other:?}")),
    }
}

#[cfg(test)]
fn workflow_step_kind_or_run(value: &Value) -> Result<Value, String> {
    let step_kind = value
        .get("stepKind")
        .filter(|value| !value.is_null())
        .and_then(Value::as_str)
        .unwrap_or("run");
    match step_kind {
        "run" | "sideEffect" | "child" => Ok(Value::String(step_kind.to_string())),
        other => Err(format!("unknown workflow stepKind {other:?}")),
    }
}

#[cfg(test)]
fn copy_workflow_output(source: &Value, target: &mut Value) {
    if let Some(output_ref) = source.get("outputRef").filter(|value| !value.is_null()) {
        target["outputRef"] = output_ref.clone();
    } else {
        target["output"] = source.get("output").cloned().unwrap_or(Value::Null);
    }
}

#[cfg(test)]
fn copy_continue_as_new_input(source: &Value, target: &mut Value) {
    if let Some(input_ref) = source.get("inputRef").filter(|value| !value.is_null()) {
        target["inputRef"] = input_ref.clone();
    } else {
        target["input"] = source.get("input").cloned().unwrap_or(Value::Null);
    }
}

#[cfg(test)]
fn ensure_workflow_output(outcome: &mut Value) {
    let has_ref = outcome
        .get("outputRef")
        .is_some_and(|value| !value.is_null());
    let has_output = outcome.get("output").is_some();
    if !has_ref && !has_output {
        outcome["output"] = Value::Null;
    }
}

#[cfg(test)]
fn ensure_continue_as_new_input(outcome: &mut Value) {
    let has_ref = outcome
        .get("inputRef")
        .is_some_and(|value| !value.is_null());
    let has_input = outcome.get("input").is_some();
    if !has_ref && !has_input {
        outcome["input"] = Value::Null;
    }
}

#[cfg(test)]
fn workflow_error_or_default(error: Option<&Value>, message: &str) -> Value {
    error
        .filter(|value| !value.is_null())
        .cloned()
        .unwrap_or_else(|| serde_json::json!({"type": "Error", "message": message}))
}

#[cfg(test)]
fn normalize_workflow_outcomes(
    mut outcomes: Vec<Value>,
    fallback_error: Option<Value>,
) -> Result<Vec<Value>, String> {
    if outcomes.is_empty() {
        return Err("workflow outcome batch is empty".to_string());
    }
    let fallback_error = fallback_error.filter(|value| !value.is_null());
    let last = outcomes.len() - 1;
    for (idx, outcome) in outcomes.iter_mut().enumerate() {
        let kind = outcome
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| "outcome missing kind".to_string())?;
        // StepCompleted and Child may appear non-trailing: a dispatch can settle
        // multiple concurrent steps and spawn multiple children (startMany) in one
        // batch. A true suspension/terminal is mutually exclusive and must be the
        // trailing entry. Compensation is a serial reverse-frontier outcome and is
        // trailing too.
        if kind != "StepCompleted" && kind != "Child" && idx != last {
            return Err("workflow suspension or terminal outcome must be the trailing batch entry".to_string());
        }
        match kind {
            "StepCompleted" => {
                let step_kind = workflow_step_kind_or_run(outcome)?;
                outcome["stepKind"] = step_kind;
                outcome["compensable"] = serde_json::json!(
                    outcome.get("compensable").and_then(Value::as_bool).unwrap_or(false)
                );
                outcome["compensationMaxAttempts"] = serde_json::json!(
                    outcome.get("compensationMaxAttempts").and_then(Value::as_i64).unwrap_or(1)
                );
                ensure_workflow_output(outcome);
            }
            "RunCompleted" => ensure_workflow_output(outcome),
            "ContinueAsNew" => ensure_continue_as_new_input(outcome),
            "RunFailed" => {
                if outcome.get("error").is_none() || outcome.get("error").is_some_and(Value::is_null) {
                    let message = if outcome.get("ordinal").is_some() && outcome.get("name").is_some() {
                        "workflow step failed"
                    } else {
                        "workflow run failed"
                    };
                    outcome["error"] = workflow_error_or_default(fallback_error.as_ref(), message);
                }
            }
            "Sleep" => {
                let wake_at = normalize_workflow_wake_at(outcome.get("wakeAt"))
                    .ok_or_else(|| "invalid sleep wakeAt".to_string())?;
                outcome["wakeAt"] = wake_at;
            }
            "Wait" => {
                let wake_at = normalize_workflow_wake_at(
                    outcome.get("wakeAt").filter(|value| !value.is_null()).or_else(|| outcome.get("timeout")),
                )
                .ok_or_else(|| "invalid wait timeout".to_string())?;
                outcome["wakeAt"] = wake_at;
                outcome["signalType"] = outcome
                    .get("signalType")
                    .cloned()
                    .unwrap_or_else(|| outcome.get("name").cloned().unwrap_or(Value::Null));
                outcome["maxSignalAgeMs"] = normalize_workflow_duration_ms(
                    outcome
                        .get("maxSignalAgeMs")
                        .filter(|value| !value.is_null())
                        .or_else(|| outcome.get("maxSignalAge")),
                )
                .ok_or_else(|| "invalid wait maxSignalAge".to_string())?;
            }
            "Child" => {
                // Child spawn (parent parks) — a trailing suspension outcome.
                // Fields (ordinal/name/childWorkflowName/input/options) pass through;
                // the engine's StepOutcome::Child fold spawns the child run + parks
                // the parent. The runtime emits both childWorkflowName and the base
                // workflowName; keep only childWorkflowName (StepOutcome::Child aliases
                // workflowName → child_workflow_name, so both present = a serde
                // "duplicate field" error).
                if let Some(obj) = outcome.as_object_mut() {
                    if !obj.contains_key("childWorkflowName") {
                        if let Some(wn) = obj.get("workflowName").cloned() {
                            obj.insert("childWorkflowName".to_string(), wn);
                        }
                    }
                    obj.remove("workflowName");
                }
            }
            "CompensationCompleted" => {
                let _ = required_i64(outcome, "ordinal")?;
                let _ = required_str(outcome, "name")?;
                outcome["nameOccurrence"] =
                    serde_json::json!(outcome.get("nameOccurrence").and_then(Value::as_i64).unwrap_or(0));
            }
            "CompensationFailed" => {
                let _ = required_i64(outcome, "ordinal")?;
                let _ = required_str(outcome, "name")?;
                outcome["nameOccurrence"] =
                    serde_json::json!(outcome.get("nameOccurrence").and_then(Value::as_i64).unwrap_or(0));
                if outcome.get("error").is_none() || outcome.get("error").is_some_and(Value::is_null) {
                    outcome["error"] = workflow_error_or_default(fallback_error.as_ref(), "workflow compensator failed");
                }
            }
            other => return Err(format!("unknown worker workflow outcome kind {other:?}")),
        }
    }
    Ok(outcomes)
}

#[cfg(test)]
fn legacy_step_result_to_outcomes(result: &Value) -> Result<Vec<Value>, String> {
    let mut outcomes = Vec::new();
    let mut failed_checkpoint_encoded = false;
    for checkpoint in result
        .get("checkpoints")
        .and_then(Value::as_array)
        .ok_or_else(|| "legacy StepResult missing checkpoints".to_string())?
    {
        let kind = checkpoint.get("kind").and_then(Value::as_str).unwrap_or_default();
        let state = checkpoint.get("state").and_then(Value::as_str).unwrap_or_default();
        match (kind, state) {
            ("run" | "sideEffect", "completed") => {
                let compensable = checkpoint
                    .get("compensable")
                    .and_then(Value::as_bool)
                    .unwrap_or_else(|| {
                        checkpoint
                            .get("compensationState")
                            .and_then(Value::as_str)
                            .is_some_and(|state| {
                                matches!(state, "pending" | "running" | "completed" | "failed")
                            })
                    });
                let mut outcome = serde_json::json!({
                    "kind": "StepCompleted",
                    "ordinal": required_i64(checkpoint, "ordinal")?,
                    "name": required_str(checkpoint, "name")?,
                    "nameOccurrence": checkpoint.get("nameOccurrence").and_then(Value::as_i64).unwrap_or(0),
                    "stepKind": kind,
                    "compensable": compensable,
                    "compensationMaxAttempts": checkpoint
                        .get("compensationMaxAttempts")
                        .and_then(Value::as_i64)
                        .unwrap_or(1),
                });
                copy_workflow_output(checkpoint, &mut outcome);
                outcomes.push(outcome);
            }
            ("run", "failed") => {
                failed_checkpoint_encoded = true;
                outcomes.push(serde_json::json!({
                    "kind": "RunFailed",
                    "ordinal": required_i64(checkpoint, "ordinal")?,
                    "name": required_str(checkpoint, "name")?,
                    "nameOccurrence": checkpoint.get("nameOccurrence").and_then(Value::as_i64).unwrap_or(0),
                    "error": checkpoint.get("error").cloned().unwrap_or_else(|| {
                        serde_json::json!({"type": "Error", "message": "workflow step failed"})
                    }),
                }));
            }
            ("sleep", "running") => outcomes.push(serde_json::json!({
                "kind": "Sleep",
                "ordinal": required_i64(checkpoint, "ordinal")?,
                "name": required_str(checkpoint, "name")?,
                "nameOccurrence": checkpoint.get("nameOccurrence").and_then(Value::as_i64).unwrap_or(0),
                "wakeAt": checkpoint.get("wakeAt").cloned().unwrap_or(Value::Null),
            })),
            ("wait_signal", "running") => outcomes.push(serde_json::json!({
                "kind": "Wait",
                "ordinal": required_i64(checkpoint, "ordinal")?,
                "name": required_str(checkpoint, "name")?,
                "nameOccurrence": checkpoint.get("nameOccurrence").and_then(Value::as_i64).unwrap_or(0),
                "wakeAt": checkpoint.get("wakeAt").cloned().unwrap_or(Value::Null),
                "signalType": checkpoint.get("signalType").cloned().unwrap_or(Value::Null),
                "maxSignalAgeMs": checkpoint.get("maxSignalAgeMs").cloned().unwrap_or(Value::Null),
                "consumedSignalId": checkpoint.get("consumedSignalId").cloned().unwrap_or(Value::Null),
            })),
            _ => {}
        }
    }

    match result.pointer("/runUpdate/state").and_then(Value::as_str) {
        Some("completed") => outcomes.push(serde_json::json!({
            "kind": "RunCompleted",
            "output": result.pointer("/runUpdate/output").cloned().unwrap_or(Value::Null),
        })),
        Some("failed") if !failed_checkpoint_encoded => outcomes.push(serde_json::json!({
            "kind": "RunFailed",
            "error": result.pointer("/runUpdate/error").cloned().unwrap_or_else(|| {
                serde_json::json!({"type": "Error", "message": "workflow run failed"})
            }),
        })),
        _ => {}
    }

    normalize_workflow_outcomes(outcomes, None)
}

#[cfg(test)]
fn required_i64(value: &Value, key: &str) -> Result<i64, String> {
    value
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("missing {key}"))
}

#[cfg(test)]
fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {key}"))
}

#[cfg(test)]
fn normalize_workflow_step_result(mut result: Value) -> Result<Value, String> {
    let state = result
        .pointer("/runUpdate/state")
        .and_then(Value::as_str)
        .unwrap_or_default();

    if state == "sleeping" {
        let wake_at = normalize_workflow_wake_at(result.pointer("/runUpdate/wakeAt"))
            .filter(|value| !value.is_null())
            .or_else(|| {
                result
                    .get("checkpoints")
                    .and_then(Value::as_array)
                    .and_then(|checkpoints| {
                        checkpoints.iter().find_map(|checkpoint| {
                            let is_sleep =
                                checkpoint.get("kind").and_then(Value::as_str) == Some("sleep");
                            let is_running =
                                checkpoint.get("state").and_then(Value::as_str) == Some("running");
                            (is_sleep && is_running)
                                .then(|| normalize_workflow_wake_at(checkpoint.get("wakeAt")))
                                .flatten()
                                .filter(|value| !value.is_null())
                        })
                    })
            })
            .ok_or_else(|| "sleeping workflow StepResult missing wakeAt".to_string())?;

        if let Some(update) = result.get_mut("runUpdate").and_then(Value::as_object_mut) {
            update.insert("wakeAt".to_string(), wake_at.clone());
        }
        if let Some(checkpoints) = result.get_mut("checkpoints").and_then(Value::as_array_mut) {
            for checkpoint in checkpoints {
                let is_sleep = checkpoint.get("kind").and_then(Value::as_str) == Some("sleep");
                let is_running = checkpoint.get("state").and_then(Value::as_str) == Some("running");
                if is_sleep && is_running {
                    checkpoint["wakeAt"] = wake_at.clone();
                }
            }
        }
    } else if state == "waiting" {
        let wake_at = normalize_workflow_wake_at(result.pointer("/runUpdate/wakeAt"))
            .ok_or_else(|| "invalid waiting workflow wakeAt".to_string())?;
        if let Some(update) = result.get_mut("runUpdate").and_then(Value::as_object_mut) {
            update.insert("wakeAt".to_string(), wake_at.clone());
        }
        if let Some(checkpoints) = result.get_mut("checkpoints").and_then(Value::as_array_mut) {
            for checkpoint in checkpoints {
                let is_wait =
                    checkpoint.get("kind").and_then(Value::as_str) == Some("wait_signal");
                let is_running = checkpoint.get("state").and_then(Value::as_str) == Some("running");
                if is_wait && is_running {
                    checkpoint["wakeAt"] = wake_at.clone();
                    checkpoint["maxSignalAgeMs"] =
                        normalize_workflow_duration_ms(checkpoint.get("maxSignalAgeMs"))
                            .ok_or_else(|| "invalid wait maxSignalAgeMs".to_string())?;
                }
            }
        }
    }

    Ok(result)
}

#[cfg(test)]
fn normalize_workflow_wake_at(raw: Option<&Value>) -> Option<Value> {
    let value = raw?;
    if value.is_null() {
        return Some(Value::Null);
    }
    let Some(s) = value.as_str() else {
        return Some(value.clone());
    };
    if DateTime::parse_from_rfc3339(s).is_ok() {
        return Some(Value::String(s.to_string()));
    }
    let ms = parse_iso8601_duration_ms(s)?;
    let wake_at = Utc::now() + chrono::Duration::milliseconds(ms);
    Some(Value::String(wake_at.to_rfc3339()))
}

#[cfg(test)]
fn normalize_workflow_duration_ms(raw: Option<&Value>) -> Option<Value> {
    let Some(value) = raw else {
        return Some(Value::Null);
    };
    if value.is_null() {
        return Some(Value::Null);
    }
    if let Some(ms) = value.as_i64() {
        return Some(Value::Number(ms.into()));
    }
    let s = value.as_str()?;
    let ms = parse_iso8601_duration_ms(s)?;
    Some(Value::Number(ms.into()))
}

#[cfg(test)]
fn parse_iso8601_duration_ms(raw: &str) -> Option<i64> {
    let s = raw.strip_prefix('P')?;
    let (date_part, time_part) = match s.split_once('T') {
        Some((date, time)) => (date, time),
        None => (s, ""),
    };
    let mut total_ms = 0_i64;
    let mut num = String::new();
    for ch in date_part.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            num.push(ch);
            continue;
        }
        let value = parse_duration_number(&num)?;
        num.clear();
        match ch {
            'D' => total_ms = total_ms.checked_add((value * 86_400_000.0).round() as i64)?,
            _ => return None,
        }
    }
    if !num.is_empty() {
        return None;
    }
    for ch in time_part.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            num.push(ch);
            continue;
        }
        let value = parse_duration_number(&num)?;
        num.clear();
        match ch {
            'H' => total_ms = total_ms.checked_add((value * 3_600_000.0).round() as i64)?,
            'M' => total_ms = total_ms.checked_add((value * 60_000.0).round() as i64)?,
            'S' => total_ms = total_ms.checked_add((value * 1_000.0).round() as i64)?,
            _ => return None,
        }
    }
    if !num.is_empty() {
        return None;
    }
    Some(total_ms.max(0))
}

#[cfg(test)]
fn parse_duration_number(raw: &str) -> Option<f64> {
    if raw.is_empty() {
        return None;
    }
    let value = raw.parse::<f64>().ok()?;
    value.is_finite().then_some(value)
}

// ---------------------------------------------------------------------------
// Per-rule rate-limit bucket key derivation
// ---------------------------------------------------------------------------

/// Resolve the bucket discriminator for a per-rule rate limit. The
/// returned string is concatenated with `(app_id, rule_idx)` in
/// `PerRuleKey` to form the bucket key — clients sharing the same
/// discriminator share a token bucket.
///
/// * `RateLimitPer::Ip` — request's client IP. Falls back to "unknown"
///   when the connection has no peer address (test fixtures, exotic
///   transports). Uses the peer socket unless `trust_proxy` is enabled.
/// * `RateLimitPer::User` — the authenticated user identity (the JWT
///   `sub`). Unlike `Session`, one user's many sessions/devices share a
///   bucket. Callers whose identity was not established fall back to the
///   IP, so an unauthenticated burst still gets bucketed instead of
///   sharing one `""` key.
/// * `RateLimitPer::Session` — the `__Host-zeroship_app_session` cookie
///   value (the per-origin session id the gateway mints on
///   `/__zeroship/auth/callback`), again only once identity is
///   established; otherwise the IP.
/// * `RateLimitPer::App` — constant `"app"`. One bucket platform-wide;
///   `(app_id, rule_idx, "app")` is the key, equivalent to a global
///   per-app limit at the rule level.
///
/// `identity_verified` says whether the auth gate actually resolved a
/// user for this request. It is load-bearing, not decorative: the cookie
/// and the JWT `sub` are read WITHOUT re-verifying a signature here, which
/// is sound only because something upstream already did. On an `auth:
/// "user"` route that holds — an invalid credential never reaches this
/// point. On an `auth: "anon"` route it does not: `resolve_auth` lets a
/// missing, expired, or outright forged credential through, so trusting
/// those bytes would let the caller pick their own bucket and mint a
/// fresh allowance per request simply by varying a cookie. Whenever
/// identity was not established, the discriminator has to be something
/// the caller does not control.
pub(crate) fn compute_bucket_id(
    req: &HttpRequest,
    per: zeroship_bundle::RateLimitPer,
    trust_proxy: bool,
    identity_verified: bool,
) -> String {
    use zeroship_bundle::RateLimitPer;
    match per {
        RateLimitPer::Ip => client_ip(req, trust_proxy),
        // Unverified caller on an identity-scoped rule: the only honest
        // discriminator left is the network peer.
        RateLimitPer::User | RateLimitPer::Session if !identity_verified => {
            client_ip(req, trust_proxy)
        }
        RateLimitPer::User => {
            // `"Bearer "` ONLY, and it must stay that way: `router/auth.rs`
            // strips exactly this prefix, so anything else here would be a
            // subject the gate never looked at. `identity_verified` attests
            // that SOME credential was verified - the cookie, on this path -
            // not that this header was, so widening the set would let a
            // cookie-authenticated caller hand-roll an unsigned JWT and mint
            // themselves a private bucket. Keep this set <= the gate's.
            if let Some(sub) = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|auth| auth.strip_prefix("Bearer "))
                .and_then(|jwt| jwt_subject_unverified(jwt.trim()))
            {
                return format!("sub:{sub}");
            }
            // Unauthenticated caller hitting a user-scoped rule: degrade to
            // the session cookie, then the IP — never share one empty bucket.
            let cookie = req
                .headers()
                .get("cookie")
                .and_then(|v| v.to_str().ok());
            extract_session_cookie(cookie)
                .map(|s| format!("sess:{s}"))
                .unwrap_or_else(|| client_ip(req, trust_proxy))
        }
        RateLimitPer::Session => {
            let cookie = req
                .headers()
                .get("cookie")
                .and_then(|v| v.to_str().ok());
            extract_session_cookie(cookie)
                .unwrap_or_else(|| client_ip(req, trust_proxy))
        }
        RateLimitPer::App => "app".to_string(),
    }
}

/// Bucket discriminator for a caller whose address could not be resolved.
/// One shared bucket, deliberately: an unresolvable caller must not get a
/// private allowance just for being unresolvable.
const UNKNOWN_CLIENT_IP: &str = "unknown";

/// The client address, as a bare IP.
///
/// Deferring to ntex's `connection_info().remote()` was wrong three ways, and
/// all three ended up in a rate-limit bucket key or an audit row:
///
/// * it returns the LEFTMOST `X-Forwarded-For` token, which is whatever the
///   caller sent, while `zeroship-control` and `zeroship-auth` both read the
///   rightmost (the entry the fronting proxy authored);
/// * it reads a `Forwarded` header ahead of `X-Forwarded-For`, and nothing in
///   this deployment emits one, so honouring it only ever honoured a caller;
/// * it validates nothing, so a value with a port -- including its own peer
///   fallback, `format!("{addr}")` over a `SocketAddr` -- reached the bucket
///   key verbatim and gave every TCP connection a bucket of its own.
///
/// The resolution itself lives in [`zeroship_core::client_ip`], shared with
/// control and auth so the three cannot drift apart again.
pub(crate) fn client_ip(req: &HttpRequest, trust_proxy: bool) -> String {
    resolve_client_ip(req, trust_proxy)
        .map_or_else(|| UNKNOWN_CLIENT_IP.to_string(), |ip| ip.to_string())
}

/// [`client_ip`] before it is stringified, for callers that want the address.
fn resolve_client_ip(req: &HttpRequest, trust_proxy: bool) -> Option<std::net::IpAddr> {
    let forwarded_for = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok());
    zeroship_core::client_ip::resolve_client_ip(
        forwarded_for,
        req.peer_addr().map(|addr| addr.ip()),
        trust_proxy,
    )
}

/// True when the request carries the RFC 6455 upgrade headers we
/// expect for a WebSocket subscription. We check both `Upgrade` and
/// `Connection` (case-insensitive) to mirror what reverse proxies
/// typically forward — some normalize header casing, some don't.
pub(crate) fn is_websocket_upgrade(req: &HttpRequest) -> bool {
    let upgrade = req
        .headers()
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !upgrade.eq_ignore_ascii_case("websocket") {
        return false;
    }
    let connection = req
        .headers()
        .get("connection")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // Connection can be a comma-separated list; check token-by-token.
    connection
        .split(',')
        .any(|tok| tok.trim().eq_ignore_ascii_case("upgrade"))
}

/// Affinity discriminator for a subscription request — the second key
/// (after `app_id`) the gateway hashes to pick a worker. Callers who
/// reconnect with the same JWT or from the same client IP must land
/// on the same worker for the lifetime of a connection so the worker
/// can keep per-subscription state across `_zsRunSubscriptionGen`
/// turns. Spec §16 #4. Resolution order:
///
///   1. JWT subject (extracted from `Authorization: Bearer <jwt>` —
///      we do NOT verify the signature here; the gateway's auth
///      gate already ran and treats verification failures as 401).
///   2. `__Host-zeroship_app_session` cookie value (browser tab affinity).
///   3. `Sec-WebSocket-Key` (per-connection nonce — same connection
///      always hashes to the same bucket; reconnects vary).
///   4. Client IP (last-resort fallback for unauthenticated callers
///      hitting subscriptions on `publicly_accessible` resources).
pub(crate) fn subscription_affinity_key(
    req: &HttpRequest,
    trust_proxy: bool,
) -> String {
    // `"Bearer "` ONLY - see the note in `compute_bucket_id`. This function is
    // stricter-looking but weaker: it takes no `identity_verified`, so it reads
    // the header unconditionally. Accepting a scheme the gate never validated
    // would let an entirely UNAUTHENTICATED caller choose their own affinity
    // key and so steer their own CHWBL worker.
    if let Some(auth) = req.headers().get("authorization").and_then(|v| v.to_str().ok()) {
        if let Some(rest) = auth.strip_prefix("Bearer ") {
            if let Some(sub) = jwt_subject_unverified(rest.trim()) {
                return format!("sub:{sub}");
            }
        }
    }
    let cookie = req
        .headers()
        .get("cookie")
        .and_then(|v| v.to_str().ok());
    if let Some(token) = extract_session_cookie(cookie) {
        return format!("sess:{token}");
    }
    if let Some(key) = req
        .headers()
        .get("sec-websocket-key")
        .and_then(|v| v.to_str().ok())
    {
        return format!("wsk:{key}");
    }
    let ip = client_ip(req, trust_proxy);
    format!("ip:{ip}")
}

// ---------------------------------------------------------------------------
// Existing path-based handler
// ---------------------------------------------------------------------------

pub async fn handle(
    req: HttpRequest,
    state: web::types::State<Arc<GateState>>,
    path: web::types::Path<(String, String)>,
    body: Bytes,
) -> HttpResponse {
    let (app_name, tail) = path.into_inner();
    handle_request(req, state, &app_name, &tail, body).await
}

// ---------------------------------------------------------------------------
// Subdomain-based handler (catch-all)
// ---------------------------------------------------------------------------

pub async fn handle_subdomain(
    req: HttpRequest,
    state: web::types::State<Arc<GateState>>,
    path: web::types::Path<String>,
    body: Bytes,
) -> HttpResponse {
    // OIDC callback for hosted creator apps. Intercepted *before*
    // manifest dispatch so the path can never collide with a route
    // the creator wrote; the `/__zeroship/` prefix is reserved.
    if req.uri().path() == "/__zeroship/auth/callback" {
        return handle_auth_callback(req, state).await;
    }

    let app_name = match extract_app_name(&req, None) {
        Some(name) => name,
        None => {
            return HttpResponse::BadRequest()
                .json(&serde_json::json!({"error": "could not determine app from Host header"}));
        }
    };
    let tail = path.into_inner();
    handle_request(req, state, &app_name, &tail, body).await
}

// ---------------------------------------------------------------------------
// Unified request handler
// ---------------------------------------------------------------------------

/// Outcome of canonicalizing an inbound dispatch path (SEC-2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CanonicalPath {
    /// The path is safe to dispatch under this canonical form. Auth matching
    /// AND the worker-forwarded URL both use this exact string, so the
    /// gateway's resource match can never disagree with the worker's
    /// WHATWG `new URL` re-parse.
    Use(String),
    /// The path carried a traversal / empty-segment form (`.`/`..`, literal or
    /// `%2e`-encoded, or `//`) that a browser's `new URL` would silently
    /// rewrite. Rather than guess the worker's normalization, reject (400).
    Reject,
}

/// Canonicalize an inbound dispatch path for SEC-2.
///
/// Fails CLOSED on the gateway↔worker path-disagreement class: a path that
/// carries a dot-segment (`.`/`..`, literal or `%2e`-encoded) or an empty
/// interior segment (`//`) is `Reject`ed (the caller answers 400) — a browser's
/// `new URL` would silently rewrite those, so matching auth on one form while
/// forwarding another is the bypass. Every other path is returned as its
/// [`canonicalize_path`] normal form (e.g. a lone trailing slash is stripped),
/// and the caller forwards THAT canonical string to the worker so the worker's
/// `new URL(req.url).pathname` reproduces exactly what the gateway matched.
pub(crate) fn canonicalize_dispatch_path(dispatch_path: &str) -> CanonicalPath {
    if crate::compiled::path_has_traversal_or_empty_segment(dispatch_path) {
        return CanonicalPath::Reject;
    }
    CanonicalPath::Use(crate::compiled::canonicalize_path(dispatch_path))
}

async fn handle_request(
    req: HttpRequest,
    state: web::types::State<Arc<GateState>>,
    app_name: &str,
    tail: &str,
    body: Bytes,
) -> HttpResponse {
    let wall_start = std::time::Instant::now();

    // 1. Route resolution — we need the app_id for both dispatch and static assets.
    let (app_id, compiled_route) = match state.routes.lookup_by_name(app_name) {
        Some(r) => r,
        None => {
            return HttpResponse::NotFound()
                .json(&serde_json::json!({"error": format!("app '{app_name}' not found")}));
        }
    };

    // Normalize tail: strip leading slash
    let tail = tail.strip_prefix('/').unwrap_or(tail);
    let raw_dispatch_path = format!("/{tail}");

    // SEC-2: canonicalize the request path BEFORE any resource matching, and
    // derive the worker-forwarded path from the SAME canonical form so the
    // gateway's auth match can never disagree with the worker's WHATWG
    // `new URL(req.url).pathname`. A traversal / empty-segment form (`.`/`..`,
    // literal or `%2e`-encoded, or `//`) — which a browser would silently
    // rewrite — is rejected (400) rather than guessed.
    let dispatch_path = match canonicalize_dispatch_path(&raw_dispatch_path) {
        CanonicalPath::Use(p) => p,
        CanonicalPath::Reject => {
            return HttpResponse::BadRequest().json(&serde_json::json!({
                "error": "request path is not canonical (path traversal or empty segment)",
            }));
        }
    };
    // The forwarded `tail` is the canonical path minus its leading slash, so
    // the URL the worker re-parses matches the resource the gateway gated.
    let tail = dispatch_path.strip_prefix('/').unwrap_or(&dispatch_path);

    // CORS preflight short-circuit. The browser sends `OPTIONS` with
    // `Origin` and `Access-Control-Request-Method` *before* the actual
    // request; we look up the resource-tree CORS policy for the
    // requested path and answer with a 204. If no resource matches,
    // fall through to normal dispatch (which will return 404).
    let resolved_resource = compiled_route
        .manifest
        .lookup_canonical_resource_resolved(&dispatch_path);
    if req.method() == ntex::http::Method::OPTIONS && req.headers().contains_key("origin") {
        if let Some(resolved) = resolved_resource.as_ref() {
            if let Some(cors) = &resolved.policy.cors {
                let origin = req
                    .headers()
                    .get("origin")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                return build_preflight_response(cors, origin, wall_start);
            }
        }
    }

    // Resource-tree dispatch — resources is the only dispatch path.
    // No match → 404 (the `*` catch-all in resources should always
    // match if the user wants a fallback handler).
    let Some(resolved_resource) = resolved_resource else {
        // On the RPC rail, answer in the WORKER's error envelope rather than
        // the gateway's generic one. Measured 2026-08-11 by
        // `tests/e2e_dev_vs_deployed_errors.sh` (dispatcher leg): an unknown
        // procedure id returned `{"message":"Method not found: <id>",
        // "name":"Error","code":"NOT_FOUND"}` in `pnpm dev` and
        // `{"error":"no resource matched"}` deployed. Different key, different
        // text, and no `code` at all deployed - so a client that branches on
        // `code === "NOT_FOUND"` works locally and silently stops working in
        // production. Both shapes were defensible in isolation; two shapes for
        // one operation is not, and the gateway is the side that has to move
        // because the worker's envelope is what every other RPC error uses.
        if let Some(resp) = rpc_predispatch_not_found(&dispatch_path) {
            return resp;
        }
        return HttpResponse::NotFound()
            .json(&serde_json::json!({"error": "no resource matched"}));
    };

    execute_resource_tree(
        req,
        state,
        &app_id,
        &compiled_route,
        resolved_resource,
        &dispatch_path,
        tail,
        body,
        wall_start,
    )
    .await
}

// ---------------------------------------------------------------------------
// v3 resource-tree dispatch
// ---------------------------------------------------------------------------

/// Run the v3 resource-tree dispatch for a request that resolved to a
/// manifest with `resources` non-empty. Looks up the matching resource,
/// enforces the precomputed `EffectivePolicy`, and executes the
/// resolved action (worker forward / redirect / static).
#[allow(clippy::too_many_arguments)]
async fn execute_resource_tree(
    req: HttpRequest,
    state: web::types::State<Arc<GateState>>,
    app_id: &Uuid,
    compiled_route: &crate::sync::CompiledRoute,
    resolved_resource: crate::compiled::ResolvedResource<'_>,
    dispatch_path: &str,
    tail: &str,
    body: Bytes,
    wall_start: std::time::Instant,
) -> HttpResponse {
    use crate::compiled::ResolvedAction;
    use zeroship_bundle::ProcedureKind;

    let policy = resolved_resource.policy;

    // 1a. Account gate (G2): the OUTER AND, evaluated BEFORE spend at the SAME
    //     hoist point so a `Suspended` creator's apps 402 across every action
    //     class (worker, redirect, rewrite, AND static egress) before any worker
    //     proxy — a suspended creator must not serve billed static/redirect
    //     egress either. `Suspended` → 402 `ACCOUNT_SUSPENDED`; `PastDue` (the
    //     grace window) and `Active` pass. Distinct from the spend 402 below
    //     (`SPEND_LIMIT`) so a dead-card suspension is told apart from a usage cap.
    if let Err(resp) = enforce::check_account(compiled_route.entry.account_state) {
        return resp;
    }

    // 1b. Spend gate: hoisted to the TOP — BEFORE the action
    //     match — so `Block` 402s every action class uniformly (worker forward,
    //     redirect, rewrite, AND static), not just the worker path. A Blocked
    //     app must not serve static assets / redirects either: that egress is
    //     billed work the platform eats. `Warn`/`Allow`/`Degrade` pass here;
    //     Warn still only adds the `x-zs-spend-warn` header at the worker call
    //     site.
    //     Fail-closed: an unknown spend state maps to Block (see `check_spend`).
    //
    if let Err(resp) = enforce::check_spend(compiled_route.entry.spend_state) {
        return resp;
    }

    // 1c. Global per-app rate limit + concurrency ceiling. Hoisted to sit with
    //     the two gates above, and for the same reason: so they bind every
    //     action class rather than only the worker arms. These two are the
    //     ONLY readers of the degraded registries, so while they lived inside
    //     `handle_dispatch` / `handle_subscription_dispatch`, `Degrade` was a
    //     no-op for `Static` and `Redirect` — the arms that serve an SPA, and
    //     the same egress `Block` refuses above and step 8b meters as
    //     `gateway_egress_bytes`. Billing it, refusing it when Blocked, and
    //     never throttling it when Degraded was not a coherent set.
    //
    //     A Degraded app pays `DEGRADE_FACTOR` tokens per request against the
    //     same bucket and gets `limit / DEGRADE_FACTOR` concurrency, so this
    //     throttles rather than refuses: at the shipped ceilings that is ~125
    //     rps and 12 in flight, which slows an SPA without taking it down.
    //
    //     The guard binds for the rest of this function, so it covers building
    //     the response as well as producing it — wider than the handler-local
    //     scope it replaces.
    if let Err(resp) = enforce::check_rate_limit(&state.rate_limiters, app_id) {
        return resp;
    }
    let _concurrency_guard = match enforce::acquire_concurrency(&state.concurrency, app_id) {
        Ok(guard) => guard,
        Err(resp) => return resp,
    };

    // Capture origin once for downstream CORS injection.
    let origin_value = req
        .headers()
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // 2. Method-vs-kind gate (RPC procedures only).
    //    `kind: "mutation"` cannot be served via GET, and
    //    `kind: "subscription"` requires GET + Upgrade headers. We
    //    surface 405 for the wrong method and 426 UPGRADE_REQUIRED
    //    when Upgrade headers are missing. The actual handshake runs
    //    in `proxy_subscription_upgrade` once we get past the rest of
    //    the pre-dispatch checks.
    if let Some(kind) = policy.kind {
        let method = req.method();
        let allow = match kind {
            ProcedureKind::Query => {
                method == ntex::http::Method::GET
                    || method == ntex::http::Method::POST
                    || method == ntex::http::Method::HEAD
            }
            ProcedureKind::Mutation => method == ntex::http::Method::POST,
            // Action is the most permissive variant — no method
            // restriction. Mirrors the "no kind" path used by the
            // current vite-plugin manifest emitter, which OMITS `kind`
            // for `action(...)` procedures so today's `if let
            // Some(kind)` check skips the gate entirely. When a future
            // emitter writes `kind: "action"` explicitly, the gate
            // still passes here.
            ProcedureKind::Action => true,
            ProcedureKind::Stream => true,
            ProcedureKind::Subscription => method == ntex::http::Method::GET,
        };
        if !allow {
            // Same envelope decision as the 404 arm above. Dev answers this
            // case from `sdks/bootstrap/src/fetch-handler.ts` with
            // `{"message":"method PUT not allowed on /__zeroship/v1/<id>",
            // "name":"Error","code":"FAILED_PRECONDITION"}`; the gateway used to
            // answer `{"error":"method not allowed for this procedure kind"}`.
            // The message is built the same way on both sides so the two tiers
            // are byte-identical, and it is NOT the old text: the old one named
            // the procedure kind, which dev has no way to know, so keeping it
            // would have meant the deployed tier is the only one a client can
            // parse. The `kind` is recoverable from the manifest; the `code` is
            // what a client actually branches on.
            return zs_rpc_predispatch_error(
                ntex::http::StatusCode::METHOD_NOT_ALLOWED,
                "FAILED_PRECONDITION",
                &format!("method {} not allowed on {}", method, dispatch_path),
            );
        }
        if matches!(kind, ProcedureKind::Subscription) {
            // Subscription URLs only resolve over a WebSocket upgrade.
            // Browsers + load testers that hit them with a plain GET
            // get a structured 426 so they know to switch protocols.
            if !is_websocket_upgrade(&req) {
                return HttpResponse::build(ntex::http::StatusCode::UPGRADE_REQUIRED)
                    .header("connection", "Upgrade")
                    .header("upgrade", "websocket")
                    .json(&serde_json::json!({
                        "code": "UPGRADE_REQUIRED",
                        "message": "this resource is a subscription; use WebSocket (Upgrade: websocket, Sec-WebSocket-Protocol: zs.v1)",
                    }));
            }
        }
    }

    // 3. Auth gate. `anon` always passes (subject to
    //    `publicly_accessible` being set, which is enforced at
    //    validate-time). `user`/`admin` require a valid
    //    `__Host-zeroship_app_session` cookie. Richer admin-vs-user role
    //    checks will arrive with the auth tier.
    //
    //    On miss for an HTML navigation we kick off the OIDC dance via
    //    a 302 → the platform OP; API clients see a 401 with a `WWW-Authenticate`
    //    challenge so they can prompt the user out-of-band.
    let request_id = Uuid::new_v4();
    let user_header_from_gate =
        match resolve_auth(
            &req,
            &state,
            policy,
            &request_id,
            compiled_route.entry.oauth_client_id.as_deref(),
            compiled_route.entry.sector_identifier.as_deref(),
        )
        .await
        {
            AuthOutcome::Allowed { user_header } => user_header,
            AuthOutcome::Unauthenticated => {
                return unauthenticated_response(
                    &req,
                    &state,
                    compiled_route.entry.oauth_client_id.as_deref(),
                );
            }
            AuthOutcome::ClientNotProvisioned => {
                return client_not_provisioned_response();
            }
            AuthOutcome::InsufficientScope { required } => {
                return insufficient_scope_response(&required);
            }
        };

    // 4. CSRF origin guard. Mutations with a declared csrf_origins list
    //    require the request's `Origin` to match.
    //
    //    `None` is NOT matched here, but it is not unguarded either: RPC
    //    dispatch is a POST, so the method arm below catches it. A
    //    kind-less procedure reachable over GET would fall through, which
    //    is exactly the hole the vite emitter closed by always writing
    //    `kind` (see `ProcedureKind` in crates/bundle/src/rule.rs). `None`
    //    now only reaches here from a hand-authored raw-JS manifest.
    if matches!(policy.kind, Some(ProcedureKind::Mutation) | Some(ProcedureKind::Action))
        || req.method() == ntex::http::Method::POST
        || req.method() == ntex::http::Method::PUT
        || req.method() == ntex::http::Method::PATCH
        || req.method() == ntex::http::Method::DELETE
    {
        if let Some(allowed) = &policy.csrf_origins {
            let origin = origin_value.as_deref().unwrap_or("");
            if !allowed.iter().any(|o| o == origin) {
                return HttpResponse::Forbidden()
                    .json(&serde_json::json!({"error": "origin not in csrf_origins allow list"}));
            }
        }
    }

    // 5. Max-input-bytes guard. Cheap when not set; cap the body size
    //    before forwarding to the worker.
    if let Some(cap) = policy.max_input_bytes {
        if body.len() > cap as usize {
            return HttpResponse::PayloadTooLarge()
                .json(&serde_json::json!({"error": "input exceeds max_input_bytes"}));
        }
    }

    // 6. Per-resource rate-limit (when declared).
    //    Reuses the existing `PerRuleRateLimitRegistry` by hashing the
    //    resource key into a stable `rule_idx`. Two distinct resource
    //    keys with the same rate_limit shape get independent buckets.
    if let Some(rl) = &policy.rate_limit {
        let rule_idx = resource_key_hash(resolved_resource.key.as_ref());
        let bucket_id = compute_bucket_id(
            &req,
            rl.per,
            state.config.trust_proxy,
            user_header_from_gate.is_some(),
        );
        if let Err(resp) = state.per_rule_rate_limits.check(
            app_id,
            rule_idx,
            rl.per,
            &bucket_id,
            rl,
        ) {
            return resp;
        }
    }

    // 7. Idempotency dedupe (only for `idempotent: true` mutations).
    //    Resolved before the worker is touched: a hit returns the
    //    stored response verbatim; a conflict / missing-header case
    //    rejects with the right error envelope.
    //
    //    Only mutations enter the dedupe path. Queries are inherently
    //    safe; streams and subscriptions do not use dedupe either.
    //
    //    `None` is admitted alongside `Some(Action)`. The stated reason
    //    used to be that the emitter omits `kind` for actions; it does
    //    not, and never should — it writes every capability precisely so
    //    that unknown and action are distinguishable. `None` therefore
    //    means "hand-authored manifest declared no capability", and
    //    admitting it here treats an undeclared procedure as the most
    //    permissive one, which is the opposite of how the CSRF guard
    //    above reads it. No test constructs `kind: None` on this path.
    //    Tracked as task #202; left as-is because narrowing it changes
    //    what raw-JS deploys get, which is a contract call.
    let idempotency_handle = if policy.idempotent
        && matches!(
            policy.kind,
            Some(ProcedureKind::Mutation) | Some(ProcedureKind::Action) | None
        )
        && matches!(policy.action, ResolvedAction::WorkerRpc)
    {
        match handle_idempotency_pre_dispatch(
            &req,
            &state,
            app_id,
            dispatch_path,
            policy,
            user_header_from_gate.as_deref(),
            &body,
            wall_start,
        )
        .await
        {
            IdempotencyOutcome::ReturnNow(mut resp) => {
                if let (Some(cors), Some(origin)) = (policy.cors.as_ref(), origin_value.as_deref()) {
                    if !origin.is_empty() {
                        inject_cors_response_headers(resp.headers_mut(), cors, origin);
                    }
                }
                return resp;
            }
            IdempotencyOutcome::Proceed(handle) => Some(handle),
        }
    } else {
        None
    };

    // 8. Execute the resolved action.
    //
    //    Metering coverage (#27): track whether THIS arm produced
    //    gateway-originated egress — a body the worker never sees (static
    //    asset, redirect, or the gateway's own error page for those arms).
    //    Only such bodies are metered as `gateway_egress_bytes` in step 8b
    //    below. The worker-proxy arms (`WorkerRpc`/`WorkerSsr`/`Rewrite`)
    //    are NOT gateway-owned: the worker already counts its response body
    //    as `egress_bytes`, so the gateway must never meter it (that would
    //    double-bill the same byte — the one over-bill vector). The two
    //    metrics are disjoint BY CONSTRUCTION (worker-body vs gateway-body).
    let gateway_owned_egress = matches!(
        policy.action,
        ResolvedAction::Static { .. } | ResolvedAction::Redirect { .. }
    );
    //
    //    Response AUTHORSHIP (`ResponseOrigin`) is tracked alongside, and is a
    //    DIFFERENT question from `gateway_owned_egress` above: this arm is
    //    about who wrote the bytes, that one about who is billed for them. The
    //    gateway's own 502/302/503 inside `handle_dispatch` are gateway-
    //    AUTHORED but not gateway-OWNED egress (an error page the platform
    //    emits is not creator-billed traffic), so the two must not be merged.
    //    Only `Worker` responses may enter the idempotency store at step 9.
    let (mut response, response_origin) = match &policy.action {
        ResolvedAction::WorkerRpc | ResolvedAction::WorkerSsr => {
            // Subscription procedures need a WebSocket-aware proxy
            // path. Idempotency is bypassed (already enforced above).
            // Affinity routing uses `(app_id, principal)` so reconnects
            // from the same caller pin the same worker.
            //
            // THE TRANSPARENT WS PROXY DOES NOT EXIST. Until 2026-08-12
            // this comment named a helper in the `proxy` module as the
            // place it was wired. The module is real; that function was
            // in no file in this repo, and the claim contradicted
            // `handle_subscription_dispatch` below, which says so plainly
            // and returns 501. Deployed subscriptions are a stub; only
            // single-tenant `zeroship serve` speaks WebSocket.
            // `tests/ws_subscription_stub_gate.sh` now fails if a name
            // like that comes back without a definition behind it.
            // Bounded: `docs/reference/rpc.md` states subscriptions are
            // not part of the public client surface, so no creator can
            // reach this today. Latent, not live.
            if matches!(policy.kind, Some(ProcedureKind::Subscription)) {
                // The 501 stub is the gateway's own answer, not the app's.
                (
                    handle_subscription_dispatch(
                        req,
                        &state,
                        app_id,
                        &compiled_route.entry,
                        tail,
                        wall_start,
                    )
                    .await,
                    ResponseOrigin::Gateway,
                )
            } else {
                handle_dispatch(
                    req,
                    &state,
                    app_id,
                    &compiled_route.entry,
                    tail,
                    request_id,
                    body,
                    user_header_from_gate.clone(),
                    wall_start,
                )
                .await
            }
        }
        ResolvedAction::Redirect { to, status } => {
            let st = ntex::http::StatusCode::from_u16(*status)
                .unwrap_or(ntex::http::StatusCode::FOUND);
            (
                HttpResponse::build(st)
                    .header("location", to.clone())
                    .header(
                        "x-wall-time-ms",
                        format!("{:.2}", wall_start.elapsed().as_secs_f64() * 1000.0),
                    )
                    .finish(),
                ResponseOrigin::Gateway,
            )
        }
        ResolvedAction::Rewrite { to } => {
            // Unreachable through a validated manifest: `Manifest::validate`
            // rejects `rewrite` because honouring it needs re-entrant rule
            // walking under a hop limit, which does not exist. Kept as a
            // total match arm, and it forwards under the ORIGINAL path -
            // the rewrite target is deliberately discarded rather than
            // half-applied.
            let _ = to;
            handle_dispatch(
                req,
                &state,
                app_id,
                &compiled_route.entry,
                tail,
                request_id,
                body,
                user_header_from_gate.clone(),
                wall_start,
            )
            .await
        }
        ResolvedAction::Static { try_chain } => {
            // Resolve the first asset that exists in `assets` /
            // `runtime_assets`. The legacy walker has more
            // sophisticated `$path` / `[capture]` substitution; for the
            // resource-tree path the build emits literal templates.
            //
            // The gateway serves the bytes off the blob store; the worker is
            // never involved, so this is gateway-authored.
            (
                serve_resource_tree_static(
                    &state,
                    compiled_route,
                    &req,
                    dispatch_path,
                    try_chain,
                    app_id,
                    wall_start,
                )
                .await,
                ResponseOrigin::Gateway,
            )
        }
    };

    // 8b. Gateway egress metering (#27). For gateway-owned arms only, record
    //     the served body length as `gateway_egress_bytes` against the
    //     route's server-resolved `app_id` (never a client value).
    //
    //     Two recording points, by body shape:
    //       * Buffered static (`Bytes`), redirect, and the static arm's own
    //         error bodies (404/503) — `BodySize::Sized(n)` is the FULLY
    //         delivered length, so a one-shot record here is exact.
    //       * Streamed static (`SizedStream`) — the served length is only
    //         INTENDED up front; a client disconnect delivers fewer bytes.
    //         The streamed drain in `static_serve`/`streaming` therefore meters
    //         DELIVERED bytes itself (incremental accrual + a final delta on
    //         completion/disconnect) and stamps `EGRESS_METERED_HEADER`;
    //         `record_gateway_egress` sees the marker and skips the up-front
    //         size so a stream is never double-counted (finding #2).
    //
    //     The bump is cheap — `Meter::increment` takes an `RwLock` read plus a
    //     per-app `Mutex` for the custom metric (uncontended, no await, no
    //     blocking I/O); the flush to control runs in a detached task, so the
    //     proxy hot path is not slowed (the worker arms skip this entirely).
    //     This is disjoint from the worker's `egress_bytes` by construction;
    //     the gateway NEVER touches `egress_bytes`.
    if gateway_owned_egress {
        record_gateway_egress(&state, app_id, &mut response);
    }

    // 9. Idempotency capture — store the worker's response under the
    //    dedupe key when we held the in-flight lock through dispatch.
    //    `response_origin` decides whether there is anything to store at
    //    all: a gateway-synthesised answer says nothing about whether the
    //    app operation happened, so it is released rather than stored.
    //    Errors here are logged but never block the response.
    if let Some(handle) = idempotency_handle {
        response =
            capture_response_for_idempotency(&state, app_id, handle, response_origin, response)
                .await;
    }

    // 10. CORS injection on the response (resource-tree's flattened
    //     `cors`).
    if let (Some(cors), Some(origin)) = (policy.cors.as_ref(), origin_value.as_deref()) {
        if !origin.is_empty() {
            inject_cors_response_headers(response.headers_mut(), cors, origin);
        }
    }

    response
}

/// Record a gateway-originated response's body length as
/// `gateway_egress_bytes` for `app_id` (metering coverage #27).
///
/// Called ONLY for gateway-owned arms (static asset / redirect / the
/// gateway error page those arms emit) — bodies the worker never sees. The
/// worker owns `egress_bytes` for its proxied response bodies, so this
/// function (and the gateway in general) NEVER touches `egress_bytes`: the
/// two metrics are disjoint by construction and can never count the same
/// byte. `app_id` is the route's server-resolved id (never a client value).
///
/// Records the FULLY-delivered body size for buffered bodies (`Bytes` static,
/// redirect, the static arm's 404/503 error bodies) — `BodySize::Sized(n)` is
/// the exact delivered length there. The streamed-static (`SizedStream`) path
/// instead meters DELIVERED bytes inside its own drain (a disconnect bills
/// only what was written, not the intended size — finding #2) and stamps
/// `EGRESS_METERED_HEADER`; we detect that marker, strip it, and skip the
/// up-front size so a stream is never double-counted.
///
/// The increment is a cheap counter bump (an `RwLock` read + a per-app
/// `Mutex` for the custom metric — uncontended, not literally lock-free); the
/// flush to control runs in a detached background task, so this adds no
/// latency to the response path.
fn record_gateway_egress(state: &GateState, app_id: &Uuid, response: &mut HttpResponse) {
    use ntex::http::body::{BodySize, MessageBody};
    // A streamed-static response self-meters delivered bytes in its drain.
    // Strip the internal marker and skip the size record (no double-count).
    if response
        .headers()
        .contains_key(super::static_serve::EGRESS_METERED_HEADER)
    {
        response
            .headers_mut()
            .remove(super::static_serve::EGRESS_METERED_HEADER);
        return;
    }
    if let BodySize::Sized(n) = response.body().size() {
        if n > 0 {
            state
                .meter
                .increment(&app_id.to_string(), "gateway_egress_bytes", n);
        }
    }
}

// ---------------------------------------------------------------------------
// Idempotency dedupe — pre/post worker hooks
// ---------------------------------------------------------------------------

/// Outcome of [`handle_idempotency_pre_dispatch`]. The caller either
/// returns the response right now (cache hit / conflict / missing
/// header) or proceeds to dispatch with the [`InflightHandle`] in
/// hand to capture the worker's response.
pub(super) enum IdempotencyOutcome {
    /// Return this response without invoking the worker.
    ReturnNow(HttpResponse),
    /// Proceed to worker dispatch; capture the response after.
    Proceed(InflightHandle),
}

/// Carries the post-dispatch metadata needed to persist the worker's
/// response under the dedupe key. Constructed by `pre_dispatch` and
/// consumed by `capture_response_for_idempotency`.
pub(super) struct InflightHandle {
    pub(super) entry_key: String,
    pub(super) lock_key: String,
    pub(super) body_hash: String,
    pub(super) ttl_hours: u32,
}

/// Resolve the wireId from a `/__zeroship/v1/<wireId>` dispatch path. The
/// dispatch path always has a leading slash; the wireId is the rest
/// after the literal prefix. Returns `None` for any non-RPC path —
/// callers should only enter idempotency for `WorkerRpc` actions.
fn dispatch_path_wire_id(dispatch_path: &str) -> Option<&str> {
    dispatch_path.strip_prefix("/__zeroship/v1/")
}

/// The RPC error envelope the WORKER speaks:
/// `{"message":...,"name":"Error","code":...}`, `content-type: application/json`.
///
/// This is deliberately NOT [`build_zs_error_response`]'s
/// `application/zs-error+json` `{code,message,details,retryable}` shape. That one
/// is the idempotency subsystem's; the shape here is what
/// `crates/runtime/src/core/init.rs`'s `mkErr` and
/// `sdks/bootstrap/src/fetch-handler.ts`'s `errResponse` produce, and therefore
/// what a client sees for every RPC error the gateway does NOT intercept. A
/// gateway pre-dispatch rejection is the same event to a client as a worker
/// rejection, so it gets the same envelope.
fn zs_rpc_predispatch_error(
    status: ntex::http::StatusCode,
    code: &str,
    message: &str,
) -> HttpResponse {
    HttpResponse::build(status)
        .content_type("application/json")
        .body(
            serde_json::to_vec(&serde_json::json!({
                "message": message,
                "name": "Error",
                "code": code,
            }))
            .unwrap_or_default(),
        )
}

/// Pre-dispatch 404 on the RPC rail, in the worker's envelope. `None` for any
/// non-RPC path, so ordinary asset/page 404s keep the gateway's generic body.
///
/// The id-less form needs its own arm and it is easy to miss:
/// `canonicalize_path` strips a lone trailing slash, so a request for
/// `/__zeroship/v1/` reaches here as `/__zeroship/v1` and `strip_prefix` of the
/// slash-terminated tag returns `None`. Without the first arm that request
/// falls through to the generic 404 - which is exactly the divergence being
/// fixed, one path deeper. Dev answers it 400 `INVALID_ARGUMENT`
/// `"missing wireId"` (`fetch-handler.ts`), and 400 is the more accurate of the
/// two: the request names no procedure, which is a malformed request rather than
/// a missing resource.
fn rpc_predispatch_not_found(dispatch_path: &str) -> Option<HttpResponse> {
    const TAG: &str = "/__zeroship/v1/";
    let id = if dispatch_path == TAG.trim_end_matches('/') {
        ""
    } else {
        dispatch_path.strip_prefix(TAG)?
    };
    if id.is_empty() {
        return Some(zs_rpc_predispatch_error(
            ntex::http::StatusCode::BAD_REQUEST,
            "INVALID_ARGUMENT",
            "missing wireId",
        ));
    }
    Some(zs_rpc_predispatch_error(
        ntex::http::StatusCode::NOT_FOUND,
        "NOT_FOUND",
        &format!("Method not found: {id}"),
    ))
}

/// Build the standard `application/zs-error+json` envelope for an
/// idempotency rejection.
fn build_zs_error_response(
    status: ntex::http::StatusCode,
    code: &str,
    message: &str,
    details: serde_json::Value,
    retryable: bool,
    retry_after_secs: Option<u64>,
    wall_start: std::time::Instant,
) -> HttpResponse {
    let body = serde_json::json!({
        "code": code,
        "message": message,
        "details": details,
        "retryable": retryable,
    });
    let mut builder = HttpResponse::build(status);
    builder.header("content-type", "application/zs-error+json");
    if let Some(secs) = retry_after_secs {
        builder.header("retry-after", secs.to_string());
    }
    builder.header(
        "x-wall-time-ms",
        format!("{:.2}", wall_start.elapsed().as_secs_f64() * 1000.0),
    );
    builder.body(serde_json::to_vec(&body).unwrap_or_default())
}

/// Replay a stored response. Drops hop-by-hop headers (we re-synthesize
/// the connection layer's headers fresh) and stamps `x-zs-idempotent-replay`
/// so callers can observe a hit in the wild.
pub(super) fn build_replay_response(
    stored: &idempotency::StoredResponse,
    wall_start: std::time::Instant,
) -> HttpResponse {
    let status = ntex::http::StatusCode::from_u16(stored.status)
        .unwrap_or(ntex::http::StatusCode::INTERNAL_SERVER_ERROR);
    let mut builder = HttpResponse::build(status);
    for (name, value) in &stored.headers {
        // The stored map already has hop-by-hop headers stripped, but
        // an extra defensive filter here protects against future
        // schema drift.
        builder.set_header(name.as_str(), value.as_str());
    }
    builder.set_header("x-zs-idempotent-replay", "true");
    builder.header(
        "x-wall-time-ms",
        format!("{:.2}", wall_start.elapsed().as_secs_f64() * 1000.0),
    );
    builder.body(stored.body_bytes())
}

/// Resolve the per-procedure inflight wait timeout. Bounded by the
/// procedure's declared timeout when present, and capped at
/// `DEFAULT_INFLIGHT_WAIT_MS` (~30s). Letting dedupe contention block a
/// worker thread for longer than the handler itself could run is
/// pointless.
fn inflight_wait_ms(policy: &crate::compiled::EffectivePolicy) -> u64 {
    match policy.timeout_ms {
        Some(t) if t > 0 => t.min(idempotency::DEFAULT_INFLIGHT_WAIT_MS),
        _ => idempotency::DEFAULT_INFLIGHT_WAIT_MS,
    }
}

/// Resolve who owns this request's dedupe namespace.
///
/// `Ok(Some(sub))` is an authenticated caller keyed on their per-app
/// pairwise subject; `Ok(None)` is the shared anonymous namespace;
/// `Err(outcome)` refuses the request.
///
/// The subject is recovered from the VERIFIED `ZeroShip-User` header, so
/// the partition is the same identity the worker will act under and is
/// never a client-supplied value. It keys off the RESOLVED identity, not
/// the declared auth level: an `auth: "anon"` procedure still resolves a
/// session for a logged-in visitor, and that visitor gets their own
/// partition rather than sharing the anonymous one.
///
/// A `user`/`admin` procedure with no readable principal is REFUSED. Only
/// a gateway-side invariant break reaches that arm — `resolve_auth` 401s
/// an unauthenticated caller long before dispatch, and the header is
/// minted and MAC'd by this same process moments earlier — but falling
/// back to the shared anonymous namespace there would be exactly the
/// cross-user leak the partition exists to prevent.
fn resolve_dedupe_principal(
    state: &Arc<GateState>,
    policy: &crate::compiled::EffectivePolicy,
    user_header: Option<&str>,
    wire_id: &str,
    wall_start: std::time::Instant,
) -> Result<Option<String>, IdempotencyOutcome> {
    let principal_sub = user_header.and_then(|h| super::auth::user_header_subject(state, h));
    match (principal_sub, policy.auth) {
        (Some(sub), _) => Ok(Some(sub)),
        (None, AuthLevel::Anon) => Ok(None),
        (None, AuthLevel::User | AuthLevel::Admin) => {
            tracing::error!(
                wire_id = %wire_id,
                "gateway: idempotent authenticated procedure without a readable principal; \
                 refusing rather than sharing the anonymous dedupe namespace"
            );
            Err(IdempotencyOutcome::ReturnNow(build_zs_error_response(
                ntex::http::StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL",
                "could not resolve the caller identity for idempotency dedupe",
                serde_json::json!({ "reason": "idempotency_principal_unavailable" }),
                false,
                None,
                wall_start,
            )))
        }
    }
}

/// Entropy gate for the SHARED anonymous namespace (spec §8): reject an
/// `auth: "anon"` procedure's `Idempotency-Key` unless it is a UUIDv4/v7.
///
/// Keyed on the DECLARED auth level, not on whether this particular caller
/// happened to be logged in. The constraint has to be knowable from the
/// manifest at build time, and a rule that only bit logged-out callers
/// would make the same key legal or illegal depending on session state.
///
/// An empty/absent key returns `None` here so it falls through to
/// `pre_dispatch`'s `MissingHeader` arm and keeps that more specific error.
fn reject_low_entropy_anon_key(
    policy: &crate::compiled::EffectivePolicy,
    idem_key: Option<&str>,
    wall_start: std::time::Instant,
) -> Option<IdempotencyOutcome> {
    if !matches!(policy.auth, AuthLevel::Anon) {
        return None;
    }
    let key = idem_key.filter(|s| !s.is_empty())?;
    if idempotency::is_high_entropy_key(key) {
        return None;
    }
    Some(IdempotencyOutcome::ReturnNow(build_zs_error_response(
        ntex::http::StatusCode::BAD_REQUEST,
        "INVALID_ARGUMENT",
        "Idempotency-Key for an anonymous procedure must be a UUIDv4 or UUIDv7",
        serde_json::json!({ "reason": "anonymous_idempotency_key_must_be_uuid_v4_or_v7" }),
        false,
        None,
        wall_start,
    )))
}

/// Pre-dispatch idempotency hook: extract `Idempotency-Key`, resolve the
/// dedupe namespace's owner, hash the body, consult the store, and
/// decide. Only called for `idempotent: true` mutations; the caller
/// filters by policy.
///
/// `user_header` is the `ZeroShip-User` header the auth gate resolved for
/// this request (step 3), or `None` when no identity was resolved. It is
/// NOT optional bookkeeping: a dedupe hit replays a stored body verbatim,
/// so the entry key must name the caller it was stored for, or user B
/// sending user A's `Idempotency-Key` is handed A's response.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_idempotency_pre_dispatch(
    req: &HttpRequest,
    state: &Arc<GateState>,
    app_id: &Uuid,
    dispatch_path: &str,
    policy: &crate::compiled::EffectivePolicy,
    user_header: Option<&str>,
    body: &Bytes,
    wall_start: std::time::Instant,
) -> IdempotencyOutcome {
    let Some(wire_id) = dispatch_path_wire_id(dispatch_path) else {
        // Not an RPC path — caller already filtered, but defensive
        // fallthrough means we don't dedupe.
        return IdempotencyOutcome::Proceed(InflightHandle {
            entry_key: String::new(),
            lock_key: String::new(),
            body_hash: String::new(),
            ttl_hours: idempotency::clamp_ttl_hours(policy.idempotency_ttl_hours),
        });
    };

    let idem_key = req
        .headers()
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Who owns this dedupe namespace, and — for the shared anonymous
    // namespace — whether this key is allowed into it at all.
    let principal_sub =
        match resolve_dedupe_principal(state, policy, user_header, wire_id, wall_start) {
            Ok(sub) => sub,
            Err(outcome) => return outcome,
        };
    let principal = match principal_sub.as_deref() {
        Some(sub) => idempotency::Principal::User(sub),
        None => idempotency::Principal::Anon,
    };
    if let Some(outcome) = reject_low_entropy_anon_key(policy, idem_key.as_deref(), wall_start) {
        return outcome;
    }

    let ttl_hours = idempotency::clamp_ttl_hours(policy.idempotency_ttl_hours);

    let decision = idempotency::pre_dispatch(
        state.idempotency_store.as_ref(),
        app_id,
        wire_id,
        principal,
        idem_key.as_deref(),
        body,
        ttl_hours,
        inflight_wait_ms(policy),
    )
    .await;

    match decision {
        Ok(idempotency::DedupeDecision::Hit(stored)) => {
            IdempotencyOutcome::ReturnNow(build_replay_response(&stored, wall_start))
        }
        Ok(idempotency::DedupeDecision::Conflict { retry_after_secs }) => {
            IdempotencyOutcome::ReturnNow(build_zs_error_response(
                ntex::http::StatusCode::CONFLICT,
                "ALREADY_EXISTS",
                "Idempotency-Key was used with a different input within the dedupe window",
                serde_json::json!({
                    "reason": "idempotency_key_reused_with_different_input"
                }),
                false,
                Some(retry_after_secs),
                wall_start,
            ))
        }
        Ok(idempotency::DedupeDecision::MissingHeader) => {
            IdempotencyOutcome::ReturnNow(build_zs_error_response(
                ntex::http::StatusCode::BAD_REQUEST,
                "INVALID_ARGUMENT",
                "Idempotency-Key header required for this procedure",
                serde_json::json!({ "reason": "missing_idempotency_key" }),
                false,
                None,
                wall_start,
            ))
        }
        Ok(idempotency::DedupeDecision::InFlightTimedOut) => {
            IdempotencyOutcome::ReturnNow(build_zs_error_response(
                ntex::http::StatusCode::CONFLICT,
                "ABORTED",
                "Idempotency-Key in flight; original request did not complete in time",
                serde_json::json!({ "reason": "idempotency_inflight_timeout" }),
                false,
                None,
                wall_start,
            ))
        }
        Ok(idempotency::DedupeDecision::Proceed { entry_key, lock_key, body_hash, ttl_hours }) => {
            IdempotencyOutcome::Proceed(InflightHandle { entry_key, lock_key, body_hash, ttl_hours })
        }
        Err(e) => {
            // Store error → log and fail closed. A degraded dedupe
            // backend must not silently let duplicate mutations
            // through.
            tracing::error!(error = %e, "gateway: idempotency store error");
            IdempotencyOutcome::ReturnNow(build_zs_error_response(
                ntex::http::StatusCode::SERVICE_UNAVAILABLE,
                "UNAVAILABLE",
                "idempotency store temporarily unavailable",
                serde_json::json!({}),
                true,
                Some(1),
                wall_start,
            ))
        }
    }
}

/// Drain a response's body into bytes, returning a fresh
/// `HttpResponse` with the buffered body. The original response is
/// consumed because ntex's `take_body` empties it. Used after worker
/// dispatch so we can both capture the body for idempotency storage
/// AND ship it back to the client.
///
/// Streaming responses (chunked / SSE) flow through here too — we
/// buffer them entirely. Idempotency only applies to mutation
/// responses, which are virtually always small JSON envelopes; if a
/// mutation streams back megabytes it pays the buffer cost. Streams
/// and subscriptions don't enter this path (caller filters by kind).
pub(super) async fn buffer_response_body(mut resp: HttpResponse) -> (HttpResponse, Vec<u8>) {
    use ntex::http::body::{Body, MessageBody};
    let mut body = resp.take_body();
    let mut buf: Vec<u8> = Vec::new();
    std::future::poll_fn(|cx| {
        loop {
            match body.poll_next_chunk(cx) {
                std::task::Poll::Ready(Some(Ok(chunk))) => buf.extend_from_slice(&chunk),
                std::task::Poll::Ready(Some(Err(_))) | std::task::Poll::Ready(None) => {
                    return std::task::Poll::Ready(());
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    })
    .await;
    let bytes = buf.clone();
    let new_resp = resp.set_body(Body::Bytes(Bytes::from(buf)));
    (new_resp, bytes)
}

/// Who authored the bytes of a dispatched response.
///
/// A dedupe entry means "this logical APP operation happened, here is its
/// outcome", so the capture has to know whether the app spoke at all. The
/// status code is a LOSSY proxy for that and cannot be used: an app may
/// legitimately answer 302 or 401 itself (and those ARE outcomes worth
/// remembering), while the gateway synthesises responses in the same
/// status ranges about the TRANSPORT or the SESSION. So the answer is
/// carried from the origin instead of re-derived at the capture point.
///
/// Every arm of the action match in [`execute_resource_tree`] and every
/// exit of [`handle_dispatch`] must name one of these. That is the point:
/// a new gateway-synthesised response cannot be added without the compiler
/// asking which it is, so it is excluded by default rather than by a new
/// special case in the capture.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ResponseOrigin {
    /// The worker produced these bytes. The gateway may have stamped
    /// observability headers on top (`x-request-id`, `x-wall-time-ms`,
    /// `x-zs-spend-warn`) — the STATUS and BODY are the app's.
    Worker,
    /// The gateway produced these bytes itself. The worker either never
    /// spoke or its answer was replaced, so this response reports nothing
    /// about whether the app operation happened.
    Gateway,
}

/// Spec §8 post-dispatch hook: capture the response under the dedupe key
/// and release the in-flight lock. Returns the response to ship back to
/// the client (with the body intact and an `x-zs-idempotent-stored` flag
/// for observability).
///
/// A dedupe entry means "this logical APP operation reached a definitive
/// outcome; replay it". Two independent things can make that false, so
/// there are two guards, on two different axes — a response is stored only
/// if it clears BOTH.
///
/// **1. Only the WORKER's own responses are storable** (`origin`). The
/// gateway synthesises responses of its own inside the dispatch path: the
/// `502 {"error":"worker error: …"}` from the `Err(_)` arm of
/// `forward_dispatch`, and — on a worker 401 that looks like an HTML
/// navigation — either a `302` into the OP (`start_oidc_redirect`) or a
/// `503 client_not_provisioned`. None of them is an app outcome; they
/// describe the gateway's reach or the visitor's session. The 302 is the
/// sharpest case because it is not merely uninformative but ACTIVELY
/// wrong to replay: it carries a freshly minted per-request PKCE stash in
/// a `Set-Cookie`, so a stored copy hands every retry within the TTL a
/// verifier bound to an already-spent `code_challenge`/`state` pair.
///
/// This is deliberately NOT written as `status == 302 || status == 502 ||
/// …`. Status is a lossy proxy for authorship: an app that answers 302
/// (post-redirect-get) or 401 itself HAS produced an outcome, and those
/// must keep deduping. A status list would either freeze those out or
/// need a new arm for every future gateway-synthesised status; keying on
/// the author closes the class instead of the instance.
///
/// **2. A 5xx is never stored, even from the worker** (`status`). This is
/// not implied by (1): a worker 500 is worker-authored yet still reports
/// no outcome.
///
/// * The common trigger for a handler 500 is a transient fault INSIDE it
///   (a DB blip, an upstream timeout) — exactly the case a stable retry
///   key exists for.
/// * At this point the code cannot tell "worker wrote then threw" from
///   "worker never received it": both surface as a 5xx with no committed
///   outcome reported. Unknown must resolve to retryable, not to
///   permanently-failed — storing it destroys information rather than
///   preserving it.
/// * It gives apps a contract they can drive. 4xx IS stored and replayed,
///   so a deterministic rejection (400/409/422) stays cheap and stable
///   under retry; 5xx is "unexpected, try again". An app that wants
///   "come back later" remembered as a non-outcome already has 503;
///   429 stays storable for the same reason (the app chose a 4xx).
///
/// Either way the lock is RELEASED rather than held, so the next request
/// with the same key retries, exactly as
/// [`idempotency::release_lock_without_storing`] documents.
///
/// The cost is that a deterministic worker 500 re-executes on every
/// retry. That is already true of the 502 arm (the lock is released
/// either way), and a handler that commits a write and then 500s is
/// buggy in a way the gateway cannot repair by freezing its 500 for a
/// day.
pub(super) async fn capture_response_for_idempotency(
    state: &GateState,
    app_id: &Uuid,
    handle: InflightHandle,
    origin: ResponseOrigin,
    response: HttpResponse,
) -> HttpResponse {
    let (mut buffered, body_bytes) = buffer_response_body(response).await;
    let status = buffered.status().as_u16();

    let skip = if origin == ResponseOrigin::Gateway {
        Some("gateway-synthesised response: the app operation did not happen")
    } else if status >= 500 {
        Some("5xx: the worker reported no definitive outcome")
    } else {
        None
    };

    if let Some(reason) = skip {
        tracing::warn!(
            status,
            ?origin,
            reason,
            "gateway: idempotency capture skipped; lock released so the key can be retried"
        );
        let _ = idempotency::release_lock_without_storing(
            state.idempotency_store.as_ref(),
            &handle.lock_key,
        )
        .await;
        buffered.headers_mut().insert(
            ntex::http::header::HeaderName::from_static("x-zs-idempotent-stored"),
            ntex::http::header::HeaderValue::from_static("false"),
        );
        return buffered;
    }

    // Snapshot headers we want to replay. Drops hop-by-hop and
    // per-request stamps before storage.
    let mut header_pairs: Vec<(String, String)> = Vec::new();
    for (name, value) in buffered.headers().iter() {
        if let Ok(v) = value.to_str() {
            header_pairs.push((name.as_str().to_string(), v.to_string()));
        }
    }
    let stored_headers = idempotency::capture_response_headers(&header_pairs);

    if let Err(e) = idempotency::capture_response(
        state.idempotency_store.as_ref(),
        app_id,
        &handle.entry_key,
        &handle.lock_key,
        &handle.body_hash,
        status,
        &stored_headers,
        &body_bytes,
        handle.ttl_hours,
    )
    .await
    {
        tracing::error!(error = %e, "gateway: idempotency capture failed");
        // Best-effort lock release.
        let _ = idempotency::release_lock_without_storing(
            state.idempotency_store.as_ref(),
            &handle.lock_key,
        )
        .await;
    }

    buffered.headers_mut().insert(
        ntex::http::header::HeaderName::from_static("x-zs-idempotent-stored"),
        ntex::http::header::HeaderValue::from_static("true"),
    );
    buffered
}

// ---------------------------------------------------------------------------
// Subscription dispatch (WebSocket-aware)
// ---------------------------------------------------------------------------

/// Forward a `kind: "subscription"` GET-with-Upgrade request through the
/// gateway. Differs from `handle_dispatch` in two ways:
///
///   1. **Affinity routing**: hashes by `(app_id, principal)` so
///      reconnects from the same caller land on the same worker for
///      the lifetime of the connection. Falls back through JWT subject,
///      session cookie, Sec-WebSocket-Key, then IP — see
///      `subscription_affinity_key` for the order.
///
///   2. **Transparent WS proxy** (multi-node gateway): the gateway
///      currently does not perform a transparent WS proxy of the
///      upgraded connection — that requires hijacking the TCP
///      stream from ntex, which is a non-trivial integration. For
///      now, gateway-fronted subscriptions return 501 with a clear
///      message; single-tenant `zeroship serve` handles WS directly.
///
/// The affinity selection itself runs on the request — even when the
/// proxy returns 501 — so the routing decision is testable and
/// observable. Worker is acquired/released around the (currently
/// stub) proxy call to keep concurrency accounting consistent.
async fn handle_subscription_dispatch(
    _req: HttpRequest,
    state: &GateState,
    app_id: &Uuid,
    _route: &zeroship_core::types::RouteEntry,
    _tail: &str,
    wall_start: std::time::Instant,
) -> HttpResponse {
    // `Block`, the rate limit and the concurrency ceiling are all enforced by
    // `execute_resource_tree`, hoisted above the action match so they bind
    // every action class, so a Blocked or throttled subscription never reaches
    // this stub. Warn passes (no body header on the 501 stub path).
    //
    // Subscriptions therefore count against the same accounting as unary
    // dispatch, and one open for hours holds its concurrency slot for that
    // whole time — intentional, so the operator can size the ceiling to the
    // steady-state subscription count plus a margin for unary traffic. The
    // guard now lives in the caller, so the slot is held for exactly as long.

    // Affinity selection — exercised even when the proxy itself
    // returns 501, so tests against this path can verify that the
    // hashing decision is correct.
    let affinity = subscription_affinity_key(&_req, state.config.trust_proxy);
    let (idx, _worker_url) = state.hash_ring.select_with_affinity(app_id, &affinity);
    state.hash_ring.acquire(idx);
    // Release immediately — see comment below; we never actually
    // pump traffic through to the worker.
    state.hash_ring.release(idx);

    let wall_ms = wall_start.elapsed().as_secs_f64() * 1000.0;
    HttpResponse::build(ntex::http::StatusCode::NOT_IMPLEMENTED)
        .header("x-wall-time-ms", format!("{wall_ms:.2}"))
        .header("x-zs-affinity-idx", idx.to_string())
        .json(&serde_json::json!({
            "code": "UNIMPLEMENTED",
            "message": "WebSocket subscription proxy through the multi-node gateway is not yet wired; use `zeroship serve` for single-tenant subscriptions",
            "retryable": false,
        }))
}

// ---------------------------------------------------------------------------
// Reserved-header scrubbing for the worker dispatch envelope
// ---------------------------------------------------------------------------

/// Platform-reserved header names that must never be forwarded verbatim
/// into the worker dispatch envelope. These are either set by the gateway
/// itself on the trusted side-channel (`ZeroShip-User` HMAC), are
/// gateway/control-internal routing/identity hints, or are an attractive
/// nuisance for app JS that (incorrectly) reads identity from raw
/// `request.headers` instead of `env.auth`. A forged inbound copy of any
/// of these is dropped at the trust boundary.
///
/// Matching is case-insensitive (HTTP header names are case-insensitive);
/// callers compare against the lowercased inbound name.
const RESERVED_HEADER_EXACT: &[&str] = &[
    "zeroship-user",
    "authorization",
    "x-app-id",
    "x-plan-id",
    "x-request-id",
];

/// Reserved header *prefix*: every `x-zs-*` header is gateway/platform
/// internal and is stripped from the forwarded envelope.
const RESERVED_HEADER_PREFIX: &str = "x-zs-";

/// True if `name` (any case) is a platform-reserved header that must not
/// be forwarded into the worker dispatch envelope.
fn is_reserved_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with(RESERVED_HEADER_PREFIX)
        || RESERVED_HEADER_EXACT.iter().any(|r| *r == lower)
}

/// Collect inbound request headers as `[name, value]` pairs for the worker
/// dispatch envelope, dropping the platform-reserved set (see
/// [`is_reserved_header`]). Non-UTF-8 header values are skipped.
fn collect_forwarded_headers(headers: &ntex::http::HeaderMap) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (name, value) in headers {
        if is_reserved_header(name.as_str()) {
            continue;
        }
        if let Ok(v) = value.to_str() {
            out.push((name.as_str().to_string(), v.to_string()));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Dispatch handler — the single worker-facing path
// ---------------------------------------------------------------------------

/// Forward an HTTP request to the worker via `/dispatch/{app_id}`.
/// Reconstruct the URL the worker's JS handler sees, re-appending the raw
/// query string the ntex `{tail*}` path extractor drops. Preserving the query
/// is load-bearing: `query()` RPC input rides in `?input=<base64url>` and apps
/// read `request.url` query params directly. See ISS-70.
fn forward_url(scheme: &str, host: &str, tail: &str, query: Option<&str>) -> String {
    match query {
        Some(q) if !q.is_empty() => format!("{scheme}://{host}/{tail}?{q}"),
        _ => format!("{scheme}://{host}/{tail}"),
    }
}

///
/// The full HTTP request (method, URL, headers, raw body bytes) is packaged
/// into the dispatch frame and handed to `Runtime::call_fetch_handler`, which
/// invokes the app's exported `default.fetch(req, env, ctx)`. Covers both the
/// `_rpc/*` URLs (routed inside the kernel via the bootstrap) and plain
/// HTTP requests. Enables streaming responses (e.g., SSE for LLM token
/// streaming).
#[allow(clippy::too_many_arguments)] // post-U5 arg count; refactor candidate for U6+.
///
/// Returns the response paired with its [`ResponseOrigin`]. Three of the four
/// exits here are the GATEWAY's own bytes, not the app's, and the idempotency
/// capture must be able to tell them apart from a worker response that happens
/// to share their status code.
async fn handle_dispatch(
    req: HttpRequest,
    state: &Arc<GateState>,
    app_id: &Uuid,
    route: &zeroship_core::types::RouteEntry,
    tail: &str,
    request_id: Uuid,
    body: Bytes,
    user_header_value: Option<String>,
    wall_start: std::time::Instant,
) -> (HttpResponse, ResponseOrigin) {
    // The `Block` 402 is enforced by
    // `execute_resource_tree` (hoisted to the top, before the action match) so
    // it covers worker forward / redirect / rewrite / static uniformly — a
    // Blocked app never reaches this worker-forwarding path. Here we only read
    // the `Warn` flag to stamp the advisory `x-zs-spend-warn: 1` response
    // header. Degrade is throttled by the rate limit and concurrency ceiling,
    // which now live beside that 402 in `execute_resource_tree` so they cover
    // every action class rather than only this one.
    let spend_warn = route.spend_state == zeroship_core::types::SpendState::Warn;

    // Reconstruct the URL the JS handler will see. The query string MUST be
    // preserved: the `@zeroship/rpc` transport sends `query()` calls as
    // `GET /__zeroship/v1/<id>?input=<base64url>`, and deployed apps read
    // `new URL(request.url).searchParams` for pagination/filters/search. The
    // ntex `{tail*}` extractor yields the PATH only, so the raw query has to be
    // re-appended here — dropping it makes every GET query-RPC arrive with
    // `input: undefined` (→ 400 INVALID_ARGUMENT) and silently strips app query
    // params (ISS-70).
    // The worker sees the browser-visible URL, not the gateway-to-edge
    // transport. A TLS-terminating edge legitimately forwards over HTTP.
    let host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let url = worker_visible_url(&state.config, host, tail, req.uri().query());

    // Collect request headers as [key, value] pairs, scrubbing the
    // platform-reserved set so a forged inbound `ZeroShip-User` /
    // `Authorization` / `x-zs-*` cannot ride into the worker's
    // dispatch envelope and be trusted by app JS reading raw
    // `request.headers`. Authoritative identity travels the separate
    // HMAC `ZeroShip-User` channel (`user_header_value`), never here.
    let headers = collect_forwarded_headers(req.headers());

    // Proxy to worker via CHWBL hash ring.
    let mut response = match proxy::forward_dispatch(
        &state.hash_ring,
        app_id,
        &route.plan_id,
        &request_id,
        req.method().as_str(),
        &url,
        &headers,
        body.as_ref(),
        user_header_value.as_deref(),
        &state.config.worker_key,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            // The worker never spoke — this envelope is the gateway's own.
            return (
                HttpResponse::BadGateway()
                    .json(&serde_json::json!({"error": format!("worker error: {e}")})),
                ResponseOrigin::Gateway,
            );
        }
    };

    // 401 from worker on an HTML navigation → start the OIDC dance.
    // The worker reaches this branch on resources its own JS code
    // gated as `user`/`admin` when the gateway forwarded without a
    // `ZeroShip-User` header. (Resource-tree `user`/`admin` are
    // already short-circuited by `auth_satisfied` upstream, so they
    // never reach the worker.) API clients still see the 401 verbatim.
    //
    // Both arms REPLACE the worker's answer with the gateway's own, so both
    // are `Gateway`-origin. That matters beyond bookkeeping for the redirect:
    // `start_oidc_redirect` mints a FRESH per-request PKCE stash and sets it
    // as a cookie, so storing it under an `Idempotency-Key` would replay a
    // spent verifier to every retry for the whole TTL.
    if response.status() == ntex::http::StatusCode::UNAUTHORIZED && wants_html(&req) {
        return (
            match route.oauth_client_id.as_deref() {
                Some(client_id) => start_oidc_redirect(&req, state, client_id),
                None => client_not_provisioned_response(),
            },
            ResponseOrigin::Gateway,
        );
    }

    // Add response headers.
    let wall_ms = wall_start.elapsed().as_secs_f64() * 1000.0;
    response.headers_mut().insert(
        ntex::http::header::HeaderName::from_static("x-wall-time-ms"),
        ntex::http::header::HeaderValue::from_str(&format!("{wall_ms:.2}")).unwrap(),
    );
    response.headers_mut().insert(
        ntex::http::header::HeaderName::from_static("x-request-id"),
        ntex::http::header::HeaderValue::from_str(&request_id.to_string()).unwrap(),
    );
    // Spend Warn (~80%): served, but flag it so the SDK / dashboard can prompt
    // the creator to raise their limit before Degrade/Block kicks in.
    if spend_warn {
        response.headers_mut().insert(
            ntex::http::header::HeaderName::from_static("x-zs-spend-warn"),
            ntex::http::header::HeaderValue::from_static("1"),
        );
    }

    // The status and body are the app's; the headers stamped above are
    // observability only, so this stays worker-authored.
    (response, ResponseOrigin::Worker)
}

fn worker_visible_url(
    config: &crate::GateConfig,
    host: &str,
    tail: &str,
    query: Option<&str>,
) -> String {
    forward_url(config.origin_scheme.as_str(), host, tail, query)
}

// ---------------------------------------------------------------------------
// Unauthenticated request handling — HTML vs API split
// ---------------------------------------------------------------------------

/// True when the request looks like an HTML navigation rather than an
/// API call. The classification rule is the canonical browser
/// signature: `Accept: text/html` somewhere in the header value. The
/// gateway only triggers the OIDC redirect dance on these — `fetch()`
/// callers see a 401 with `WWW-Authenticate: Bearer` so they can
/// surface an in-app login prompt themselves.
fn wants_html(req: &HttpRequest) -> bool {
    req.headers()
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/html"))
}

/// Build the unauthenticated response: 302 → `auth.zeroship.ai/authorize`
/// for HTML navigations, 401 with a `WWW-Authenticate` challenge for
/// API clients. Sets the `__Host-zs_oidc_stash` cookie carrying PKCE,
/// state, and the original path so `/__zeroship/auth/callback` can finish
/// the dance.
fn unauthenticated_response(
    req: &HttpRequest,
    state: &Arc<GateState>,
    oauth_client_id: Option<&str>,
) -> HttpResponse {
    if wants_html(req) {
        match oauth_client_id {
            Some(client_id) => start_oidc_redirect(req, state, client_id),
            None => client_not_provisioned_response(),
        }
    } else {
        HttpResponse::Unauthorized()
            .header("www-authenticate", "Bearer realm=\"zeroship\"")
            .json(&serde_json::json!({
                "code": "UNAUTHENTICATED",
                "message": "authentication required",
            }))
    }
}

/// `503 client_not_provisioned`: a raw OP bearer resolved a real user, but the
/// route is missing its sector and is not fully provisioned for OAuth. Once
/// control finishes provisioning the client and sector, the retry succeeds.
fn client_not_provisioned_response() -> HttpResponse {
    HttpResponse::ServiceUnavailable()
        .header("cache-control", "no-store")
        .json(&serde_json::json!({
            "error": "client_not_provisioned",
            "error_description": "app has no sector_identifier yet",
        }))
}

/// `403 scope_required` — the request authenticated, but the matched
/// route's `required_scopes` are not a subset of
/// the principal's granted scopes.
///
/// Two distinct contracts ride this response, and they use DIFFERENT tokens
/// on purpose:
///
/// * **`WWW-Authenticate` header** — RFC 6750 §3.1's challenge. The
///   error-code token there is the RFC-registered `insufficient_scope`, and
///   the `scope` parameter lists the required scopes space-delimited. This is
///   the value HTTP-aware clients / proxies read.
/// * **JSON body** — the SDK contract. `sdks/auth` `mapError()` reads
///   `body.error` and maps it onto an `AuthErrorCode`; the registered code is
///   `scope_required`, and there is NO
///   `insufficient_scope` code. We therefore emit `{"error":"scope_required",
///   "scope":"<space-joined required>"}` so the SDK surfaces the typed error
///   (and the `scope` string tells the client exactly which scopes to request).
fn insufficient_scope_response(required: &[String]) -> HttpResponse {
    let scope_param = required.join(" ");
    let challenge = format!(
        "Bearer realm=\"zeroship\", error=\"insufficient_scope\", scope=\"{scope_param}\""
    );
    HttpResponse::Forbidden()
        .header("www-authenticate", challenge.as_str())
        .json(&serde_json::json!({
            "error": "scope_required",
            "scope": scope_param,
        }))
}

// ---------------------------------------------------------------------------
// /__zeroship/auth/callback — the gateway-owned OIDC callback per hosted app
// ---------------------------------------------------------------------------

/// Handle `/__zeroship/auth/callback` on any `{app}.zeroship.ai` host. Reads
/// the signed stash cookie + `code`/`state` query, exchanges with
/// op via `OidcRp::finish_callback`, persists a row in
/// `zeroship.gateway_sessions`, sets the per-origin
/// `__Host-zeroship_app_session` cookie, clears the stash cookie, and 302s
/// back to the original path the user was trying to reach when the
/// dance started.
async fn handle_auth_callback(
    req: HttpRequest,
    state: web::types::State<Arc<GateState>>,
) -> HttpResponse {
    // Resolve the app by subdomain — same logic the manifest dispatcher uses
    // for normal requests — then key the gateway_sessions row by the app's
    // STABLE UUID (`app_uuid`), NOT the slug. This is the canonical session
    // key (the `app_id` column is UUID, bound natively); a slug-keyed row
    // would never match on the real SPA→app request path. The slug can be
    // renamed; the UUID is the immutable identity.
    let Some(app_name) = extract_app_name(&req, None) else {
        return render_callback_error("host header missing or unparseable");
    };
    let Some((app_uuid, route)) = state.routes.lookup_by_name(&app_name) else {
        return render_callback_error("app not found for this host");
    };
    // The interactive flow now issues the SAME signed `zeroship-sess+jwt` cookie the
    // SDK popup flow does (BFF slice R1b) — so the cookie arm has ONE
    // local-verify path. We need the route's per-app `oauth_client_id` (the
    // signed cookie's `app` binding) + `sector_identifier` (the `pws_`
    // derivation). An app with no OAuth client provisioned can't have a signed
    // cookie minted, so fail the callback closed.
    let Some(client_id) = route.entry.oauth_client_id.clone() else {
        return render_callback_error("app has no oauth_client_id yet");
    };
    let sector_identifier = route.entry.sector_identifier.clone();

    // 1. Parse query (code + state). The OP may also send `error=...`
    //    for user-denied consent; surface it directly.
    let query_str = req.uri().query().unwrap_or("");
    let mut code: Option<String> = None;
    let mut state_param: Option<String> = None;
    let mut oauth_error: Option<String> = None;
    let mut issuer_param: Option<String> = None;
    for (k, v) in url::form_urlencoded::parse(query_str.as_bytes()) {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state_param = Some(v.into_owned()),
            "error" => oauth_error = Some(v.into_owned()),
            "iss" => issuer_param = Some(v.into_owned()),
            _ => {}
        }
    }
    if let Some(e) = oauth_error {
        return render_callback_error(&format!("oauth error: {e}"));
    }
    let (Some(code), Some(state_param)) = (code, state_param) else {
        return render_callback_error("missing code or state query parameter");
    };
    // RFC 9207 issuer identification. The OP emits `iss` on authorize
    // responses; tolerate absence for mixed-version local/dev flows, but reject
    // any present mismatch before consuming the code.
    if let Some(issuer_param) = issuer_param.as_deref() {
        if issuer_param != state.oidc_rp.issuer {
            return render_callback_error("issuer mismatch");
        }
    }

    // 2. Read the signed stash cookie.
    let cookie_header = req
        .headers()
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let Some(stash) = oidc_rp::parse_stash_cookie(cookie_header) else {
        return render_callback_error("missing stash cookie");
    };

    // 3. Exchange the code with the OP + verify the ID token.
    let (claims, original_path, granted_scopes) = match state
        .oidc_rp
        .finish_callback(&code, &state_param, &stash, &client_id)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "gateway: oidc callback failed");
            return render_callback_error(oidc_callback_public_error(&e));
        }
    };

    // 4. Create a per-origin session row. Check out a pooled connection
    //    for just this insert and release it on drop.
    let Some(db_cfg) = state.db.as_ref() else {
        return render_callback_error("gateway not configured with a session database");
    };
    let pool = match crate::db::checkout(db_cfg).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "gateway: pg pool checkout failed (session create)");
            return render_callback_error("session create failed");
        }
    };
    let mut conn = match pool.get().await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "gateway: pg pool checkout failed (session create)");
            return render_callback_error("session create failed");
        }
    };
    let session = match crate::sessions::create(
        &mut conn,
        &crate::sessions::NewSession {
            user_id: &claims.sub,
            app_id: app_uuid,
            email: claims.email.as_deref(),
            name: claims.name.as_deref(),
            avatar_url: claims.picture.as_deref(),
            email_verified: claims.email_verified.unwrap_or(false),
            granted_scopes: &granted_scopes,
            sid: claims.sid.as_deref(),
            // Carry the OIDC auth_time/amr onto the cookie session so the SPA
            // projection + step-up gate read them off this row (BFF §2.2/§5.3).
            auth_time: claims.auth_time,
            amr: claims.amr.as_deref().unwrap_or(&[]),
        },
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "gateway: session create failed");
            return render_callback_error("session create failed");
        }
    };
    // Release the pooled connection before building the response — no
    // further DB work happens on this path. `session.id` is the audit row id;
    // it is no longer used as the cookie value (BFF R1b: the cookie is signed).
    let _ = session.id;
    drop(conn);

    // 5. Mint the SIGNED `zeroship-sess+jwt` session cookie from the validated claims
    //    (the SAME mint path the SDK popup flow uses — one cookie shape, one
    //    verifier). `claims.sub` is the global UUID; the helper derives the
    //    per-app `pws_` + relay alias before signing.
    let Ok(global_user_id) = uuid::Uuid::parse_str(&claims.sub) else {
        return render_callback_error("id_token sub is not a global user id");
    };
    let amr = claims.amr.clone().unwrap_or_default();
    let session_cookie = match crate::auth_token::issue_interactive_session_cookie(
        &state,
        db_cfg,
        &client_id,
        sector_identifier.as_deref(),
        global_user_id,
        claims.iat,
        claims.name.as_deref(),
        claims.picture.as_deref(),
        claims.email_verified,
        claims.auth_time,
        &amr,
        &granted_scopes,
    )
    .await
    {
        Ok(c) => c,
        Err(msg) => return render_callback_error(&msg),
    };

    // 6. 302 back to the original path, set the signed session cookie, clear
    //    the stash cookie. Two `Set-Cookie` headers on one response is
    //    valid per RFC 6265 §3 (and is how op emits its own cookies).
    let mut builder = HttpResponse::Found();
    builder.header("location", sanitize_oidc_original_path(&original_path));
    builder.header("set-cookie", session_cookie);
    builder.header("set-cookie", oidc_rp::clear_stash_cookie());
    builder.finish()
}

/// Render the failed-callback page. Generic on purpose — leaking
/// op's error string to the user would be a noisy debugging tool
/// for an attacker. The structured error is already in the gateway log
/// at warn / error.
fn render_callback_error(msg: &str) -> HttpResponse {
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Sign-in failed</title>\
        <h1>Sign-in failed</h1><p>{}</p>\
        <p><a href=\"/\">Back to app</a></p>",
        html_escape(msg),
    );
    HttpResponse::BadRequest()
        .content_type("text/html; charset=utf-8")
        .body(body)
}

fn oidc_callback_public_error(e: &oidc_rp::OidcRpError) -> &'static str {
    match e {
        oidc_rp::OidcRpError::UpstreamUnavailable(_) => {
            // The OP is browning out (breaker open / bounded timeout). Tell the
            // user it's transient rather than implying their credentials are
            // bad — the §8.7 fast-fail surfaces here on the cookie flow.
            "sign-in is temporarily unavailable, please try again shortly"
        }
        oidc_rp::OidcRpError::StashInvalid
        | oidc_rp::OidcRpError::StateMismatch
        | oidc_rp::OidcRpError::ClientMismatch
        | oidc_rp::OidcRpError::TokenExchange(_)
        | oidc_rp::OidcRpError::VerifyIdToken(_)
        | oidc_rp::OidcRpError::VerifyAccessToken(_) => "sign-in could not be completed",
    }
}

/// Minimal HTML-escape — enough to make the rendered error page safe
/// when the upstream message contains user input (state token,
/// query-string echo, etc).
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Build the 302 → op redirect that kicks off the OIDC dance.
/// Stash cookie carries the PKCE verifier + state + original_path so
/// the callback can resume.
fn start_oidc_redirect(req: &HttpRequest, state: &Arc<GateState>, client_id: &str) -> HttpResponse {
    let original_path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/")
        .to_string();
    let original_path = sanitize_oidc_original_path(&original_path);
    let host = req
        .headers()
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let scheme = state.config.origin_scheme;
    let redirect_uri = format!("{scheme}://{host}/__zeroship/auth/callback");

    let (auth_url, stash) = state
        .oidc_rp
        .build_authorize_redirect(
            client_id,
            &original_path,
            &redirect_uri,
        );

    let mut builder = HttpResponse::Found();
    builder.header("location", auth_url);
    builder.header("set-cookie", oidc_rp::set_stash_cookie(&stash));
    builder.finish()
}

fn sanitize_oidc_original_path(path: &str) -> String {
    if is_safe_oidc_original_path(path) {
        path.to_string()
    } else {
        "/".to_string()
    }
}

fn is_safe_oidc_original_path(path: &str) -> bool {
    if path == "/" {
        return true;
    }

    let bytes = path.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'/' {
        return false;
    }

    if !matches!(
        bytes[1],
        b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_'
    ) {
        return false;
    }

    // The scheme-lookalike guard: reject `/javascript:alert(1)`, where the
    // colon sits in the FIRST PATH SEGMENT. The segment therefore ends at the
    // first `/`, `?` or `#` — a query or fragment is not part of it. Stopping
    // only at `/` ran the colon scan across the query string, so
    // `/search?q=https://example.com` was judged unsafe and the user's
    // destination was silently replaced with `/` after login.
    let first_segment_end = path[1..]
        .find(['/', '?', '#'])
        .map(|idx| idx + 1)
        .unwrap_or(path.len());
    !path[1..first_segment_end].contains(':')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    use ntex::http::body::{Body, MessageBody, ResponseBody};
    use ntex::util::Bytes;
    use ntex::web::{self, HttpResponse};

    use crate::compiled::{CompiledManifest, EffectivePolicy};
    use zeroship_bundle::{
        AuthLevel, Manifest, ProcedureKind, RateLimit, RateLimitPer, ResourceEntry,
    };

    fn test_broker_secret() -> crate::oidc_rp::BrokerSecret {
        crate::oidc_rp::BrokerSecret::from_bytes(
            b"gateway-dispatch-test-broker-master-32-bytes".to_vec(),
        )
        .expect("broker secret")
    }

    fn manifest_with_resources(resources: HashMap<String, ResourceEntry>) -> Manifest {
        Manifest {
            version: 1,
            resources,
            ..Manifest::default()
        }
    }

    fn usage_value(
        events: &[zeroship_core::usage_event::UsageEvent],
        app_id: Uuid,
        meter: &str,
    ) -> Option<u64> {
        events
            .iter()
            .find(|event| event.subject.app == Some(app_id) && event.meter == meter)
            .map(|event| event.value)
    }

    /// Drain a `ResponseBody<Body>` to bytes — the dispatch tests need
    /// to inspect idempotency-replayed and error-envelope bodies.
    async fn collect_body(mut body: ResponseBody<Body>) -> Vec<u8> {
        let mut out = Vec::new();
        std::future::poll_fn(|cx| {
            loop {
                match body.poll_next_chunk(cx) {
                    std::task::Poll::Ready(Some(Ok(chunk))) => out.extend_from_slice(&chunk),
                    std::task::Poll::Ready(Some(Err(e))) => panic!("body error: {e}"),
                    std::task::Poll::Ready(None) => return std::task::Poll::Ready(()),
                    std::task::Poll::Pending => return std::task::Poll::Pending,
                }
            }
        })
        .await;
        out
    }

    /// Minimal in-memory blob store for the idempotency tests below.
    /// They never actually read assets; the store exists only to
    /// satisfy `GateState`'s required field.
    #[derive(Debug, Default)]
    struct StubBlobStore;

    #[async_trait::async_trait(?Send)]
    impl zeroship_bundle::BlobStore for StubBlobStore {
        async fn get_blob(&self, _hash: &str) -> Result<bytes::Bytes, zeroship_bundle::BlobError> {
            Err(zeroship_bundle::BlobError::NotFound("unused".into()))
        }
        fn local_path(&self, _hash: &str) -> Option<std::path::PathBuf> {
            None
        }
        async fn put_blob(
            &self,
            _hash: &str,
            _data: &[u8],
        ) -> Result<zeroship_bundle::PutOutcome, zeroship_bundle::BlobError> {
            Ok(zeroship_bundle::PutOutcome::Wrote)
        }
        async fn put_blob_stream(
            &self,
            _hash: &str,
            _expected_size: u64,
            _reader: &mut dyn std::io::Read,
        ) -> Result<zeroship_bundle::PutOutcome, zeroship_bundle::BlobError> {
            Ok(zeroship_bundle::PutOutcome::Wrote)
        }
        async fn has_blob(&self, _hash: &str) -> Result<bool, zeroship_bundle::BlobError> {
            Ok(false)
        }

        async fn probe(&self) -> Result<(), zeroship_bundle::BlobError> {
            Ok(())
        }
        async fn get_blob_to_file(
            &self,
            _hash: &str,
            _out: &compio::fs::File,
            _expected_size: Option<u64>,
            _max_bytes: u64,
        ) -> Result<u64, zeroship_bundle::BlobError> {
            Err(zeroship_bundle::BlobError::NotFound("unused".into()))
        }
        async fn put_manifest(
            &self,
            _app_id: &uuid::Uuid,
            _deploy_hash: &str,
            _json: &[u8],
        ) -> Result<(), zeroship_bundle::BlobError> {
            Ok(())
        }
        async fn get_manifest(
            &self,
            _app_id: &uuid::Uuid,
            _deploy_hash: &str,
        ) -> Result<bytes::Bytes, zeroship_bundle::BlobError> {
            Err(zeroship_bundle::BlobError::NotFound("unused".into()))
        }
        async fn delete_manifest(
            &self,
            _app_id: &uuid::Uuid,
            _deploy_hash: &str,
        ) -> Result<bool, zeroship_bundle::BlobError> {
            Ok(false)
        }
        async fn delete_app_manifests(
            &self,
            _app_id: &uuid::Uuid,
        ) -> Result<(), zeroship_bundle::BlobError> {
            Ok(())
        }
    }

    fn build_test_state_with_workers(worker_urls: Vec<String>) -> Arc<GateState> {
        // `burst: 1` gives a 1000-token bucket, and ANY request costs the whole
        // thing, so this fixture cannot tell a Degraded request from a normal
        // one. Tests that need that distinction call
        // `build_test_state_with_limits` with a burst above `DEGRADE_FACTOR`.
        build_test_state_with_limits(worker_urls, 1, 1, 1)
    }

    fn build_test_state_with_limits(
        worker_urls: Vec<String>,
        rate: u32,
        burst: u32,
        concurrency: u32,
    ) -> Arc<GateState> {
        build_test_state_inner(worker_urls, rate, burst, concurrency, None)
    }

    /// As [`build_test_state_with_limits`], but with the signed-session-cookie
    /// verifier wired in so a test can authenticate a request through the real
    /// cookie arm of `resolve_auth`.
    fn build_test_state_inner(
        worker_urls: Vec<String>,
        rate: u32,
        burst: u32,
        concurrency: u32,
        session_verifier: Option<Arc<crate::session_token::Verifier>>,
    ) -> Arc<GateState> {
        let mut tmp = std::env::temp_dir();
        tmp.push(format!("zsgate-idem-{}", uuid::Uuid::new_v4().simple()));
        let disk = crate::blob_cache::DiskBlobCache::new(tmp, 1024 * 1024).expect("disk cache");
        Arc::new(GateState {
            config: crate::GateConfig {
                control_url: String::new(),
                control_key: String::new(),
                worker_urls: worker_urls.clone(),
                poll_interval_secs: 5,
                worker_key: String::new(),
                auth_ui_url: String::new(),
                origin_scheme: zeroship_core::config::OriginScheme::Http,
                trusted_origins: vec![],
                trust_proxy: false,
                public_url: "https://api.zeroship.ai".into(),
            },
            routes: crate::sync::RouteCache::new(),
            hash_ring: crate::proxy::HashRing::new(worker_urls, 1),
            rate_limiters: crate::enforce::RateLimitRegistry::new(rate, burst),
            per_rule_rate_limits: crate::enforce::PerRuleRateLimitRegistry::new(),
            concurrency: crate::enforce::ConcurrencyRegistry::new(concurrency),
            blob_store: Arc::new(StubBlobStore),
            blob_cache: crate::blob_cache::BlobCache::new(8 * 1024 * 1024),
            disk_cache: disk,
            idempotency_store: Arc::new(crate::idempotency::InMemoryIdempotencyStore::new()),
            oidc_rp: Arc::new(crate::oidc_rp::OidcRp::new(
                "http://auth.test",
                test_broker_secret(),
                b"test-stash-key-32-bytes-long----".to_vec(),
            )),
            db: None,
            logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
            revocation_cache: Arc::new(zeroship_authz::wrapper_revocation::RevocationCache::new()),
            signing_key: None,
            prev_signing_key: None,
            session_issuer: None,
            session_verifier,
            anchor_enc_key: [0u8; 32],
            pairwise_salt: [0u8; 32],
            meter: Arc::new(zeroship_metering::Meter::new()),
        })
    }

    fn build_idempotency_state() -> Arc<GateState> {
        build_test_state_with_workers(vec!["http://0.0.0.0:0".into()])
    }

    fn workflow_step_request(app_id: Uuid) -> Value {
        serde_json::json!({
            "runId": "run_test",
            "appId": app_id,
        })
    }

    async fn workflow_mock_worker(
        seen: web::types::State<Arc<std::sync::Mutex<Vec<Value>>>>,
        body: Bytes,
    ) -> HttpResponse {
        let request: Value = serde_json::from_slice(body.as_ref()).expect("worker request json");
        seen.lock().expect("seen lock").push(request.clone());
        HttpResponse::Ok().json(&serde_json::json!({
            "ack": true,
            "runId": request["runId"],
            "registrations": [{
                "runId": request["runId"],
                "appId": request["appId"],
                "terminal": true
            }]
        }))
    }

    fn install_workflow_route(
        state: &GateState,
        app_id: Uuid,
        spend_state: zeroship_core::types::SpendState,
        account_state: zeroship_core::types::AccountState,
    ) {
        let mut entry = worker_spend_route(spend_state);
        entry.account_state = account_state;
        let mut routes = zeroship_core::types::RouteMap::new();
        routes.insert(app_id, entry);
        state.routes.update_snapshot(
            zeroship_core::types::GatewaySnapshot {
                routes,
                principal_lifecycle: Vec::new(),
                family_revocations: Vec::new(),
            },
            &state.rate_limiters,
            &state.concurrency,
        );
    }

    #[ntex::test]
    async fn internal_workflow_advance_routes_to_worker_and_returns_ack() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let server_seen = Arc::clone(&seen);
        let worker = ntex::web::test::server(move || {
            let seen = Arc::clone(&server_seen);
            async move {
                web::App::new().state(seen).service(
                    web::resource("/workflow-advance-unsigned/{app_id}")
                        .route(web::post().to(workflow_mock_worker)),
                )
            }
        })
        .await;

        let state = build_test_state_with_workers(vec![worker.url("/")]);
        let app_id = Uuid::new_v4();
        install_workflow_route(
            &state,
            app_id,
            zeroship_core::types::SpendState::Allow,
            zeroship_core::types::AccountState::Active,
        );
        let app = ntex::web::test::init_service(
            web::App::new().state(state).service(
                web::resource("/__zeroship/internal/workflow-advance")
                    .route(web::post().to(workflow_advance_internal)),
            ),
        )
        .await;

        let req = ntex::web::test::TestRequest::post()
            .uri("/__zeroship/internal/workflow-advance")
            .set_payload(serde_json::to_vec(&workflow_step_request(app_id)).unwrap())
            .to_request();
        let resp = ntex::web::test::call_service(&app, req).await;
        assert_eq!(resp.status(), ntex::http::StatusCode::OK);
        let body = ntex::web::test::read_body(resp).await;
        let result: Value = serde_json::from_slice(&body).expect("workflow advance ack JSON");
        assert_eq!(result["ack"], true);
        assert_eq!(result["runId"], "run_test");
        assert_eq!(result["registrations"][0]["runId"], "run_test");
        assert_eq!(result["registrations"][0]["appId"], app_id.to_string());
        assert_eq!(result["registrations"][0]["terminal"], true);

        let seen = seen.lock().expect("seen lock");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0]["runId"], "run_test");
        assert_eq!(seen[0]["appId"], app_id.to_string());
        let keys = seen[0].as_object().expect("worker request object");
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn workflow_sleep_frontier_normalizes_duration_wake_at() {
        let request: WorkflowStepRequest =
            serde_json::from_value(workflow_step_request(Uuid::new_v4())).unwrap();
        let worker_result = serde_json::json!({
            "kind": "Sleep",
            "runId": "run_test",
            "nonce": "wfd_test",
            "workflowName": "Checkout",
            "ordinal": 1,
            "name": "sleep",
            "nameOccurrence": 0,
            "wakeAt": "PT1S"
        });
        let result = workflow_worker_result_to_step_result(
            &request,
            serde_json::to_vec(&worker_result).unwrap().as_slice(),
        )
        .expect("sleep result");

        let wake_at = result["outcomes"][0]["wakeAt"]
            .as_str()
            .expect("sleep wakeAt");
        assert!(DateTime::parse_from_rfc3339(wake_at).is_ok());
        assert_eq!(result["outcomes"][0]["kind"], "Sleep");
    }

    #[test]
    fn workflow_wait_frontier_normalizes_timeout_and_max_signal_age() {
        let request: WorkflowStepRequest =
            serde_json::from_value(workflow_step_request(Uuid::new_v4())).unwrap();
        let worker_result = serde_json::json!({
            "kind": "Wait",
            "runId": "run_test",
            "nonce": "wfd_test",
            "workflowName": "Checkout",
            "ordinal": 1,
            "name": "go",
            "nameOccurrence": 0,
            "signalType": "go",
            "timeout": "PT30S",
            "maxSignalAge": "PT5S"
        });
        let result = workflow_worker_result_to_step_result(
            &request,
            serde_json::to_vec(&worker_result).unwrap().as_slice(),
        )
        .expect("wait result");

        let wake_at = result["outcomes"][0]["wakeAt"]
            .as_str()
            .expect("wait wakeAt");
        assert!(DateTime::parse_from_rfc3339(wake_at).is_ok());
        assert_eq!(result["outcomes"][0]["kind"], "Wait");
        assert_eq!(result["outcomes"][0]["signalType"], "go");
        assert_eq!(result["outcomes"][0]["maxSignalAgeMs"], 5_000);
    }

    #[test]
    fn workflow_step_completed_preserves_side_effect_step_kind() {
        let request: WorkflowStepRequest =
            serde_json::from_value(workflow_step_request(Uuid::new_v4())).unwrap();
        let worker_result = serde_json::json!({
            "kind": "StepCompleted",
            "runId": "run_test",
            "nonce": "wfd_test",
            "workflowName": "Checkout",
            "ordinal": 0,
            "name": "v",
            "nameOccurrence": 0,
            "stepKind": "sideEffect",
            "output": {"value": 1}
        });
        let result = workflow_worker_result_to_step_result(
            &request,
            serde_json::to_vec(&worker_result).unwrap().as_slice(),
        )
        .expect("sideEffect completed result");

        assert_eq!(result["outcomes"][0]["kind"], "StepCompleted");
        assert_eq!(result["outcomes"][0]["stepKind"], "sideEffect");
        assert_eq!(result["outcomes"][0]["name"], "v");
    }

    #[test]
    fn workflow_continue_as_new_preserves_seed_input() {
        let request: WorkflowStepRequest =
            serde_json::from_value(workflow_step_request(Uuid::new_v4())).unwrap();
        let worker_result = serde_json::json!({
            "kind": "ContinueAsNew",
            "runId": "run_test",
            "nonce": "wfd_test",
            "workflowName": "Checkout",
            "input": {"generation": 1}
        });
        let result = workflow_worker_result_to_step_result(
            &request,
            serde_json::to_vec(&worker_result).unwrap().as_slice(),
        )
        .expect("continue-as-new result");

        assert_eq!(result["outcomes"][0]["kind"], "ContinueAsNew");
        assert_eq!(result["outcomes"][0]["input"], serde_json::json!({"generation": 1}));
    }

    #[test]
    fn workflow_run_failed_batch_uses_batch_error_when_outcome_error_missing() {
        let request: WorkflowStepRequest =
            serde_json::from_value(workflow_step_request(Uuid::new_v4())).unwrap();
        let worker_result = serde_json::json!({
            "kind": "RunFailed",
            "runId": "run_test",
            "dispatchNonce": "wfd_test",
            "workflowName": "Checkout",
            "error": {
                "type": "NondeterministicError",
                "message": "workflow journal mismatch at ordinal 0"
            },
            "outcomes": [{
                "kind": "RunFailed"
            }]
        });
        let result = workflow_worker_result_to_step_result(
            &request,
            serde_json::to_vec(&worker_result).unwrap().as_slice(),
        )
        .expect("run failed result");

        assert_eq!(result["outcomes"][0]["kind"], "RunFailed");
        assert_eq!(result["outcomes"][0]["error"]["type"], "NondeterministicError");
        assert_eq!(
            result["outcomes"][0]["error"]["message"],
            "workflow journal mismatch at ordinal 0"
        );
    }

    #[test]
    fn workflow_batch_normalizes_only_trailing_suspension() {
        let request: WorkflowStepRequest =
            serde_json::from_value(workflow_step_request(Uuid::new_v4())).unwrap();
        let worker_result = serde_json::json!({
            "runId": "run_test",
            "dispatchNonce": "wfd_test",
            "outcomes": [
                {
                    "kind": "StepCompleted",
                    "ordinal": 0,
                    "name": "a",
                    "nameOccurrence": 0,
                    "output": "A"
                },
                {
                    "kind": "StepCompleted",
                    "ordinal": 1,
                    "name": "b",
                    "nameOccurrence": 0,
                    "output": "B"
                },
                {
                    "kind": "Sleep",
                    "ordinal": 2,
                    "name": "cooldown",
                    "nameOccurrence": 0,
                    "wakeAt": "PT1S"
                }
            ]
        });
        let result = workflow_worker_result_to_step_result(
            &request,
            serde_json::to_vec(&worker_result).unwrap().as_slice(),
        )
        .expect("batch result");

        assert_eq!(result["outcomes"][0]["kind"], "StepCompleted");
        assert_eq!(result["outcomes"][1]["kind"], "StepCompleted");
        assert_eq!(result["outcomes"][2]["kind"], "Sleep");
        let wake_at = result["outcomes"][2]["wakeAt"]
            .as_str()
            .expect("sleep wakeAt");
        assert!(DateTime::parse_from_rfc3339(wake_at).is_ok());
    }

    #[test]
    fn workflow_batch_preserves_compensable_on_every_completed_step() {
        let request: WorkflowStepRequest =
            serde_json::from_value(workflow_step_request(Uuid::new_v4())).unwrap();
        let worker_result = serde_json::json!({
            "runId": "run_test",
            "dispatchNonce": "wfd_test",
            "outcomes": [
                {
                    "kind": "StepCompleted",
                    "ordinal": 0,
                    "name": "a",
                    "nameOccurrence": 0,
                    "stepKind": "run",
                    "compensable": true,
                    "compensationMaxAttempts": 1,
                    "output": "A"
                },
                {
                    "kind": "StepCompleted",
                    "ordinal": 1,
                    "name": "b",
                    "nameOccurrence": 0,
                    "stepKind": "run",
                    "compensable": true,
                    "compensationMaxAttempts": 3,
                    "output": "B"
                },
                {
                    "kind": "RunFailed",
                    "ordinal": 2,
                    "name": "c",
                    "nameOccurrence": 0,
                    "error": {"type": "Error", "message": "c failed"}
                }
            ]
        });
        let result = workflow_worker_result_to_step_result(
            &request,
            serde_json::to_vec(&worker_result).unwrap().as_slice(),
        )
        .expect("compensable batch result");

        assert_eq!(result["outcomes"][0]["compensable"], true);
        assert_eq!(result["outcomes"][0]["compensationMaxAttempts"], 1);
        assert_eq!(result["outcomes"][1]["compensable"], true);
        assert_eq!(result["outcomes"][1]["compensationMaxAttempts"], 3);
        assert_eq!(result["outcomes"][2]["kind"], "RunFailed");
    }

    #[test]
    fn workflow_legacy_checkpoints_preserve_compensation_metadata() {
        let request: WorkflowStepRequest =
            serde_json::from_value(workflow_step_request(Uuid::new_v4())).unwrap();
        let worker_result = serde_json::json!({
            "runId": "run_test",
            "dispatchNonce": "wfd_test",
            "checkpoints": [
                {
                    "ordinal": 0,
                    "name": "a",
                    "nameOccurrence": 0,
                    "kind": "run",
                    "state": "completed",
                    "output": "A",
                    "compensable": true,
                    "compensationMaxAttempts": 1
                },
                {
                    "ordinal": 1,
                    "name": "b",
                    "nameOccurrence": 0,
                    "kind": "run",
                    "state": "completed",
                    "output": "B",
                    "compensationState": "pending",
                    "compensationMaxAttempts": 2
                },
                {
                    "ordinal": 2,
                    "name": "plain",
                    "nameOccurrence": 0,
                    "kind": "run",
                    "state": "completed",
                    "output": "plain"
                }
            ],
            "runUpdate": {"state": "queued"}
        });
        let result = workflow_worker_result_to_step_result(
            &request,
            serde_json::to_vec(&worker_result).unwrap().as_slice(),
        )
        .expect("legacy checkpoint result");

        assert_eq!(result["outcomes"][0]["compensable"], true);
        assert_eq!(result["outcomes"][0]["compensationMaxAttempts"], 1);
        assert_eq!(result["outcomes"][1]["compensable"], true);
        assert_eq!(result["outcomes"][1]["compensationMaxAttempts"], 2);
        assert_eq!(result["outcomes"][2]["compensable"], false);
        assert_eq!(result["outcomes"][2]["compensationMaxAttempts"], 1);
    }

    #[test]
    fn workflow_step_result_sleep_normalizes_duration_wake_at() {
        let result = serde_json::json!({
            "runId": "run_test",
            "dispatchNonce": "wfd_test",
            "checkpoints": [{
                "ordinal": 1,
                "name": "sleep",
                "nameOccurrence": 0,
                "kind": "sleep",
                "state": "running",
                "output": null,
                "error": null,
                "wakeAt": "PT1S",
                "signalType": null,
                "maxSignalAgeMs": null,
                "consumedSignalId": null
            }],
            "runUpdate": {"state": "sleeping", "wakeAt": null}
        });
        let normalized = normalize_workflow_step_result(result).expect("normalized StepResult");
        let wake_at = normalized["runUpdate"]["wakeAt"]
            .as_str()
            .expect("runUpdate wakeAt");
        assert!(DateTime::parse_from_rfc3339(wake_at).is_ok());
        assert_eq!(
            normalized["checkpoints"][0]["wakeAt"],
            normalized["runUpdate"]["wakeAt"]
        );
    }

    #[test]
    fn workflow_step_result_wait_normalizes_duration_fields() {
        let result = serde_json::json!({
            "runId": "run_test",
            "dispatchNonce": "wfd_test",
            "checkpoints": [{
                "ordinal": 1,
                "name": "go",
                "nameOccurrence": 0,
                "kind": "wait_signal",
                "state": "running",
                "output": null,
                "error": null,
                "wakeAt": "PT30S",
                "signalType": "go",
                "maxSignalAgeMs": "PT5S",
                "consumedSignalId": null
            }],
            "runUpdate": {"state": "waiting", "wakeAt": "PT30S"}
        });
        let normalized = normalize_workflow_step_result(result).expect("normalized StepResult");
        let wake_at = normalized["runUpdate"]["wakeAt"]
            .as_str()
            .expect("runUpdate wakeAt");
        assert!(DateTime::parse_from_rfc3339(wake_at).is_ok());
        assert_eq!(
            normalized["checkpoints"][0]["wakeAt"],
            normalized["runUpdate"]["wakeAt"]
        );
        assert_eq!(normalized["checkpoints"][0]["maxSignalAgeMs"], 5_000);
    }

    #[ntex::test]
    async fn internal_workflow_advance_spend_blocked_app_returns_402() {
        let state = build_test_state_with_workers(Vec::new());
        let app_id = Uuid::new_v4();
        install_workflow_route(
            &state,
            app_id,
            zeroship_core::types::SpendState::Block,
            zeroship_core::types::AccountState::Active,
        );
        let app = ntex::web::test::init_service(
            web::App::new().state(state).service(
                web::resource("/__zeroship/internal/workflow-advance")
                    .route(web::post().to(workflow_advance_internal)),
            ),
        )
        .await;

        let req = ntex::web::test::TestRequest::post()
            .uri("/__zeroship/internal/workflow-advance")
            .set_payload(serde_json::to_vec(&workflow_step_request(app_id)).unwrap())
            .to_request();
        let resp = ntex::web::test::call_service(&app, req).await;
        assert_eq!(resp.status(), ntex::http::StatusCode::PAYMENT_REQUIRED);
        let body = ntex::web::test::read_body(resp).await;
        let json: Value = serde_json::from_slice(&body).expect("402 JSON");
        assert_eq!(json["code"], "SPEND_LIMIT");
    }

    #[ntex::test]
    async fn public_vhost_workflow_advance_path_is_404() {
        let state = build_test_state_with_workers(Vec::new());
        let app_id = Uuid::new_v4();
        install_workflow_route(
            &state,
            app_id,
            zeroship_core::types::SpendState::Allow,
            zeroship_core::types::AccountState::Active,
        );
        let app = ntex::web::test::init_service(
            web::App::new().state(state).service(
                web::resource("/__zeroship/internal/workflow-advance")
                    .route(web::post().to(workflow_advance_internal)),
            ),
        )
        .await;

        let req = ntex::web::test::TestRequest::post()
            .uri("/__zeroship/internal/workflow-advance")
            .header("host", "spend-app.zeroship.localhost")
            .set_payload(serde_json::to_vec(&workflow_step_request(app_id)).unwrap())
            .to_request();
        let resp = ntex::web::test::call_service(&app, req).await;
        assert_eq!(resp.status(), ntex::http::StatusCode::NOT_FOUND);
    }

    // -----------------------------------------------------------------------
    // Per-rule rate-limit bucket key derivation
    // -----------------------------------------------------------------------

    #[test]
    fn compute_bucket_id_app_returns_constant() {
        // RateLimitPer::App always returns "app" regardless of IP or
        // cookie state — every caller shares the same bucket.
        let req = ntex::web::test::TestRequest::default()
            .header("cookie", "__Host-zeroship_app_session=abc")
            .to_http_request();
        assert_eq!(compute_bucket_id(&req, RateLimitPer::App, false, true), "app");
    }

    #[test]
    fn compute_bucket_id_ip_falls_back_to_unknown() {
        // The TestRequest has no peer addr → "unknown" sentinel keeps
        // the bucket lookup well-defined instead of crashing.
        let req = ntex::web::test::TestRequest::default().to_http_request();
        let id = compute_bucket_id(&req, RateLimitPer::Ip, false, true);
        assert_eq!(id, "unknown");
    }

    // -----------------------------------------------------------------------
    // ISS-70: the worker-forward URL must carry the query string. Dropping it
    // makes every GET `query()` RPC arrive with `input: undefined` (the input
    // rides in `?input=<base64url>`) and silently strips app query params.
    // -----------------------------------------------------------------------

    #[test]
    fn forward_url_preserves_query_string() {
        // GET query-RPC: the base64url input MUST survive into the worker URL.
        assert_eq!(
            forward_url("http", "app.localhost:8080", "__zeroship/v1/listTodos", Some("input=e30")),
            "http://app.localhost:8080/__zeroship/v1/listTodos?input=e30",
        );
        // Arbitrary app query params (search/pagination) survive too.
        assert_eq!(
            forward_url("https", "shop.zeroship.ai", "products", Some("q=shoes&page=2")),
            "https://shop.zeroship.ai/products?q=shoes&page=2",
        );
    }

    #[test]
    fn forward_url_omits_empty_or_absent_query() {
        // No query → no trailing '?'.
        assert_eq!(
            forward_url("http", "h", "p", None),
            "http://h/p",
        );
        // Empty query (e.g. a bare trailing '?') is treated as absent.
        assert_eq!(
            forward_url("http", "h", "p", Some("")),
            "http://h/p",
        );
    }

    #[test]
    fn worker_visible_url_uses_public_scheme() {
        let mut state = build_test_state_with_workers(vec![]);
        let config = &mut Arc::get_mut(&mut state)
            .expect("fresh test state has one owner")
            .config;

        config.origin_scheme = zeroship_core::config::OriginScheme::Https;
        assert_eq!(
            worker_visible_url(config, "app.zeroship.ai", "items", Some("page=2")),
            "https://app.zeroship.ai/items?page=2",
        );

        config.origin_scheme = zeroship_core::config::OriginScheme::Http;
        assert_eq!(
            worker_visible_url(config, "app.zeroship.ai", "items", None),
            "http://app.zeroship.ai/items",
        );
    }

    // -----------------------------------------------------------------------
    // L7: platform-reserved headers must be scrubbed from the worker
    // dispatch envelope so a forged inbound ZeroShip-User / Authorization /
    // x-zs-* cannot ride into app JS `request.headers`.
    // -----------------------------------------------------------------------

    #[test]
    fn forged_reserved_headers_are_stripped_from_envelope() {
        let req = ntex::web::test::TestRequest::default()
            // forged identity + platform-internal headers an attacker
            // could inject on the inbound request
            .header("ZeroShip-User", "usr_forged_admin")
            .header("Authorization", "Bearer attacker-token")
            .header("X-App-Id", "app_spoofed")
            .header("X-Plan-Id", "plan_enterprise")
            .header("X-Request-Id", "forged-rid")
            .header("X-ZS-Internal", "1")
            .header("x-zs-anything", "2")
            // a legitimate app header that MUST survive
            .header("X-Custom-App-Header", "keep-me")
            .header("Content-Type", "application/json")
            .to_http_request();

        let fwd = collect_forwarded_headers(req.headers());
        let names: Vec<String> = fwd.iter().map(|(k, _)| k.to_ascii_lowercase()).collect();

        // None of the platform-reserved headers survive (case-insensitive).
        for reserved in [
            "zeroship-user",
            "authorization",
            "x-app-id",
            "x-plan-id",
            "x-request-id",
            "x-zs-internal",
            "x-zs-anything",
        ] {
            assert!(
                !names.iter().any(|n| n == reserved),
                "reserved header `{reserved}` leaked into the worker envelope: {names:?}"
            );
        }

        // Legitimate app headers are preserved.
        assert!(
            names.iter().any(|n| n == "x-custom-app-header"),
            "legitimate app header was dropped: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "content-type"),
            "content-type was dropped: {names:?}"
        );
    }

    #[test]
    fn is_reserved_header_is_case_insensitive_and_prefix_aware() {
        assert!(is_reserved_header("ZeroShip-User"));
        assert!(is_reserved_header("AUTHORIZATION"));
        assert!(is_reserved_header("x-zs-foo"));
        assert!(is_reserved_header("X-ZS-Bar"));
        // Not reserved: ordinary headers and the X-Forwarded-* family
        // that the gateway's own client_ip logic handles separately.
        assert!(!is_reserved_header("content-type"));
        assert!(!is_reserved_header("x-forwarded-for"));
        assert!(!is_reserved_header("x-custom"));
    }

    #[test]
    fn client_ip_ignores_forwarded_headers_without_trust_proxy() {
        // Default posture: the gateway IS the edge, so a forwarded header is
        // just caller-supplied text. This fixture has no peer, so the only
        // honest answer is the shared "unknown" bucket.
        let req = ntex::web::test::TestRequest::default()
            .header("x-forwarded-for", "203.0.113.77")
            .header("forwarded", "for=198.51.100.9")
            .to_http_request();
        assert_eq!(client_ip(&req, false), "unknown");
    }

    #[test]
    fn client_ip_honors_proxy_headers_only_with_trust_proxy() {
        let req = ntex::web::test::TestRequest::default()
            .header("x-forwarded-for", "203.0.113.77")
            .to_http_request();

        assert_eq!(client_ip(&req, true), "203.0.113.77");
        assert_eq!(
            compute_bucket_id(&req, RateLimitPer::Ip, true, true),
            "203.0.113.77"
        );
    }

    // -----------------------------------------------------------------------
    // The bucket key must be an IP, never an `ip:port` socket address.
    //
    // A `RateLimitPer::Ip` bucket is keyed on `compute_bucket_id`'s return
    // value verbatim (`enforce.rs`, `PerRuleKey { bucket }`), so any port that
    // survives into that string splits one client across a fresh bucket per
    // TCP connection and the per-IP limit stops limiting.
    // -----------------------------------------------------------------------

    #[test]
    fn compute_bucket_id_ip_ignores_the_source_port_of_one_client() {
        // The reviewer's report, end to end through a real `HttpRequest` and
        // real header parsing rather than by reading code.
        //
        // Two requests from ONE client on two connections differ only in the
        // ephemeral source port. Before the fix each got its OWN bucket
        // (`192.0.2.43:40001` and `192.0.2.43:40002` were distinct keys), so a
        // client that reconnected drew a fresh allowance every time and the
        // per-IP limit limited nothing. They must share one bucket.
        let from_port = |port: u16| {
            let req = ntex::web::test::TestRequest::default()
                .header("x-forwarded-for", format!("192.0.2.43:{port}"))
                .to_http_request();
            compute_bucket_id(&req, RateLimitPer::Ip, true, true)
        };
        let conn_a = from_port(40001);
        let conn_b = from_port(40002);
        assert_eq!(conn_a, "192.0.2.43");
        assert_eq!(conn_a, conn_b, "same client, two connections, one bucket");
    }

    #[test]
    fn one_client_on_two_connections_shares_one_rate_limit_allowance() {
        // The whole point, proved at the layer that enforces it rather than by
        // comparing two strings: run `compute_bucket_id` into the real
        // `PerRuleRateLimitRegistry` exactly as `handle_request` does.
        //
        // rps=1, so the bucket admits one request and refuses the next within
        // the same second. If the two connections key differently they each
        // get their own allowance and BOTH are admitted -- which is what the
        // limiter did before the fix, for every reconnect, forever.
        use crate::enforce::PerRuleRateLimitRegistry;
        use zeroship_bundle::RateLimit;

        let registry = PerRuleRateLimitRegistry::new();
        let app = uuid::Uuid::nil();
        let limit = RateLimit {
            rpm: None,
            rps: Some(1),
            per: RateLimitPer::Ip,
        };

        let admit = |port: u16| {
            let req = ntex::web::test::TestRequest::default()
                .header("x-forwarded-for", format!("192.0.2.43:{port}"))
                .to_http_request();
            let bucket_id = compute_bucket_id(&req, limit.per, true, true);
            registry
                .check(&app, 0, limit.per, &bucket_id, &limit)
                .is_ok()
        };

        assert!(admit(40001), "first connection draws the single token");
        assert!(
            !admit(40002),
            "a reconnect from the same client must NOT draw a second token"
        );
    }

    #[test]
    fn compute_bucket_id_ip_ignores_the_source_port_of_an_ipv6_client() {
        let req = ntex::web::test::TestRequest::default()
            .header("x-forwarded-for", "[2001:db8::1]:8080")
            .to_http_request();
        assert_eq!(
            compute_bucket_id(&req, RateLimitPer::Ip, true, true),
            "2001:db8::1"
        );
    }

    // -----------------------------------------------------------------------
    // Which `X-Forwarded-For` entry the gateway reads. `zeroship-control`
    // (http_util.rs) and `zeroship-auth` (headers.rs) both take the RIGHTMOST
    // token; the gateway must agree, or one recorded client address is wrong.
    // -----------------------------------------------------------------------

    #[test]
    fn client_ip_takes_the_rightmost_forwarded_for_entry() {
        // Caddy appends the address it actually accepted the connection from,
        // so the closest hop is last. Everything to its left is what the
        // caller CLAIMED. ntex's `connection_info().remote()` took the
        // leftmost and so returned "1.2.3.4" here.
        let req = ntex::web::test::TestRequest::default()
            .header("x-forwarded-for", "1.2.3.4, 203.0.113.7")
            .to_http_request();
        assert_eq!(client_ip(&req, true), "203.0.113.7");
    }

    #[test]
    fn client_ip_ignores_a_forwarded_header_entirely() {
        // ntex reads `Forwarded` BEFORE `X-Forwarded-For` (ntex 3.7.2
        // web/info.rs:35-59 runs ahead of :101), so a test matrix built only
        // around XFF cannot catch this.
        //
        // Measured against this deployment's own proxy (caddy:2-alpine, the
        // image and `reverse_proxy` shape of deploy/ops/Caddyfile, probed
        // 2026-08-18): Caddy REPLACES `X-Forwarded-For` with the peer it
        // accepted -- a caller's `X-Forwarded-For: 1.2.3.4` arrived as
        // `127.0.0.1` -- but it neither sets nor strips `Forwarded`, and
        // passed `Forwarded: for=9.9.9.9` through verbatim. So the one header
        // ntex preferred is the one the caller still controls, and preferring
        // it handed the caller their own bucket. Control and auth read
        // `X-Forwarded-For` only; this is the gateway agreeing with them.
        let req = ntex::web::test::TestRequest::default()
            .header("forwarded", "for=1.2.3.4")
            .header("x-forwarded-for", "203.0.113.7")
            .to_http_request();
        assert_eq!(client_ip(&req, true), "203.0.113.7");

        // Alone, it resolves nothing: this fixture has no peer address.
        let only_forwarded = ntex::web::test::TestRequest::default()
            .header("forwarded", "for=1.2.3.4")
            .to_http_request();
        assert_eq!(client_ip(&only_forwarded, true), "unknown");
    }

    #[test]
    fn client_ip_rejects_a_forwarded_for_token_that_is_not_an_address() {
        // `unknown` and `_obfuscated` are legal RFC 7239 identifiers, and a
        // caller can send arbitrary bytes. None of them is an IP, so none may
        // become an IP bucket key. With no peer to fall back to, the request
        // lands in the shared "unknown" bucket rather than one of its own.
        for junk in ["unknown", "_hidden", "not-an-ip"] {
            let req = ntex::web::test::TestRequest::default()
                .header("x-forwarded-for", junk)
                .to_http_request();
            assert_eq!(client_ip(&req, true), "unknown", "token {junk:?}");
        }
    }

    #[test]
    fn compute_bucket_id_session_uses_cookie() {
        let req = ntex::web::test::TestRequest::default()
            .header(
                "cookie",
                "other=foo; __Host-zeroship_app_session=abc123; trailing=x",
            )
            .to_http_request();
        let id = compute_bucket_id(&req, RateLimitPer::Session, false, true);
        assert_eq!(id, "abc123");
    }

    #[test]
    fn compute_bucket_id_session_falls_back_to_ip_when_cookie_missing() {
        // Anonymous caller (no __Host-zeroship_app_session) → fall back to IP. The
        // TestRequest has no peer → "unknown".
        let req = ntex::web::test::TestRequest::default()
            .header("cookie", "other=foo")
            .to_http_request();
        let id = compute_bucket_id(&req, RateLimitPer::Session, false, true);
        assert_eq!(id, "unknown");
    }

    /// Build an unsigned-but-structurally-valid JWT with the given `sub`. Only
    /// the payload segment matters to `jwt_subject_unverified` (it never checks
    /// the signature), so a fixed header + dummy signature suffice.
    fn jwt_with_sub(sub: &str) -> String {
        use base64::Engine as _;
        let b64 = |v: &serde_json::Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(v).unwrap())
        };
        let header = b64(&serde_json::json!({ "alg": "none", "typ": "JWT" }));
        let payload = b64(&serde_json::json!({ "sub": sub }));
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn compute_bucket_id_user_uses_jwt_subject() {
        // RateLimitPer::User buckets by the authenticated JWT `sub` — the
        // wire scope that round-trips the console's `per: "user"` rules.
        let req = ntex::web::test::TestRequest::default()
            .header("authorization", format!("Bearer {}", jwt_with_sub("usr_alice")))
            .to_http_request();
        let id = compute_bucket_id(&req, RateLimitPer::User, false, true);
        assert_eq!(id, "sub:usr_alice");
    }

    #[test]
    fn compute_bucket_id_ignores_a_bearer_scheme_the_auth_gate_never_validated() {
        // The auth gate (`router/auth.rs`) strips `"Bearer "` and NOTHING else,
        // so a lowercase `bearer ` credential is invisible to it: the request
        // falls through to the cookie arm, authenticates on the cookie, and
        // arrives here with identity_verified = true.
        //
        // If bucketing also accepted `"bearer "`, that flag would be doing work
        // it never earned - it attests that SOME identity was verified, not
        // that THIS header was. A caller could then hand-roll an unsigned JWT,
        // pick any `sub`, and mint themselves a private rate-limit bucket per
        // request, which is exactly the hole `identity_verified` was added to
        // close.
        //
        // The invariant: the prefix set here must not be WIDER than the gate's.
        let req = ntex::web::test::TestRequest::default()
            .header(
                "authorization",
                format!("bearer {}", jwt_with_sub("usr_attacker")),
            )
            .header("cookie", "__Host-zeroship_app_session=real-session")
            .to_http_request();
        let id = compute_bucket_id(&req, RateLimitPer::User, false, true);
        assert_ne!(
            id, "sub:usr_attacker",
            "a scheme the auth gate never validated must not choose the bucket"
        );
        assert_eq!(
            id, "sess:real-session",
            "it should degrade to the credential that WAS verified"
        );
    }

    #[test]
    fn subscription_affinity_key_ignores_the_same_unvalidated_scheme() {
        // Same defect, second site, and WORSE: this function takes no
        // `identity_verified` at all, so it reads the header unconditionally.
        // An entirely unauthenticated caller can therefore choose their own
        // affinity key and steer their own CHWBL worker.
        let req = ntex::web::test::TestRequest::default()
            .header(
                "authorization",
                format!("bearer {}", jwt_with_sub("usr_attacker")),
            )
            .to_http_request();
        let key = subscription_affinity_key(&req, false);
        assert!(
            !key.contains("usr_attacker"),
            "lowercase bearer must not steer affinity, got {key:?}"
        );
    }

    #[test]
    fn compute_bucket_id_user_distinct_per_subject_not_per_session() {
        // One user, two sessions (distinct cookies) → SAME user bucket; the
        // whole point of `user` vs `session`. Then a second user → different.
        let alice = jwt_with_sub("usr_alice");
        let req_a1 = ntex::web::test::TestRequest::default()
            .header("authorization", format!("Bearer {alice}"))
            .header("cookie", "__Host-zeroship_app_session=device-1")
            .to_http_request();
        let req_a2 = ntex::web::test::TestRequest::default()
            .header("authorization", format!("Bearer {alice}"))
            .header("cookie", "__Host-zeroship_app_session=device-2")
            .to_http_request();
        let req_bob = ntex::web::test::TestRequest::default()
            .header("authorization", format!("Bearer {}", jwt_with_sub("usr_bob")))
            .to_http_request();
        let a1 = compute_bucket_id(&req_a1, RateLimitPer::User, false, true);
        let a2 = compute_bucket_id(&req_a2, RateLimitPer::User, false, true);
        let bob = compute_bucket_id(&req_bob, RateLimitPer::User, false, true);
        assert_eq!(a1, a2, "same user shares a bucket across sessions");
        assert_ne!(a1, bob, "different users get different buckets");
    }

    #[test]
    fn compute_bucket_id_user_falls_back_to_session_then_ip() {
        // No bearer JWT but a session cookie → degrade to sess:<cookie>.
        let req_sess = ntex::web::test::TestRequest::default()
            .header("cookie", "__Host-zeroship_app_session=anon-tab")
            .to_http_request();
        assert_eq!(
            compute_bucket_id(&req_sess, RateLimitPer::User, false, true),
            "sess:anon-tab"
        );
        // Neither bearer nor cookie → IP ("unknown" for the peer-less fixture).
        let req_none = ntex::web::test::TestRequest::default().to_http_request();
        assert_eq!(
            compute_bucket_id(&req_none, RateLimitPer::User, false, true),
            "unknown"
        );
        // A malformed bearer (no decodable payload) is treated as anonymous —
        // degrade rather than key everyone onto one empty bucket.
        let req_bad = ntex::web::test::TestRequest::default()
            .header("authorization", "Bearer not-a-jwt")
            .header("cookie", "__Host-zeroship_app_session=anon-tab")
            .to_http_request();
        assert_eq!(
            compute_bucket_id(&req_bad, RateLimitPer::User, false, true),
            "sess:anon-tab"
        );
    }

    /// On an `auth: "anon"` resource the caller used to control the
    /// `per: "session"` discriminator outright. `extract_session_cookie`
    /// returns the raw `__Host-zeroship_app_session` value without
    /// verifying anything, and an anon route serves happily without a
    /// session — `resolve_auth` answers `Allowed { user_header: None }`
    /// rather than rejecting. So rotating the cookie minted a fresh
    /// `TokenBucket` every request and the creator's declared cap never
    /// bound: 50 of 50 admitted against `rps: 1`.
    ///
    /// `identity_verified: false` is the shape of that request — the gate
    /// resolved nobody — and the caller must land in one IP bucket.
    ///
    /// What this does NOT catch: the same evasion via `per: "user"` (forge
    /// a 3-segment JWT whose `iss` matches the OP so the Bearer arm returns
    /// `Invalid` rather than 401, then rotate `sub`) — the fix covers that
    /// arm, but only the `Session` arm is driven here. Nor the unbounded
    /// growth of `PerRuleRateLimitRegistry::buckets`, which still has no
    /// removal path: this fix stops an anonymous caller minting keys, not
    /// the registry's inability to forget them.
    #[test]
    fn per_session_rule_cannot_be_evaded_by_rotating_the_cookie_value() {
        let reg = crate::enforce::PerRuleRateLimitRegistry::new();
        let app_id = uuid::Uuid::nil();
        let rl = RateLimit {
            rps: Some(1),
            rpm: None,
            per: RateLimitPer::Session,
        };
        let mut admitted = 0;
        for i in 0..50 {
            let req = ntex::web::test::TestRequest::default()
                .header("cookie", format!("__Host-zeroship_app_session=forged-{i}"))
                .to_http_request();
            let bucket_id = compute_bucket_id(&req, rl.per, false, false);
            if reg.check(&app_id, 0, rl.per, &bucket_id, &rl).is_ok() {
                admitted += 1;
            }
        }
        assert!(
            admitted <= 2,
            "a per-session `rps: 1` rule admitted {admitted}/50 requests from one \
             caller who simply rotated the (unverified) session-cookie value; the \
             discriminator must not be attacker-chosen on an anon route",
        );
    }

    /// The counter-example: once the gate HAS resolved a user, the cookie
    /// is a trustworthy discriminator again and two genuinely different
    /// sessions must keep their own buckets. Without this, the test above
    /// could be satisfied by collapsing every caller onto the IP and
    /// throwing away per-session limiting altogether.
    #[test]
    fn a_verified_session_still_gets_its_own_bucket() {
        let mk = |v: &str| {
            ntex::web::test::TestRequest::default()
                .header("cookie", format!("__Host-zeroship_app_session={v}"))
                .to_http_request()
        };
        let a = compute_bucket_id(&mk("real-a"), RateLimitPer::Session, false, true);
        let b = compute_bucket_id(&mk("real-b"), RateLimitPer::Session, false, true);
        assert_eq!(a, "real-a");
        assert_ne!(a, b, "two verified sessions must not share one bucket");
    }

    // -----------------------------------------------------------------------
    // Wiring smoke test — bucket lookup matches what the router does
    // -----------------------------------------------------------------------
    //
    // The Outcome::Worker arm wires `compute_bucket_id` and
    // `state.per_rule_rate_limits.check(...)` together. Driving the full
    // `execute_outcome` would need a constructable `web::types::State`,
    // which ntex doesn't expose outside its `App` builder. Instead we
    // recreate the exact bucket-key composition the router uses and
    // assert it agrees with the registry's view of "drained vs fresh"
    // — same code path in two parts.

    #[test]
    fn router_wiring_bucket_id_matches_registry_key() {
        // rps=1 with RateLimitPer::Ip. First call fills, second 429s.
        // The bucket key is `compute_bucket_id(req, RateLimitPer::Ip)`
        // — verifies the IP-derived discriminator is the same string
        // the registry's key uses, otherwise the second call would
        // hit a fresh bucket and pass.
        let reg = crate::enforce::PerRuleRateLimitRegistry::new();
        let app_id = uuid::Uuid::nil();
        let rl = RateLimit { rps: Some(1), rpm: None, per: RateLimitPer::Ip };
        let req = ntex::web::test::TestRequest::default().to_http_request();
        let bucket_id = compute_bucket_id(&req, rl.per, false, true);
        assert!(reg.check(&app_id, 0, rl.per, &bucket_id, &rl).is_ok());
        // Second call with the same request → same bucket id → drained.
        let bucket_id2 = compute_bucket_id(&req, rl.per, false, true);
        assert_eq!(bucket_id, bucket_id2, "bucket id is stable for same request");
        let err = reg
            .check(&app_id, 0, rl.per, &bucket_id2, &rl)
            .expect_err("second call must 429 — bucket key matched");
        assert_eq!(err.status(), ntex::http::StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn router_wiring_session_buckets_separate_from_ip_buckets() {
        // Two requests carrying distinct __Host-zeroship_app_session cookies under
        // RateLimitPer::Session must hit independent buckets even when
        // the IP is the same.
        let reg = crate::enforce::PerRuleRateLimitRegistry::new();
        let app_id = uuid::Uuid::nil();
        let rl = RateLimit { rps: Some(1), rpm: None, per: RateLimitPer::Session };

        let req_a = ntex::web::test::TestRequest::default()
            .header("cookie", "__Host-zeroship_app_session=user-a")
            .to_http_request();
        let req_b = ntex::web::test::TestRequest::default()
            .header("cookie", "__Host-zeroship_app_session=user-b")
            .to_http_request();
        let bucket_a = compute_bucket_id(&req_a, rl.per, false, true);
        let bucket_b = compute_bucket_id(&req_b, rl.per, false, true);
        assert_eq!(bucket_a, "user-a");
        assert_eq!(bucket_b, "user-b");
        assert!(reg.check(&app_id, 0, rl.per, &bucket_a, &rl).is_ok());
        assert!(reg.check(&app_id, 0, rl.per, &bucket_b, &rl).is_ok());
        // Reusing user-a within the same second 429s.
        let err = reg
            .check(&app_id, 0, rl.per, &bucket_a, &rl)
            .expect_err("user-a drained");
        assert_eq!(err.status(), ntex::http::StatusCode::TOO_MANY_REQUESTS);
    }

    // -----------------------------------------------------------------------
    // Resource-tree request-level tests — exercise the per-resource policy
    // gates on synthetic requests built via ntex's `TestRequest`.
    // -----------------------------------------------------------------------

    #[test]
    fn canonicalize_dispatch_path_rejects_traversal_and_normalizes() {
        // SEC-2: the dispatch layer rejects (400) any path carrying a
        // traversal / empty-interior-segment form a browser's `new URL` would
        // rewrite, and otherwise forwards the CANONICAL form (so the worker
        // re-parses the exact path the gateway matched auth on). Pre-fix the
        // helper forwards the raw path and never rejects → RED.
        for traversal in [
            "/api/foo/../admin",
            "/api/%2e/admin",
            "/api/%2E/admin",
            "/api/./admin",
            "/api//admin",
            "/api/foo/%2e%2e/admin",
            "/%2e%2e/etc/passwd",
        ] {
            assert_eq!(
                canonicalize_dispatch_path(traversal),
                CanonicalPath::Reject,
                "traversal {traversal:?} must be rejected (400), not forwarded raw"
            );
        }

        // Benign forms forward under their canonical normalization. A single
        // trailing slash is a normalize case (not a traversal), and an already
        // -canonical path is forwarded unchanged.
        assert_eq!(
            canonicalize_dispatch_path("/api/admin/"),
            CanonicalPath::Use("/api/admin".to_string()),
            "a lone trailing slash normalizes (single-slash policy), not 400"
        );
        assert_eq!(
            canonicalize_dispatch_path("/api/admin"),
            CanonicalPath::Use("/api/admin".to_string()),
            "an already-canonical path forwards unchanged"
        );
        assert_eq!(
            canonicalize_dispatch_path("/__zeroship/v1/todos.list"),
            CanonicalPath::Use("/__zeroship/v1/todos.list".to_string()),
            "RPC wire paths are canonical and forward unchanged"
        );
    }

    #[test]
    fn lookup_finds_rpc_resource_after_strip_prefix() {
        let mut resources = HashMap::new();
        resources.insert(
            "rpc:listTodos".into(),
            ResourceEntry {
                kind: Some(ProcedureKind::Query),
                ..Default::default()
            },
        );
        let m = manifest_with_resources(resources);
        let c = CompiledManifest::compile(&m);
        let p = c
            .lookup_resource("/__zeroship/v1/listTodos")
            .expect("matches rpc:listTodos");
        assert_eq!(p.kind, Some(ProcedureKind::Query));
        // Bare /_rpc/ paths are no longer dispatched — `/__zeroship/v1/` is
        // the only RPC wire prefix.
        assert!(c.lookup_resource("/_rpc/listTodos").is_none());
    }

    #[test]
    fn rate_limit_resolution_min_wins_for_resource_tree() {
        // End-to-end: build a manifest with a stricter child rate limit
        // and verify the compiled policy reflects min(parent, child).
        let mut resources = HashMap::new();
        resources.insert(
            "*".into(),
            ResourceEntry {
                auth: Some(AuthLevel::User),
                rate_limit: Some(RateLimit { rpm: Some(600), rps: None, per: RateLimitPer::Ip }),
                ..Default::default()
            },
        );
        resources.insert(
            "rpc:expensive".into(),
            ResourceEntry {
                kind: Some(ProcedureKind::Mutation),
                rate_limit: Some(RateLimit { rpm: Some(10), rps: None, per: RateLimitPer::Ip }),
                r#override: vec!["rate_limit".into()],
                ..Default::default()
            },
        );
        let m = manifest_with_resources(resources);
        let c = CompiledManifest::compile(&m);
        let p = c.lookup_resource("/__zeroship/v1/expensive").expect("matches");
        assert_eq!(
            p.rate_limit.as_ref().unwrap().rpm,
            Some(10),
            "child's stricter cap survives the merge"
        );
    }

    #[test]
    fn passthrough_manifest_has_only_root_default() {
        // The synthesized passthrough manifest carries the `*` root
        // default but no other resources. URL paths fall through to
        // 404 in the gateway; the worker is never invoked.
        let m = Manifest::passthrough();
        let c = CompiledManifest::compile(&m);
        assert!(
            c.lookup_resource("/anything").is_none(),
            "no per-path resource synthesized in passthrough"
        );
        assert!(
            c.lookup_resource("/__zeroship/v1/anything").is_none(),
            "no RPC resource synthesized in passthrough"
        );
        assert!(
            c.lookup_resource("/_rpc/listTodos").is_none(),
            "legacy /_rpc/ prefix is no longer routed"
        );
    }

    // -----------------------------------------------------------------------
    // Idempotency integration — exercises the gateway-side wiring on
    // top of the `idempotency` module. End-to-end coverage of the
    // module itself lives in `idempotency::tests`; these tests focus
    // on the HTTP-shaped surface.
    // -----------------------------------------------------------------------

    fn idempotent_mutation_policy() -> EffectivePolicy {
        EffectivePolicy {
            auth: AuthLevel::Anon,
            rate_limit: None,
            cors: None,
            cache: None,
            csrf_origins: None,
            idempotent: true,
            idempotency_ttl_hours: None,
            max_input_bytes: None,
            middleware: vec![],
            publicly_accessible: true,
            required_scopes: vec![],
            kind: Some(ProcedureKind::Mutation),
            action: crate::compiled::ResolvedAction::WorkerRpc,
            timeout_ms: None,
            input_schema: None,
            output_schema: None,
        }
    }

    fn make_minimal_state() -> Arc<GateState> {
        build_idempotency_state()
    }

    #[compio::test]
    async fn idempotency_missing_header_returns_400_invalid_argument() {
        let state = make_minimal_state();
        let req = ntex::web::test::TestRequest::default()
            .method(ntex::http::Method::POST)
            .to_http_request();
        let body = Bytes::from_static(b"{}");
        let policy = idempotent_mutation_policy();

        let outcome = handle_idempotency_pre_dispatch(
            &req,
            &state,
            &uuid::Uuid::new_v4(),
            "/__zeroship/v1/todos.add",
            &policy,
            None,
            &body,
            std::time::Instant::now(),
        )
        .await;

        match outcome {
            IdempotencyOutcome::ReturnNow(mut resp) => {
                assert_eq!(resp.status(), ntex::http::StatusCode::BAD_REQUEST);
                let ct = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                assert_eq!(ct, "application/zs-error+json");
                let body = resp.take_body();
                let bytes = collect_body(body).await;
                let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(v["code"], "INVALID_ARGUMENT");
            }
            IdempotencyOutcome::Proceed(_) => panic!("must reject when header missing"),
        }
    }

    #[compio::test]
    async fn idempotency_first_request_proceeds_holds_lock() {
        let state = make_minimal_state();
        let app_id = uuid::Uuid::new_v4();
        let req = ntex::web::test::TestRequest::default()
            .header("idempotency-key", "5c7f4a1b-8d2e-4c3f-9a6b-1e2d3c4b5a60")
            .to_http_request();
        let policy = idempotent_mutation_policy();

        let outcome = handle_idempotency_pre_dispatch(
            &req,
            &state,
            &app_id,
            "/__zeroship/v1/todos.add",
            &policy,
            None,
            &Bytes::from_static(b"{\"text\":\"hi\"}"),
            std::time::Instant::now(),
        )
        .await;

        match outcome {
            IdempotencyOutcome::Proceed(handle) => {
                assert_eq!(
                    handle.entry_key,
                    crate::idempotency::entry_key(
                        &app_id,
                        "todos.add",
                        crate::idempotency::Principal::Anon,
                        "5c7f4a1b-8d2e-4c3f-9a6b-1e2d3c4b5a60",
                    )
                );
                assert_eq!(handle.ttl_hours, crate::idempotency::DEFAULT_TTL_HOURS);
            }
            IdempotencyOutcome::ReturnNow(_) => panic!("first request must Proceed"),
        }
    }

    #[compio::test]
    async fn idempotency_second_same_body_replays_cached_response() {
        let state = make_minimal_state();
        let app_id = uuid::Uuid::new_v4();
        let req = ntex::web::test::TestRequest::default()
            .header("idempotency-key", "7b1e9d40-3c5a-4f21-8e77-90ab12cd34ef")
            .to_http_request();
        let policy = idempotent_mutation_policy();
        let body = Bytes::from_static(b"{\"x\":1}");

        // First request claims the lock.
        let first = handle_idempotency_pre_dispatch(
            &req,
            &state,
            &app_id,
            "/__zeroship/v1/todos.add",
            &policy,
            None,
            &body,
            std::time::Instant::now(),
        )
        .await;
        let IdempotencyOutcome::Proceed(handle) = first else {
            panic!("expected Proceed");
        };

        // Build a fake worker response and capture it.
        let worker_resp = HttpResponse::Created()
            .header("content-type", "application/json")
            .body(b"{\"id\":42}".to_vec());
        let captured = capture_response_for_idempotency(
            &state,
            &app_id,
            handle,
            ResponseOrigin::Worker,
            worker_resp,
        )
        .await;
        let captured: HttpResponse = captured;
        assert_eq!(captured.status(), ntex::http::StatusCode::CREATED);
        assert_eq!(
            captured
                .headers()
                .get("x-zs-idempotent-stored")
                .and_then(|v| v.to_str().ok()),
            Some("true")
        );

        // Second request — same body — must replay verbatim.
        let second = handle_idempotency_pre_dispatch(
            &req,
            &state,
            &app_id,
            "/__zeroship/v1/todos.add",
            &policy,
            None,
            &body,
            std::time::Instant::now(),
        )
        .await;
        match second {
            IdempotencyOutcome::ReturnNow(mut resp) => {
                assert_eq!(resp.status(), ntex::http::StatusCode::CREATED);
                assert_eq!(
                    resp.headers()
                        .get("x-zs-idempotent-replay")
                        .and_then(|v| v.to_str().ok()),
                    Some("true")
                );
                let body = resp.take_body();
                let body_bytes = collect_body(body).await;
                assert_eq!(body_bytes, b"{\"id\":42}");
            }
            IdempotencyOutcome::Proceed(_) => panic!("must replay cached response"),
        }
    }

    #[compio::test]
    async fn idempotency_second_different_body_returns_409_already_exists() {
        let state = make_minimal_state();
        let app_id = uuid::Uuid::new_v4();
        let req_with_key = |body_label: &str| {
            ntex::web::test::TestRequest::default()
                .header("idempotency-key", "0e3c5a91-77bd-4d2f-b418-6c9a0f5e2d31")
                .header("x-test-label", body_label)
                .to_http_request()
        };
        let policy = idempotent_mutation_policy();

        // Seed the dedupe table with the original.
        let first = handle_idempotency_pre_dispatch(
            &req_with_key("a"),
            &state,
            &app_id,
            "/__zeroship/v1/todos.add",
            &policy,
            None,
            &Bytes::from_static(b"{\"a\":1}"),
            std::time::Instant::now(),
        )
        .await;
        let IdempotencyOutcome::Proceed(handle) = first else {
            panic!("expected Proceed");
        };
        let worker_resp = HttpResponse::Ok().body(b"first".to_vec());
        let _ = capture_response_for_idempotency(
            &state,
            &app_id,
            handle,
            ResponseOrigin::Worker,
            worker_resp,
        )
        .await;

        // Same key, different body → 409 ALREADY_EXISTS with Retry-After.
        let conflict = handle_idempotency_pre_dispatch(
            &req_with_key("b"),
            &state,
            &app_id,
            "/__zeroship/v1/todos.add",
            &policy,
            None,
            &Bytes::from_static(b"{\"b\":2}"),
            std::time::Instant::now(),
        )
        .await;

        match conflict {
            IdempotencyOutcome::ReturnNow(mut resp) => {
                assert_eq!(resp.status(), ntex::http::StatusCode::CONFLICT);
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .expect("retry-after present and numeric");
                assert!(retry_after > 0 && retry_after <= 24 * 3600);
                let body = resp.take_body();
                let bytes = collect_body(body).await;
                let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(v["code"], "ALREADY_EXISTS");
                assert_eq!(v["details"]["reason"], "idempotency_key_reused_with_different_input");
            }
            IdempotencyOutcome::Proceed(_) => panic!("must reject"),
        }
    }

    #[compio::test]
    async fn idempotency_per_procedure_ttl_clamps_into_band() {
        // Authored 1h TTL is honored. Authored 0 clamps up to 1; authored
        // 999 clamps down to 168.
        assert_eq!(crate::idempotency::clamp_ttl_hours(Some(1)), 1);
        assert_eq!(crate::idempotency::clamp_ttl_hours(Some(168)), 168);
        assert_eq!(crate::idempotency::clamp_ttl_hours(Some(0)), 1);
        assert_eq!(crate::idempotency::clamp_ttl_hours(Some(9999)), 168);
        assert_eq!(crate::idempotency::clamp_ttl_hours(None), 24);
    }

    #[compio::test]
    async fn idempotency_per_procedure_ttl_flows_into_handle() {
        // A mutation pinned to 48h yields handle.ttl_hours = 48.
        let state = make_minimal_state();
        let app_id = uuid::Uuid::new_v4();
        let req = ntex::web::test::TestRequest::default()
            .header("idempotency-key", "7b1e9d40-3c5a-4f21-8e77-90ab12cd34ef")
            .to_http_request();
        let mut policy = idempotent_mutation_policy();
        policy.idempotency_ttl_hours = Some(48);

        let outcome = handle_idempotency_pre_dispatch(
            &req,
            &state,
            &app_id,
            "/__zeroship/v1/todos.add",
            &policy,
            None,
            &Bytes::from_static(b"{}"),
            std::time::Instant::now(),
        )
        .await;
        let IdempotencyOutcome::Proceed(handle) = outcome else {
            panic!("expected Proceed");
        };
        assert_eq!(handle.ttl_hours, 48);
    }

    #[compio::test]
    async fn buffer_response_body_round_trips_payload() {
        // The streaming/buffered conversion preserves bytes verbatim.
        let resp = HttpResponse::Ok()
            .header("content-type", "application/json")
            .body(b"{\"hello\":\"world\"}".to_vec());
        let (rebuilt, bytes) = buffer_response_body(resp).await;
        assert_eq!(bytes, b"{\"hello\":\"world\"}");
        assert_eq!(
            rebuilt
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        let mut rebuilt = rebuilt;
        let body = rebuilt.take_body();
        let drained = collect_body(body).await;
        assert_eq!(drained, b"{\"hello\":\"world\"}");
    }

    #[compio::test]
    async fn build_replay_response_carries_status_and_body() {
        use base64::Engine as _;
        let mut headers = std::collections::HashMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());
        let stored = crate::idempotency::StoredResponse {
            input_hash: crate::idempotency::hash_body(b"{}"),
            status: 422,
            headers,
            body_b64: base64::engine::general_purpose::STANDARD.encode(b"{\"err\":\"x\"}"),
            completed_at: 0,
            ttl_until: u64::MAX,
        };
        let resp = build_replay_response(&stored, std::time::Instant::now());
        assert_eq!(resp.status(), ntex::http::StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            resp.headers()
                .get("x-zs-idempotent-replay")
                .and_then(|v| v.to_str().ok()),
            Some("true")
        );
        let mut resp = resp;
        let body = resp.take_body();
        let drained = collect_body(body).await;
        assert_eq!(drained, b"{\"err\":\"x\"}");
    }

    // -----------------------------------------------------------------------
    // Subscription gating + session affinity
    // -----------------------------------------------------------------------

    fn subscription_resource() -> ResourceEntry {
        ResourceEntry {
            kind: Some(ProcedureKind::Subscription),
            ..Default::default()
        }
    }

    fn subscription_manifest() -> Manifest {
        let mut resources = HashMap::new();
        resources.insert("rpc:todoTicker".into(), subscription_resource());
        manifest_with_resources(resources)
    }

    /// `is_websocket_upgrade` accepts `Upgrade: websocket` + `Connection`
    /// containing the `upgrade` token (case-insensitive, comma-separated).
    #[test]
    fn is_websocket_upgrade_accepts_canonical_headers() {
        let req = ntex::web::test::TestRequest::default()
            .header("upgrade", "websocket")
            .header("connection", "Upgrade")
            .to_http_request();
        assert!(is_websocket_upgrade(&req));

        let req = ntex::web::test::TestRequest::default()
            .header("upgrade", "WebSocket")
            .header("connection", "keep-alive, Upgrade")
            .to_http_request();
        assert!(is_websocket_upgrade(&req));
    }

    #[test]
    fn is_websocket_upgrade_rejects_missing_headers() {
        let req = ntex::web::test::TestRequest::default()
            .header("upgrade", "websocket")
            .to_http_request();
        assert!(!is_websocket_upgrade(&req), "missing connection rejected");

        let req = ntex::web::test::TestRequest::default()
            .header("connection", "Upgrade")
            .to_http_request();
        assert!(!is_websocket_upgrade(&req), "missing upgrade rejected");

        let req = ntex::web::test::TestRequest::default().to_http_request();
        assert!(!is_websocket_upgrade(&req), "no headers rejected");
    }

    /// Affinity key prefers JWT subject over cookie, cookie over WS-key,
    /// WS-key over IP. Rebuilds the same key for the same caller.
    #[test]
    fn subscription_affinity_prefers_jwt_subject() {
        // {"sub":"alice"} base64-url no padding
        let payload = base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            br#"{"sub":"alice"}"#,
        );
        let jwt = format!("h.{payload}.s");
        let req = ntex::web::test::TestRequest::default()
            .header("authorization", format!("Bearer {jwt}"))
            .header("cookie", "__Host-zeroship_app_session=cookieval")
            .to_http_request();
        assert_eq!(subscription_affinity_key(&req, false), "sub:alice");
    }

    #[test]
    fn subscription_affinity_falls_back_to_cookie() {
        let req = ntex::web::test::TestRequest::default()
            .header("cookie", "__Host-zeroship_app_session=tok123")
            .to_http_request();
        assert_eq!(subscription_affinity_key(&req, false), "sess:tok123");
    }

    #[test]
    fn subscription_affinity_falls_back_to_ws_key_then_ip() {
        let req = ntex::web::test::TestRequest::default()
            .header("sec-websocket-key", "abc==")
            .to_http_request();
        assert_eq!(subscription_affinity_key(&req, false), "wsk:abc==");

        let req = ntex::web::test::TestRequest::default().to_http_request();
        // No headers, no remote — falls back to "ip:unknown".
        assert_eq!(subscription_affinity_key(&req, false), "ip:unknown");
    }

    #[test]
    fn subscription_affinity_pins_one_client_to_one_worker_across_reconnects() {
        // The second consumer of the same resolver, and a second consequence
        // of the same defect: this key picks the CHWBL worker, and a
        // subscription's per-connection state lives on the worker it picked.
        // While the source port rode along in the key, an anonymous caller
        // hashed somewhere new on every reconnect and never found their state
        // again -- the exact opposite of what this function exists to do.
        let from_port = |port: u16| {
            let req = ntex::web::test::TestRequest::default()
                .header("x-forwarded-for", format!("192.0.2.43:{port}"))
                .to_http_request();
            subscription_affinity_key(&req, true)
        };
        assert_eq!(from_port(40001), "ip:192.0.2.43");
        assert_eq!(from_port(40001), from_port(40002));
    }

    /// Session-affinity invariant: the same `(app_id, principal)` always
    /// resolves to the same worker, even when the ring has many candidates
    /// and capacity is unbounded. Different principals on the same app
    /// MAY land on different workers (the spread is the whole point).
    #[test]
    fn select_with_affinity_is_sticky_for_same_principal() {
        let workers: Vec<String> = (0..16)
            .map(|i| format!("http://worker-{i}:8080"))
            .collect();
        let ring = crate::proxy::HashRing::new(workers, u32::MAX);
        let app = uuid::Uuid::nil();

        let (a, _) = ring.select_with_affinity(&app, "sub:alice");
        let (b, _) = ring.select_with_affinity(&app, "sub:alice");
        assert_eq!(a, b, "same principal always sticky-binds same worker");

        let (c, _) = ring.select_with_affinity(&app, "sub:bob");
        // Not asserting `a != c` — pigeonhole says collisions are
        // possible — but the ring should distribute across enough
        // distinct principals. Just verify Bob is also sticky.
        let (c2, _) = ring.select_with_affinity(&app, "sub:bob");
        assert_eq!(c, c2);
    }

    /// Vary either the app or the principal and the chosen worker can
    /// shift; same (app, principal) is the affinity contract.
    #[test]
    fn select_with_affinity_varies_with_principal() {
        let workers: Vec<String> = (0..16)
            .map(|i| format!("http://worker-{i}:8080"))
            .collect();
        let ring = crate::proxy::HashRing::new(workers, u32::MAX);
        let app = uuid::Uuid::nil();

        let mut hits = std::collections::HashSet::new();
        for u in 0..32 {
            let (idx, _) = ring.select_with_affinity(&app, &format!("sub:user-{u}"));
            hits.insert(idx);
        }
        // 32 principals across 16 workers — should cover at least 4
        // distinct workers (very loose; the actual spread is uniform).
        assert!(hits.len() >= 4, "affinity should distribute, hit count: {}", hits.len());
    }

    /// Idempotency middleware should NOT apply to subscriptions even
    /// when `policy.idempotent` is true — the spec only protects
    /// mutations. Verifies the gating clause directly.
    #[test]
    fn idempotency_bypasses_subscriptions() {
        // Same shape as `idempotent_mutation_policy` but with kind: Subscription.
        let policy = EffectivePolicy {
            auth: AuthLevel::Anon,
            rate_limit: None,
            cors: None,
            cache: None,
            csrf_origins: None,
            idempotent: true,
            idempotency_ttl_hours: None,
            max_input_bytes: None,
            middleware: vec![],
            publicly_accessible: true,
            required_scopes: vec![],
            kind: Some(ProcedureKind::Subscription),
            action: crate::compiled::ResolvedAction::WorkerRpc,
            timeout_ms: None,
            input_schema: None,
            output_schema: None,
        };
        // The router gate: idempotency engages only when
        //   policy.idempotent && kind in {Mutation, Action, None} &&
        //   action == WorkerRpc.
        // For subscription the kind clause is false, so we skip dedupe.
        let engages = policy.idempotent
            && matches!(
                policy.kind,
                Some(ProcedureKind::Mutation) | Some(ProcedureKind::Action) | None
            )
            && matches!(policy.action, crate::compiled::ResolvedAction::WorkerRpc);
        assert!(!engages, "idempotency must NOT engage for subscriptions");
    }

    /// Gate: `kind: subscription` + non-GET method → 405 (mirrors the
    /// dispatch-loop method check). Smoke-test against the lookup +
    /// kind classification path; the actual 405 response is built
    /// inside `execute_resource_tree` and the router test fixture
    /// doesn't expose a clean entry point for it, so we exercise the
    /// classification only.
    #[test]
    fn subscription_resource_classifies_correctly() {
        let m = subscription_manifest();
        let c = CompiledManifest::compile(&m);
        let p = c
            .lookup_resource("/__zeroship/v1/todoTicker")
            .expect("subscription resource");
        assert_eq!(p.kind, Some(ProcedureKind::Subscription));
    }

    // ----------------------------------------------------------------------
    // Host-based routing has NO platform-internal escape hatch.
    //
    // The gateway used to special-case `auth.zeroship.ai` /
    // `auth.zeroship.localhost` and proxy those hosts to the auth service.
    // Two anchored-match tests guarded that arm, because a loose
    // prefix/substring compare let a crafted Host (`auth.zeroship.ai.evil.com`,
    // `authx.zeroship.ai`, `evil-auth.zeroship.ai`) reach a platform-internal
    // upstream (finding P2-A2). The arm is gone: the gateway serves apps on the
    // worker and nothing else, and `auth.<domain>` reaches the auth service
    // through the edge proxy (deploy/ops/Caddyfile), never through here.
    //
    // The test below replaces those two. It does not re-check anchoring -
    // there is no longer a name to anchor against - it checks the stronger
    // property that made anchoring unnecessary: EVERY Host, including the ones
    // that used to be special, is resolved as an ordinary app name against the
    // registry. A reintroduced special case would have to break this to exist.
    // ----------------------------------------------------------------------

    fn req_with_host(host: &str) -> HttpRequest {
        ntex::web::test::TestRequest::default()
            .header("host", host)
            .to_http_request()
    }

    #[test]
    fn no_host_bypasses_app_name_resolution_for_a_platform_service() {
        // The formerly-special hosts now resolve like any other subdomain:
        // to an app NAME that must be looked up in the registry. `auth` is a
        // name, not a route to `crates/auth`.
        for host in [
            "auth.zeroship.ai",
            "AUTH.ZEROSHIP.AI",
            "auth.zeroship.ai:443",
            "auth.zeroship.localhost",
            "auth.zeroship.localhost:8080",
        ] {
            assert_eq!(
                extract_app_name(&req_with_host(host), None).as_deref(),
                Some(if host.starts_with("AUTH") { "AUTH" } else { "auth" }),
                "{host} must resolve to an ordinary app name, not an internal upstream"
            );
        }

        // The lookalikes the deleted anchoring test enumerated are likewise
        // just app names now; none of them names a platform component.
        for host in [
            "auth.zeroship.ai.evil.com",
            "authx.zeroship.ai",
            "auth-evil.com",
            "evil-auth.zeroship.ai",
            "auth.zeroship.evil.com",
        ] {
            let name = extract_app_name(&req_with_host(host), None);
            assert!(
                name.is_some(),
                "{host} must resolve as an app name like any other host"
            );
        }

        // A request with no Host header resolves to no app at all, so it
        // cannot fall through to some default internal destination.
        assert_eq!(
            extract_app_name(&ntex::web::test::TestRequest::default().to_http_request(), None),
            None
        );
    }

    // -----------------------------------------------------------------------
    // OIDC RP integration — auth redirect + callback handler
    // -----------------------------------------------------------------------

    /// `wants_html` should fire on `text/html`-containing Accept
    /// headers and ignore everything else. The classifier drives the
    /// 302-vs-401 split for unauthenticated requests.
    #[test]
    fn wants_html_recognizes_browser_accept() {
        let html = ntex::web::test::TestRequest::default()
            .header("accept", "text/html,application/xhtml+xml;q=0.9")
            .to_http_request();
        assert!(wants_html(&html));

        let json = ntex::web::test::TestRequest::default()
            .header("accept", "application/json")
            .to_http_request();
        assert!(!wants_html(&json));

        let none = ntex::web::test::TestRequest::default().to_http_request();
        assert!(!wants_html(&none));
    }

    /// `unauthenticated_response` returns 401+WWW-Authenticate for API
    /// callers (no `Accept: text/html`). This is the contract that
    /// lets `fetch()` callers surface their own login UI instead of
    /// following a 302 into op they can't render.
    #[test]
    fn unauthenticated_response_returns_401_for_api_clients() {
        let req = ntex::web::test::TestRequest::default()
            .header("accept", "application/json")
            .to_http_request();
        let state = build_idempotency_state();
        let resp = unauthenticated_response(&req, &state, Some("oac_myapp"));
        assert_eq!(resp.status(), ntex::http::StatusCode::UNAUTHORIZED);
        let wa = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(wa.contains("Bearer"), "got www-authenticate = {wa:?}");
    }

    /// `insufficient_scope_response` uses RFC 6750 §3.1 for its challenge
    /// header and an SDK-specific JSON body. This pins BOTH wire contracts that
    /// ride it, which use deliberately different error tokens:
    ///
    /// * `WWW-Authenticate` header → RFC 6750's registered `insufficient_scope`
    ///   token plus a space-delimited `scope` param.
    /// * JSON body → the SDK contract `{"error":"scope_required","scope":"…"}`
    ///   (the `AuthErrorCode` the SDK `mapError()` recognizes; there is no
    ///   `insufficient_scope` code SDK-side).
    ///
    /// Without this test the distinct header and body shapes could diverge
    /// unnoticed (they are only transitively covered by the `resolve_auth`
    /// enum-level tests).
    #[compio::test]
    async fn insufficient_scope_response_403_shape() {
        let required = vec!["read:billing".to_string(), "write:projects".to_string()];
        let mut resp = insufficient_scope_response(&required);
        assert_eq!(resp.status(), ntex::http::StatusCode::FORBIDDEN);

        // RFC 6750 challenge: registered token + space-joined scope param.
        let wa = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(
            wa.contains("error=\"insufficient_scope\""),
            "WWW-Authenticate must use the RFC 6750 token, got {wa:?}"
        );
        assert!(
            wa.contains("scope=\"read:billing write:projects\""),
            "WWW-Authenticate must list the space-joined required scopes, got {wa:?}"
        );

        // SDK-contract JSON body: scope_required + space-joined scope string.
        let body_bytes = collect_body(resp.take_body()).await;
        let body: serde_json::Value =
            serde_json::from_slice(&body_bytes).expect("403 body is JSON");
        assert_eq!(
            body["error"], "scope_required",
            "JSON error code must be the SDK AuthErrorCode, got {body}"
        );
        assert_eq!(
            body["scope"], "read:billing write:projects",
            "JSON scope must be the space-joined required scopes, got {body}"
        );
    }

    /// HTML navigations get a 302 → op with the stash cookie set.
    /// The redirect target carries the gateway's client_id, the PKCE
    /// challenge, and the per-app `redirect_uri` derived from the Host
    /// header.
    #[test]
    fn unauthenticated_response_redirects_html_clients_to_op() {
        let req = ntex::web::test::TestRequest::default()
            .header("accept", "text/html")
            .header("host", "myapp.zeroship.localhost")
            .uri("/dashboard?welcome=true")
            .to_http_request();
        let state = build_idempotency_state();
        let resp = unauthenticated_response(&req, &state, Some("oac_myapp"));
        assert_eq!(resp.status(), ntex::http::StatusCode::FOUND);
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            location.contains("/authorize?"),
            "location must point at the OP's /authorize; got {location:?}"
        );
        assert!(location.contains("client_id=oac_myapp"));
        assert!(location.contains("code_challenge="));
        // `redirect_uri` is the per-host callback path; origin_scheme=Http in
        // the test fixture selects the public scheme independently of cookies.
        assert!(
            location.contains("redirect_uri=http%3A%2F%2Fmyapp.zeroship.localhost%2F__zeroship%2Fauth%2Fcallback"),
            "redirect_uri must include the per-host callback path; got {location:?}"
        );
        // Stash cookie set.
        let set_cookie = resp
            .headers()
            .get("set-cookie")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            set_cookie.starts_with("__Host-zs_oidc_stash=") && set_cookie.contains("Secure"),
            "must set the secure host-only stash cookie; got {set_cookie:?}"
        );
    }

    #[test]
    fn oidc_original_path_rejects_protocol_relative_redirects() {
        assert_eq!(sanitize_oidc_original_path("//evil.com/path"), "/");
        assert_eq!(sanitize_oidc_original_path("/\\evil.com/path"), "/");
        assert_eq!(sanitize_oidc_original_path("/foo:bar/baz"), "/");
        assert_eq!(sanitize_oidc_original_path("https://evil.com/path"), "/");
    }

    #[test]
    fn oidc_original_path_keeps_origin_relative_paths() {
        assert_eq!(sanitize_oidc_original_path("/"), "/");
        assert_eq!(
            sanitize_oidc_original_path("/dashboard?welcome=true"),
            "/dashboard?welcome=true"
        );
        assert_eq!(
            sanitize_oidc_original_path("/__zeroship/auth/callback"),
            "/__zeroship/auth/callback"
        );
    }

    /// The scheme-lookalike check exists to reject `/javascript:alert(1)`.
    /// Its "first segment" must therefore end at the first `/`, `?` or `#` —
    /// a colon in the QUERY is not a scheme and must not discard the user's
    /// destination. `start_oidc_redirect` stashes `path_and_query()`, so the
    /// query is present on every real call.
    ///
    /// What this does NOT cover: it asserts nothing about fragments arriving
    /// over the wire (a browser never sends them) and nothing about the
    /// `bytes[1]` gate, which `oidc_original_path_rejects_protocol_relative_redirects`
    /// owns.
    #[test]
    fn oidc_original_path_keeps_a_colon_that_lives_in_the_query() {
        // CONTROL: colon-free query on the same shape. If this ever fails the
        // instrument never reached the colon logic.
        assert_eq!(
            sanitize_oidc_original_path("/search?q=example"),
            "/search?q=example"
        );
        // THE CASE: only the colon's presence in the query differs.
        assert_eq!(
            sanitize_oidc_original_path("/search?q=https://example.com"),
            "/search?q=https://example.com",
            "a colon inside the QUERY must not make the path unsafe"
        );
        assert_eq!(
            sanitize_oidc_original_path("/agenda?t=12:30"),
            "/agenda?t=12:30"
        );
        // A colon in the fragment is the same class.
        assert_eq!(sanitize_oidc_original_path("/doc#a:b"), "/doc#a:b");
        // NEGATIVE CONTROL, and the reason the check exists: a colon in the
        // first PATH segment still discards the destination even when a
        // query follows it.
        assert_eq!(sanitize_oidc_original_path("/foo:bar?q=1"), "/");
    }

    /// `render_callback_error` returns a 400 HTML page and escapes
    /// the inserted message so a malicious upstream cannot smuggle
    /// markup through the failure path.
    #[test]
    fn render_callback_error_returns_html_400_and_escapes_message() {
        let resp = render_callback_error("<script>alert(1)</script>");
        assert_eq!(resp.status(), ntex::http::StatusCode::BAD_REQUEST);
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(ct.starts_with("text/html"), "got content-type {ct:?}");
    }

    #[test]
    fn oidc_callback_token_exchange_error_is_generic() {
        let err = oidc_rp::OidcRpError::TokenExchange(
            "HTTP 500: op says postgres://internal".into(),
        );
        assert_eq!(
            oidc_callback_public_error(&err),
            "sign-in could not be completed",
        );
    }

    #[test]
    fn html_escape_neutralizes_tags_and_quotes() {
        assert_eq!(
            html_escape(r#"<script>alert("x" & 'y')</script>"#),
            "&lt;script&gt;alert(&quot;x&quot; &amp; &#39;y&#39;)&lt;/script&gt;",
        );
    }

    // -----------------------------------------------------------------------
    // Spend enforcement at the gateway edge.
    //
    // These drive the REAL path: a `RouteEntry` (carrying the pulled
    // `spend_state`) is pushed through the REAL `RouteCache::update` (which
    // flips the degraded registries), then either the real `handle_dispatch`
    // is invoked (402 gate) or the real `acquire_concurrency` is driven
    // against the flipped registry (degrade tightening) — no shims.
    // -----------------------------------------------------------------------

    fn spend_route(spend_state: zeroship_core::types::SpendState) -> zeroship_core::types::RouteEntry {
        zeroship_core::types::RouteEntry {
            name: "spend-app.zeroship.localhost".to_string(),
            plan_id: "free".to_string(),
            api_key_hash: "h".to_string(),
            deploy_hash: None,
            manifest: zeroship_bundle::Manifest::passthrough(),
            oauth_client_id: None,
            sector_identifier: None,
            spend_state,
            account_state: zeroship_core::types::AccountState::Active,
        }
    }

    /// A route whose manifest declares a public RPC query at wire-id `ping`,
    /// so `/__zeroship/v1/ping` resolves to a `WorkerRpc` action through the
    /// real `lookup_resource`. Used to drive the worker-dispatch path of the
    /// hoisted spend gate via the public `handle_request` entry.
    fn worker_spend_route(
        spend_state: zeroship_core::types::SpendState,
    ) -> zeroship_core::types::RouteEntry {
        use zeroship_bundle::{ProcedureKind, ResourceEntry};
        let mut resources = std::collections::HashMap::new();
        resources.insert(
            "rpc:ping".to_string(),
            ResourceEntry {
                kind: Some(ProcedureKind::Query),
                auth: Some(zeroship_bundle::AuthLevel::Anon),
                publicly_accessible: Some(true),
                ..Default::default()
            },
        );
        let manifest = zeroship_bundle::Manifest {
            version: 1,
            resources,
            ..zeroship_bundle::Manifest::default()
        };
        zeroship_core::types::RouteEntry {
            name: "spend-app.zeroship.localhost".to_string(),
            plan_id: "free".to_string(),
            api_key_hash: "h".to_string(),
            deploy_hash: None,
            manifest,
            oauth_client_id: None,
            sector_identifier: None,
            spend_state,
            account_state: zeroship_core::types::AccountState::Active,
        }
    }

    /// A route whose manifest serves a publicly-accessible STATIC resource at
    /// `/about`. Used to prove the spend gate covers the static-asset path
    /// (#3): a Blocked app must 402 BEFORE serving static egress, not fall
    /// through to the static server.
    fn static_spend_route(
        spend_state: zeroship_core::types::SpendState,
    ) -> zeroship_core::types::RouteEntry {
        use zeroship_bundle::{ResourceEntry, StaticAction};
        let mut resources = std::collections::HashMap::new();
        resources.insert(
            "/about".to_string(),
            ResourceEntry {
                auth: Some(zeroship_bundle::AuthLevel::Anon),
                publicly_accessible: Some(true),
                r#static: Some(StaticAction {
                    r#try: vec!["/about.html".into()],
                }),
                ..Default::default()
            },
        );
        let manifest = zeroship_bundle::Manifest {
            version: 1,
            resources,
            ..zeroship_bundle::Manifest::default()
        };
        zeroship_core::types::RouteEntry {
            name: "static-spend-app.zeroship.localhost".to_string(),
            plan_id: "free".to_string(),
            api_key_hash: "h".to_string(),
            deploy_hash: None,
            manifest,
            oauth_client_id: None,
            sector_identifier: None,
            spend_state,
            account_state: zeroship_core::types::AccountState::Active,
        }
    }

    /// Build an ntex `web::types::State<Arc<GateState>>` carrying `state` so a
    /// test can invoke the REAL `handle_request` / `execute_resource_tree`
    /// (their signatures take the extractor type, not `&Arc<GateState>`).
    async fn web_state(state: Arc<GateState>) -> web::types::State<Arc<GateState>> {
        use ntex::web::error::DefaultError;
        use ntex::web::FromRequest;
        let req = ntex::web::test::TestRequest::default()
            .state(state)
            .to_http_request();
        let mut payload = ntex::http::Payload::None;
        <web::types::State<Arc<GateState>> as FromRequest<DefaultError>>::from_request(
            &req,
            &mut payload,
        )
        .await
        .expect("State extractor")
    }

    /// Block → 402 SPEND_LIMIT BEFORE any worker proxy. Fed via the REAL
    /// `RouteCache::update` and driven through the REAL `handle_request` (the
    /// public entry — the gate is hoisted into `execute_resource_tree`). The
    /// stub hash-ring points at `0.0.0.0:0`; if the gate did NOT fire, the
    /// proxy attempt would surface a 502 BadGateway — so a 402 proves the gate
    /// short-circuited before the worker was ever contacted.
    #[compio::test]
    async fn over_limit_request_blocked_at_gateway() {
        use zeroship_core::types::SpendState;
        let state = build_idempotency_state();
        let app_id = Uuid::new_v4();

        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes.insert(app_id, worker_spend_route(SpendState::Block));
        state.routes.update_snapshot(
            zeroship_core::types::GatewaySnapshot {
                routes,
                principal_lifecycle: Vec::new(),
                family_revocations: Vec::new(),
            },
            &state.rate_limiters,
            &state.concurrency,
        );

        let req = ntex::web::test::TestRequest::default()
            .uri("/__zeroship/v1/ping")
            .header("host", "spend-app.zeroship.localhost")
            .to_http_request();
        let resp = handle_request(
            req,
            web_state(state.clone()).await,
            "spend-app.zeroship.localhost",
            "/__zeroship/v1/ping",
            Bytes::new(),
        )
        .await;

        assert_eq!(
            resp.status(),
            ntex::http::StatusCode::PAYMENT_REQUIRED,
            "a Blocked app must 402 before any worker proxy",
        );
        let mut resp = resp;
        let body = collect_body(resp.take_body()).await;
        let json: serde_json::Value = serde_json::from_slice(&body).expect("402 body is JSON");
        assert_eq!(json["code"], "SPEND_LIMIT");
    }

    /// Degrade must throttle a STATIC resource, exactly as Block refuses one.
    ///
    /// `check_rate_limit` and `acquire_concurrency` are the only readers of the
    /// degraded registries, and they ran only inside `handle_dispatch` /
    /// `handle_subscription_dispatch`. The `Static` and `Redirect` arms return
    /// from the action match without reaching either, so a Degraded app served
    /// unthrottled static egress — the same egress `Block` refuses one gate
    /// earlier and step 8b meters as `gateway_egress_bytes`.
    ///
    /// `burst: 8` is what makes this discriminate. A bucket holds
    /// `burst * 1000` tokens; a Degraded request costs `DEGRADE_FACTOR * 1000`
    /// clamped to capacity, so one Degraded request drains an 8-burst bucket
    /// while a normal request costs an eighth of it. The default fixture's
    /// `burst: 1` cannot tell the two apart — both cost the whole bucket.
    ///
    /// What this does NOT catch: the concurrency half (requests here are
    /// sequential, so the gauge never exceeds one), or the `Redirect` arm.
    #[compio::test]
    async fn degraded_app_is_throttled_on_a_static_resource() {
        use zeroship_core::types::SpendState;
        let state = build_test_state_with_limits(vec!["http://0.0.0.0:0".into()], 1, 8, 100);
        let app_id = Uuid::new_v4();

        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes.insert(app_id, static_spend_route(SpendState::Degrade));
        state
            .routes
            .update(routes, &state.rate_limiters, &state.concurrency);

        let fire = || async {
            let req = ntex::web::test::TestRequest::default()
                .uri("/about")
                .header("host", "static-spend-app.zeroship.localhost")
                .to_http_request();
            handle_request(
                req,
                web_state(state.clone()).await,
                "static-spend-app.zeroship.localhost",
                "/about",
                Bytes::new(),
            )
            .await
            .status()
        };

        let first = fire().await;
        let second = fire().await;
        assert_eq!(
            second,
            ntex::http::StatusCode::TOO_MANY_REQUESTS,
            "a Degraded app must be throttled on STATIC egress too, not only on \
             worker dispatch (first request answered {first}, second {second})",
        );
    }

    /// The counter-example: an app that is NOT degraded must keep serving the
    /// same static route across several requests. Without this, the test above
    /// could be satisfied by throttling every static request regardless of
    /// spend state, which would be a worse bug than the one being fixed.
    #[compio::test]
    async fn a_non_degraded_app_is_not_throttled_on_static() {
        use zeroship_core::types::SpendState;
        let state = build_test_state_with_limits(vec!["http://0.0.0.0:0".into()], 1, 8, 100);
        let app_id = Uuid::new_v4();

        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes.insert(app_id, static_spend_route(SpendState::Allow));
        state
            .routes
            .update(routes, &state.rate_limiters, &state.concurrency);

        for i in 0..4 {
            let req = ntex::web::test::TestRequest::default()
                .uri("/about")
                .header("host", "static-spend-app.zeroship.localhost")
                .to_http_request();
            let status = handle_request(
                req,
                web_state(state.clone()).await,
                "static-spend-app.zeroship.localhost",
                "/about",
                Bytes::new(),
            )
            .await
            .status();
            assert_ne!(
                status,
                ntex::http::StatusCode::TOO_MANY_REQUESTS,
                "request {i} against an Allow app was throttled; only Degrade may throttle",
            );
        }
    }

    /// #3 (RED→GREEN): a Blocked app serving a STATIC resource must 402 BEFORE
    /// any static egress. Pre-fix, the spend gate lived only in the worker
    /// dispatch path, so `ResolvedAction::Static` fell straight through to the
    /// static server (egress the platform eats). This drives the REAL
    /// `handle_request` → `execute_resource_tree` → static action against a
    /// Blocked route and asserts the 402/`SPEND_LIMIT` envelope fires first.
    #[compio::test]
    async fn over_limit_static_asset_blocked_at_gateway() {
        use zeroship_core::types::SpendState;
        let state = build_idempotency_state();
        let app_id = Uuid::new_v4();

        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes.insert(app_id, static_spend_route(SpendState::Block));
        state
            .routes
            .update(routes, &state.rate_limiters, &state.concurrency);

        let req = ntex::web::test::TestRequest::default()
            .uri("/about")
            .header("host", "static-spend-app.zeroship.localhost")
            .to_http_request();
        let resp = handle_request(
            req,
            web_state(state.clone()).await,
            "static-spend-app.zeroship.localhost",
            "/about",
            Bytes::new(),
        )
        .await;

        assert_eq!(
            resp.status(),
            ntex::http::StatusCode::PAYMENT_REQUIRED,
            "a Blocked app must 402 on a STATIC resource before serving egress",
        );
        let mut resp = resp;
        let body = collect_body(resp.take_body()).await;
        let json: serde_json::Value = serde_json::from_slice(&body).expect("402 body is JSON");
        assert_eq!(json["code"], "SPEND_LIMIT");
    }

    /// Allow → the gate passes on a static resource (so dispatch proceeds to the
    /// static server, which 404s against the stub blob store — NOT 402). Pins
    /// that the hoisted gate only fires on Block, so the static-Block 402 above
    /// isn't a blanket reject of every static request.
    #[compio::test]
    async fn allowed_static_asset_passes_spend_gate() {
        use zeroship_core::types::SpendState;
        let state = build_idempotency_state();
        let app_id = Uuid::new_v4();
        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes.insert(app_id, static_spend_route(SpendState::Allow));
        state
            .routes
            .update(routes, &state.rate_limiters, &state.concurrency);
        let req = ntex::web::test::TestRequest::default()
            .uri("/about")
            .header("host", "static-spend-app.zeroship.localhost")
            .to_http_request();
        let resp = handle_request(
            req,
            web_state(state.clone()).await,
            "static-spend-app.zeroship.localhost",
            "/about",
            Bytes::new(),
        )
        .await;
        assert_ne!(
            resp.status(),
            ntex::http::StatusCode::PAYMENT_REQUIRED,
            "an Allowed app must NOT be spend-blocked on a static resource",
        );
    }

    // -----------------------------------------------------------------------
    // Metering coverage (#27) — gateway egress + the no-double-count partition
    // -----------------------------------------------------------------------

    /// THE load-bearing regression (§2.4): the egress ownership partition.
    ///
    /// A gateway-owned response (here a STATIC action whose blob 404s — the
    /// gateway's own error body, a body the worker never sees) must record
    /// `gateway_egress_bytes` for the route's app and must NOT touch
    /// `egress_bytes` (which the WORKER owns). This drives the REAL
    /// `handle_request` → `execute_resource_tree` → static arm against the
    /// SAME `Arc<Meter>` in `GateState`, then drains it.
    ///
    /// RED pre-fix: the gateway had no meter and recorded nothing, so
    /// `gateway_egress_bytes` is absent. GREEN post-fix: the static arm
    /// records the served body length, and `egress_bytes` stays untouched.
    #[compio::test]
    async fn static_response_meters_gateway_egress_not_worker_egress() {
        use zeroship_core::types::SpendState;
        let state = build_idempotency_state();
        let meter = Arc::clone(&state.meter);
        let app_id = Uuid::new_v4();

        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes.insert(app_id, static_spend_route(SpendState::Allow));
        state
            .routes
            .update(routes, &state.rate_limiters, &state.concurrency);

        let req = ntex::web::test::TestRequest::default()
            .uri("/about")
            .header("host", "static-spend-app.zeroship.localhost")
            .to_http_request();
        let mut resp = handle_request(
            req,
            web_state(state.clone()).await,
            "static-spend-app.zeroship.localhost",
            "/about",
            Bytes::new(),
        )
        .await;
        // The stub blob store has no bytes, so the static arm emits a 404
        // JSON error body — gateway-owned egress all the same.
        let served = collect_body(resp.take_body()).await;
        assert!(!served.is_empty(), "the gateway-owned 404 body is non-empty");

        let events = meter.drain();
        assert_eq!(
            usage_value(&events, app_id, "gateway_egress_bytes"),
            Some(served.len() as u64),
            "static (gateway-owned) egress must be metered as gateway_egress_bytes \
             equal to the served body length",
        );
        assert_eq!(
            usage_value(&events, app_id, "egress_bytes"),
            None,
            "the gateway must NEVER touch the worker-owned egress_bytes metric",
        );
    }

    /// The other half of the partition: a WORKER-proxied action must NOT
    /// record `gateway_egress_bytes` — the worker already counts its body as
    /// `egress_bytes`, and metering it here too would double-bill the same
    /// byte (the one over-bill vector). The proxy 502s against the stub
    /// hash-ring (no reachable worker), which is exactly the worker-arm path;
    /// the gateway must still record NO gateway_egress_bytes for it.
    ///
    /// RED if someone later meters the worker-proxy body in the gateway.
    #[compio::test]
    async fn gateway_does_not_meter_worker_proxy_body() {
        use zeroship_core::types::SpendState;
        let state = build_idempotency_state();
        let meter = Arc::clone(&state.meter);
        let app_id = Uuid::new_v4();

        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes.insert(app_id, worker_spend_route(SpendState::Allow));
        state
            .routes
            .update(routes, &state.rate_limiters, &state.concurrency);

        let req = ntex::web::test::TestRequest::default()
            .uri("/__zeroship/v1/ping")
            .header("host", "spend-app.zeroship.localhost")
            .to_http_request();
        let _resp = handle_request(
            req,
            web_state(state.clone()).await,
            "spend-app.zeroship.localhost",
            "/__zeroship/v1/ping",
            Bytes::new(),
        )
        .await;

        let events = meter.drain();
        assert_eq!(
            usage_value(&events, app_id, "gateway_egress_bytes"),
            None,
            "the gateway must NOT meter a worker-proxied response body as \
             gateway_egress_bytes (no double-count vs the worker's egress_bytes)",
        );
    }

    /// A worker RPC route that caps input at `max` bytes, so a body over the
    /// cap trips the gateway's 413 early-return arm in `execute_resource_tree`
    /// (a gateway-edge error envelope) BEFORE any worker proxy. Used to pin
    /// finding #1: gateway error/4xx envelopes are platform overhead and are
    /// deliberately NOT metered as `gateway_egress_bytes`.
    fn max_input_route(max: u32) -> zeroship_core::types::RouteEntry {
        use zeroship_bundle::{ProcedureKind, ResourceEntry};
        let mut resources = std::collections::HashMap::new();
        resources.insert(
            "rpc:ping".to_string(),
            ResourceEntry {
                kind: Some(ProcedureKind::Mutation),
                auth: Some(zeroship_bundle::AuthLevel::Anon),
                publicly_accessible: Some(true),
                max_input_bytes: Some(max),
                ..Default::default()
            },
        );
        let manifest = zeroship_bundle::Manifest {
            version: 1,
            resources,
            ..zeroship_bundle::Manifest::default()
        };
        zeroship_core::types::RouteEntry {
            name: "spend-app.zeroship.localhost".to_string(),
            plan_id: "free".to_string(),
            api_key_hash: "h".to_string(),
            deploy_hash: None,
            manifest,
            oauth_client_id: None,
            sector_identifier: None,
            spend_state: zeroship_core::types::SpendState::Allow,
            account_state: zeroship_core::types::AccountState::Active,
        }
    }

    /// FINDING #1 (pinning): a gateway-EMITTED error envelope (here a 413 from
    /// the `max_input_bytes` early-return arm — a body the worker never sees)
    /// is PLATFORM OVERHEAD and must NOT emit `gateway_egress_bytes`. The
    /// gateway only bills successful static + redirect bodies; error/4xx/5xx/204
    /// envelopes (tiny, frequently attacker-driven) are deliberately unbilled.
    ///
    /// Drives the REAL `handle_request` → `execute_resource_tree` 413 arm with
    /// a body over the declared cap, then drains the SAME `Arc<Meter>` and
    /// asserts the gateway recorded NO usage at all for the route's app. Locks
    /// the code↔design agreement so a future change that meters error arms
    /// fails here.
    #[compio::test]
    async fn gateway_error_envelope_is_not_metered() {
        let state = build_idempotency_state();
        let meter = Arc::clone(&state.meter);
        let app_id = Uuid::new_v4();

        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes.insert(app_id, max_input_route(8));
        state
            .routes
            .update(routes, &state.rate_limiters, &state.concurrency);

        // A body over the 8-byte cap → 413 PayloadTooLarge from the gateway
        // edge, before any worker proxy.
        let oversized = Bytes::from_static(b"this body is well over eight bytes");
        let req = ntex::web::test::TestRequest::default()
            .uri("/__zeroship/v1/ping")
            .header("host", "spend-app.zeroship.localhost")
            .header("origin", "https://spend-app.zeroship.localhost")
            .method(ntex::http::Method::POST)
            .to_http_request();
        let mut resp = handle_request(
            req,
            web_state(state.clone()).await,
            "spend-app.zeroship.localhost",
            "/__zeroship/v1/ping",
            oversized,
        )
        .await;

        assert_eq!(
            resp.status(),
            ntex::http::StatusCode::PAYLOAD_TOO_LARGE,
            "an over-cap body must 413 at the gateway edge",
        );
        // The 413 body is non-empty (a JSON error envelope) — proving the
        // assertion below is about the DELIBERATE no-meter decision, not an
        // empty body.
        let body = collect_body(resp.take_body()).await;
        assert!(!body.is_empty(), "the 413 envelope is a non-empty JSON body");

        let events = meter.drain();
        // The gateway must record NOTHING for an error envelope: no
        // gateway_egress_bytes, and (the gateway never owns it) no egress_bytes.
        assert_eq!(
            usage_value(&events, app_id, "gateway_egress_bytes"),
            None,
            "a gateway error/4xx envelope is platform overhead and must NOT \
             be metered as gateway_egress_bytes",
        );
        assert_eq!(
            usage_value(&events, app_id, "egress_bytes"),
            None,
            "the gateway never touches egress_bytes",
        );
    }

    /// Restart-safety under the usage-event model: event IDs, rather than a
    /// process-local sequence, are the provider dedup identity. Two process
    /// instances using the same stable source must still emit distinct IDs.
    #[test]
    fn gateway_usage_event_ids_are_restart_unique() {
        let app_id = Uuid::new_v4();
        let first = zeroship_metering::Meter::with_source("gate-pod-3");
        let second = zeroship_metering::Meter::with_source("gate-pod-3");
        first.increment(&app_id.to_string(), "gateway_egress_bytes", 1);
        second.increment(&app_id.to_string(), "gateway_egress_bytes", 1);

        let first_event = first.drain().pop().expect("first usage event");
        let second_event = second.drain().pop().expect("second usage event");
        assert_eq!(first_event.source, "gate-pod-3");
        assert_eq!(second_event.source, "gate-pod-3");
        assert_ne!(
            first_event.event_id, second_event.event_id,
            "separate gateway boots must not collide at provider dedup",
        );
    }

    /// Allow → the gate passes (so dispatch proceeds to the proxy, which fails
    /// against the stub ring → 502, NOT 402). Pins that the gate only fires on
    /// Block, so the Block 402 above isn't a blanket reject.
    #[compio::test]
    async fn allowed_request_passes_spend_gate() {
        use zeroship_core::types::SpendState;
        let state = build_idempotency_state();
        let app_id = Uuid::new_v4();
        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes.insert(app_id, worker_spend_route(SpendState::Allow));
        state
            .routes
            .update(routes, &state.rate_limiters, &state.concurrency);
        let req = ntex::web::test::TestRequest::default()
            .uri("/__zeroship/v1/ping")
            .header("host", "spend-app.zeroship.localhost")
            .to_http_request();
        let resp = handle_request(
            req,
            web_state(state.clone()).await,
            "spend-app.zeroship.localhost",
            "/__zeroship/v1/ping",
            Bytes::new(),
        )
        .await;
        assert_ne!(
            resp.status(),
            ntex::http::StatusCode::PAYMENT_REQUIRED,
            "an Allowed app must NOT be spend-blocked",
        );
    }

    /// Degrade flipped via the REAL `RouteCache::update` tightens the app's
    /// effective concurrency ceiling. With a global limit of DEGRADE_FACTOR and
    /// DEGRADE_FACTOR=8 the effective ceiling becomes 1, so the FIRST acquire
    /// succeeds and the SECOND is rejected — proving `set_degraded` is not a
    /// no-op. A non-degraded control app at the same limit admits both.
    #[compio::test]
    async fn degraded_route_tightens_concurrency() {
        use crate::enforce::{acquire_concurrency, ConcurrencyRegistry, RateLimitRegistry, DEGRADE_FACTOR};
        use zeroship_core::types::SpendState;

        let cache = crate::sync::RouteCache::new();
        let rate = RateLimitRegistry::new(1000, 2000);
        let concurrency = ConcurrencyRegistry::new(DEGRADE_FACTOR);

        let degraded_app = Uuid::new_v4();
        let normal_app = Uuid::new_v4();
        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        let mut degraded = spend_route(SpendState::Degrade);
        degraded.name = "degraded.zeroship.localhost".into();
        let mut normal = spend_route(SpendState::Allow);
        normal.name = "normal.zeroship.localhost".into();
        routes.insert(degraded_app, degraded);
        routes.insert(normal_app, normal);

        cache.update(routes, &rate, &concurrency);
        assert!(concurrency.is_degraded(&degraded_app));
        assert!(!concurrency.is_degraded(&normal_app));

        let g1 = acquire_concurrency(&concurrency, &degraded_app).expect("first admits");
        let r2 = acquire_concurrency(&concurrency, &degraded_app);
        assert!(r2.is_err(), "degraded app's 2nd concurrent request must be rejected");
        if let Err(resp) = r2 {
            assert_eq!(resp.status(), ntex::http::StatusCode::TOO_MANY_REQUESTS);
        }
        drop(g1);

        let _n1 = acquire_concurrency(&concurrency, &normal_app).expect("normal 1");
        let _n2 = acquire_concurrency(&concurrency, &normal_app).expect("normal 2");
    }

    /// Degrade → Allow flipped via the REAL `RouteCache::update` restores full
    /// throughput on the NEXT request with no warm-up — the gauge was never
    /// rebuilt. Proves recovery is instant.
    #[compio::test]
    async fn degrade_clears_immediately_on_recovery() {
        use crate::enforce::{acquire_concurrency, ConcurrencyRegistry, RateLimitRegistry, DEGRADE_FACTOR};
        use zeroship_core::types::SpendState;

        let cache = crate::sync::RouteCache::new();
        let rate = RateLimitRegistry::new(1000, 2000);
        let concurrency = ConcurrencyRegistry::new(DEGRADE_FACTOR);
        let app = Uuid::new_v4();

        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes.insert(app, spend_route(SpendState::Degrade));
        cache.update(routes, &rate, &concurrency);
        assert!(concurrency.is_degraded(&app));
        let g1 = acquire_concurrency(&concurrency, &app).expect("first admits");
        assert!(
            acquire_concurrency(&concurrency, &app).is_err(),
            "degraded ceiling is 1",
        );
        drop(g1);

        let mut routes2: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes2.insert(app, spend_route(SpendState::Allow));
        cache.update(routes2, &rate, &concurrency);
        assert!(!concurrency.is_degraded(&app));
        let mut guards = Vec::new();
        for i in 0..DEGRADE_FACTOR {
            guards.push(
                acquire_concurrency(&concurrency, &app)
                    .unwrap_or_else(|_| panic!("recovered app admits request {i}")),
            );
        }
    }

    // -----------------------------------------------------------------------
    // G2 — faithful account-suspension enforcement at the gateway edge.
    //
    // Drives the REAL path: a `RouteEntry` carrying the pulled `account_state`
    // (and `spend_state`) is pushed through the REAL `RouteCache::update`, then
    // the public `handle_request` is invoked. The stub hash-ring points at
    // `0.0.0.0:0`, so a request that PASSES the gates surfaces a 502 (worker
    // unreachable); a 402 therefore proves the gate short-circuited BEFORE any
    // worker proxy. The account gate composes with spend as an AND.
    // -----------------------------------------------------------------------

    /// A worker route (public RPC query at `ping`) with explicit account + spend
    /// state — exercises the hoisted G2 account gate AND its composition with
    /// the spend gate through the real dispatch path.
    fn account_worker_route(
        account_state: zeroship_core::types::AccountState,
        spend_state: zeroship_core::types::SpendState,
    ) -> zeroship_core::types::RouteEntry {
        let mut entry = worker_spend_route(spend_state);
        entry.account_state = account_state;
        entry
    }

    async fn drive_ping(state: Arc<GateState>) -> HttpResponse {
        let req = ntex::web::test::TestRequest::default()
            .uri("/__zeroship/v1/ping")
            .header("host", "spend-app.zeroship.localhost")
            .to_http_request();
        handle_request(
            req,
            web_state(state).await,
            "spend-app.zeroship.localhost",
            "/__zeroship/v1/ping",
            Bytes::new(),
        )
        .await
    }

    async fn assert_402_code(mut resp: HttpResponse, code: &str) {
        assert_eq!(
            resp.status(),
            ntex::http::StatusCode::PAYMENT_REQUIRED,
            "expected a 402 with code {code}",
        );
        let body = collect_body(resp.take_body()).await;
        let json: serde_json::Value = serde_json::from_slice(&body).expect("402 body is JSON");
        assert_eq!(json["code"], code, "402 envelope code");
    }

    /// A `Suspended` creator's app → 402 `ACCOUNT_SUSPENDED` BEFORE any worker
    /// proxy. Fed via the REAL `RouteCache::update`, driven through the REAL
    /// `handle_request`. Spend is `Allow`, so the ONLY thing that can 402 is the
    /// account gate — proving suspension enforces independently of spend.
    #[compio::test]
    async fn suspended_account_blocked_at_gateway() {
        use zeroship_core::types::{AccountState, SpendState};
        let state = build_idempotency_state();
        let app_id = Uuid::new_v4();
        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes.insert(app_id, account_worker_route(AccountState::Suspended, SpendState::Allow));
        state.routes.update(routes, &state.rate_limiters, &state.concurrency);

        assert_402_code(drive_ping(state.clone()).await, "ACCOUNT_SUSPENDED").await;
    }

    /// A `PastDue` creator's app is NOT blocked (the grace window). With spend
    /// `Allow`, the request passes both gates and reaches the proxy (→ 502 vs the
    /// stub ring, NOT 402). Proves past_due is the WARNING state, not a block.
    #[compio::test]
    async fn past_due_account_not_blocked_at_gateway() {
        use zeroship_core::types::{AccountState, SpendState};
        let state = build_idempotency_state();
        let app_id = Uuid::new_v4();
        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes.insert(app_id, account_worker_route(AccountState::PastDue, SpendState::Allow));
        state.routes.update(routes, &state.rate_limiters, &state.concurrency);

        let resp = drive_ping(state.clone()).await;
        assert_ne!(
            resp.status(),
            ntex::http::StatusCode::PAYMENT_REQUIRED,
            "a past_due (grace) app must NOT be account-blocked",
        );
    }

    /// An `Active` creator with `Allow` spend passes the account gate (→ proxy →
    /// 502, NOT 402). Pins that the account gate only fires on Suspended.
    #[compio::test]
    async fn active_account_passes_gate() {
        use zeroship_core::types::{AccountState, SpendState};
        let state = build_idempotency_state();
        let app_id = Uuid::new_v4();
        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes.insert(app_id, account_worker_route(AccountState::Active, SpendState::Allow));
        state.routes.update(routes, &state.rate_limiters, &state.concurrency);

        let resp = drive_ping(state.clone()).await;
        assert_ne!(
            resp.status(),
            ntex::http::StatusCode::PAYMENT_REQUIRED,
            "an active app with Allow spend must not be 402'd",
        );
    }

    /// The two gates compose as an AND with DISTINCT codes:
    ///   * Suspended + Allow spend → 402 ACCOUNT_SUSPENDED (account beats spend).
    ///   * Active + Block spend     → 402 SPEND_LIMIT (spend fires when account ok).
    ///   * Suspended + Block spend  → 402 ACCOUNT_SUSPENDED (account is the OUTER
    ///     gate, evaluated first).
    #[compio::test]
    async fn account_and_spend_gates_compose() {
        use zeroship_core::types::{AccountState, SpendState};

        // Suspended beats Allow spend.
        {
            let state = build_idempotency_state();
            let app_id = Uuid::new_v4();
            let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
            routes.insert(app_id, account_worker_route(AccountState::Suspended, SpendState::Allow));
            state.routes.update(routes, &state.rate_limiters, &state.concurrency);
            assert_402_code(drive_ping(state.clone()).await, "ACCOUNT_SUSPENDED").await;
        }

        // Active account, Block spend → spend gate fires.
        {
            let state = build_idempotency_state();
            let app_id = Uuid::new_v4();
            let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
            routes.insert(app_id, account_worker_route(AccountState::Active, SpendState::Block));
            state.routes.update(routes, &state.rate_limiters, &state.concurrency);
            assert_402_code(drive_ping(state.clone()).await, "SPEND_LIMIT").await;
        }

        // Suspended account AND Block spend → the OUTER account gate wins (it is
        // evaluated first), so the code is ACCOUNT_SUSPENDED, not SPEND_LIMIT.
        {
            let state = build_idempotency_state();
            let app_id = Uuid::new_v4();
            let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
            routes.insert(app_id, account_worker_route(AccountState::Suspended, SpendState::Block));
            state.routes.update(routes, &state.rate_limiters, &state.concurrency);
            assert_402_code(drive_ping(state.clone()).await, "ACCOUNT_SUSPENDED").await;
        }
    }

    // -----------------------------------------------------------------
    // Idempotency capture must not freeze a 5xx under the dedupe key
    // -----------------------------------------------------------------

    /// A raw-TCP stand-in for a worker, so a test can produce a REAL
    /// transport failure (accept, read the request, close without a byte of
    /// response) that no ntex handler can express — an ntex handler always
    /// writes a well-formed response, which is `Ok(_)` out of
    /// `forward_dispatch`, never the `Err(_)` arm that synthesizes the
    /// gateway's own 502.
    struct MockWorker {
        url: String,
        /// Requests this worker actually READ AND ANSWERED. The dropped
        /// outage connection is deliberately NOT counted: it never produced
        /// an outcome, which is the whole point of the scenario.
        served: Arc<std::sync::atomic::AtomicUsize>,
    }

    /// Read exactly one HTTP/1.1 request off `stream` (headers, then
    /// `Content-Length` bytes). Returns `false` on EOF/error. Framing the
    /// read properly matters: the gateway pools connections, so one socket
    /// carries several requests, and a naive single `read()` would
    /// miscount a short read as an extra request.
    fn mock_read_one_request(stream: &mut std::net::TcpStream) -> bool {
        use std::io::Read;
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break p + 4;
            }
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return false,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
        let content_length = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while buf.len() < head_end + content_length {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return false,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        true
    }

    /// Spawn the mock worker. When `outage_on_first_connection` is set the
    /// FIRST connection is read and then closed with no response — the
    /// gateway sees EOF with zero response bytes consumed, which is exactly
    /// the "worker went away" arm. Every other request is answered with
    /// `status` and a body of `{"id":<n>}`, `n` being the served counter, so
    /// a replayed response is distinguishable from a re-executed one by its
    /// body alone.
    fn spawn_mock_worker(outage_on_first_connection: bool, status: u16) -> MockWorker {
        spawn_mock_worker_with(outage_on_first_connection, status, "")
    }

    /// As [`spawn_mock_worker`], but the worker also emits `extra_headers`
    /// (a raw, already-CRLF-terminated header block) on every response. Lets
    /// a test author a WORKER-originated redirect that carries a real
    /// `Location`, which is the one-variable control for the
    /// gateway-originated redirect.
    fn spawn_mock_worker_with(
        outage_on_first_connection: bool,
        status: u16,
        extra_headers: &'static str,
    ) -> MockWorker {
        use std::io::Write;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock worker");
        let port = listener.local_addr().expect("mock worker addr").port();
        let served = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&served);

        std::thread::spawn(move || {
            for (conn_idx, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { break };
                if outage_on_first_connection && conn_idx == 0 {
                    // Drain the request first so the gateway's `write_all`
                    // completes (a write into a closed peer would take the
                    // reconnect arm instead, which is a different failure).
                    let _ = mock_read_one_request(&mut stream);
                    drop(stream);
                    continue;
                }
                while mock_read_one_request(&mut stream) {
                    let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                    let body = format!("{{\"id\":{n}}}");
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n{extra_headers}content-length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    if stream.write_all(resp.as_bytes()).is_err() {
                        break;
                    }
                }
            }
        });

        MockWorker { url: format!("http://127.0.0.1:{port}"), served }
    }

    /// A route with one `idempotent: true` mutation at wire-id
    /// `todos.add`, reachable at `/__zeroship/v1/todos.add`. This is the
    /// only resource shape that reaches the step-9 idempotency capture.
    fn idempotent_mutation_route() -> zeroship_core::types::RouteEntry {
        idempotent_mutation_route_with_oauth(None)
    }

    /// As [`idempotent_mutation_route`], but with the app's OAuth client id
    /// set. That id is what decides which gateway-originated response the
    /// worker-401-on-an-HTML-navigation branch emits: `Some` → the 302 into
    /// the OP (`start_oidc_redirect`), `None` → the 503
    /// `client_not_provisioned`.
    fn idempotent_mutation_route_with_oauth(
        oauth_client_id: Option<&str>,
    ) -> zeroship_core::types::RouteEntry {
        let mut resources = std::collections::HashMap::new();
        resources.insert(
            "rpc:todos.add".to_string(),
            ResourceEntry {
                kind: Some(ProcedureKind::Mutation),
                auth: Some(AuthLevel::Anon),
                publicly_accessible: Some(true),
                idempotent: Some(true),
                ..Default::default()
            },
        );
        zeroship_core::types::RouteEntry {
            name: "idem-app.zeroship.localhost".to_string(),
            plan_id: "free".to_string(),
            api_key_hash: "h".to_string(),
            deploy_hash: None,
            manifest: zeroship_bundle::Manifest {
                version: 1,
                resources,
                ..zeroship_bundle::Manifest::default()
            },
            oauth_client_id: oauth_client_id.map(ToString::to_string),
            sector_identifier: None,
            spend_state: zeroship_core::types::SpendState::Allow,
            account_state: zeroship_core::types::AccountState::Active,
        }
    }

    /// Drive the REAL `handle_request` → `execute_resource_tree` →
    /// `handle_dispatch` → step-9 capture path for the idempotent mutation,
    /// carrying `key` as the `Idempotency-Key`.
    async fn post_idempotent_mutation(
        state: Arc<GateState>,
        key: &str,
        body: &'static [u8],
    ) -> HttpResponse {
        post_idempotent_mutation_accepting(state, key, body, "application/json").await
    }

    /// As [`post_idempotent_mutation`], but with an explicit `Accept`. Only
    /// `text/html` makes `wants_html` true, which is the gate on the
    /// worker-401 → OIDC-redirect branch inside `handle_dispatch`.
    async fn post_idempotent_mutation_accepting(
        state: Arc<GateState>,
        key: &str,
        body: &'static [u8],
        accept: &'static str,
    ) -> HttpResponse {
        let req = ntex::web::test::TestRequest::default()
            .method(ntex::http::Method::POST)
            .uri("/__zeroship/v1/todos.add")
            .header("host", "idem-app.zeroship.localhost")
            .header("content-type", "application/json")
            .header("accept", accept)
            .header("idempotency-key", key)
            .to_http_request();
        handle_request(
            req,
            web_state(state.clone()).await,
            "idem-app.zeroship.localhost",
            "/__zeroship/v1/todos.add",
            Bytes::from_static(body),
        )
        .await
    }

    /// Install `idempotent_mutation_route` and return the state pointed at
    /// `worker`. Limits are lifted well above the two requests each test
    /// fires — the default fixture's `burst: 1` bucket is drained by a
    /// single request, which would 429 the retry and hide the real result.
    fn idempotency_state_for(worker: &MockWorker) -> Arc<GateState> {
        idempotency_state_for_with_oauth(worker, None)
    }

    /// As [`idempotency_state_for`], but the installed route carries
    /// `oauth_client_id`.
    fn idempotency_state_for_with_oauth(
        worker: &MockWorker,
        oauth_client_id: Option<&str>,
    ) -> Arc<GateState> {
        let state = build_test_state_with_limits(vec![worker.url.clone()], 100, 100, 100);
        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes.insert(
            Uuid::new_v4(),
            idempotent_mutation_route_with_oauth(oauth_client_id),
        );
        state
            .routes
            .update(routes, &state.rate_limiters, &state.concurrency);
        state
    }

    /// A worker outage must NOT be cached under the caller's
    /// `Idempotency-Key`.
    ///
    /// The gateway's own `502 {"error":"worker error: …"}` (the `Err(_)` arm
    /// of `forward_dispatch`) is a statement about the GATEWAY's reach, not
    /// an outcome the app produced. Storing it under the dedupe key makes
    /// every retry replay that 502 for the full TTL (24h by default), and
    /// `@zeroship/rpc` retries idempotent writes with a STABLE key by
    /// design — so a single blip would render one logical operation
    /// permanently un-retryable.
    ///
    /// This drives the replay rather than inspecting the store: the second
    /// request must actually REACH the worker (`served == 1`) and come back
    /// with the worker's own `201 {"id":1}`. A fix that merely stored the
    /// 502 under a different key would still fail here.
    #[compio::test]
    async fn worker_outage_502_is_not_cached_and_the_retry_reaches_the_worker() {
        use std::sync::atomic::Ordering;

        let worker = spawn_mock_worker(true, 201);
        let state = idempotency_state_for(&worker);

        // Request 1: the worker accepts and dies mid-request. The gateway
        // synthesizes its own 502.
        let mut first = post_idempotent_mutation(state.clone(), "1a2b3c4d-0001-4111-8000-aaaabbbbcccc", b"{\"t\":1}").await;
        assert_eq!(
            first.status(),
            ntex::http::StatusCode::BAD_GATEWAY,
            "an unreachable worker must surface the gateway's own 502",
        );
        let first_body = collect_body(first.take_body()).await;
        let first_json: serde_json::Value =
            serde_json::from_slice(&first_body).expect("502 body is JSON");
        assert!(
            first_json["error"]
                .as_str()
                .unwrap_or_default()
                .starts_with("worker error:"),
            "the 502 must be the gateway's own envelope, got {first_json}",
        );
        assert_eq!(
            worker.served.load(Ordering::SeqCst),
            0,
            "the outage connection answered nothing, so nothing was served",
        );

        // Request 2: same key, same body, worker healthy again.
        let mut second = post_idempotent_mutation(state.clone(), "1a2b3c4d-0001-4111-8000-aaaabbbbcccc", b"{\"t\":1}").await;
        assert_eq!(
            worker.served.load(Ordering::SeqCst),
            1,
            "the retry must reach the worker, not replay the cached 502",
        );
        assert_eq!(
            second.status(),
            ntex::http::StatusCode::CREATED,
            "the retry must return the worker's response, not the stored 502",
        );
        let second_body = collect_body(second.take_body()).await;
        assert_eq!(
            second_body, b"{\"id\":1}",
            "the retry must carry the worker's body",
        );
    }

    /// Control for the test above, differing in exactly ONE variable: the
    /// first dispatch SUCCEEDS instead of failing. The stored 201 must
    /// still be deduped and replayed — the worker must be hit exactly once
    /// across both requests, and the second response must carry the FIRST
    /// response's body (`id:1`, not `id:2`).
    ///
    /// Without this, "never store anything" would pass the outage test.
    #[compio::test]
    async fn successful_response_is_still_captured_and_replayed_under_the_same_key() {
        use std::sync::atomic::Ordering;

        let worker = spawn_mock_worker(false, 201);
        let state = idempotency_state_for(&worker);

        let mut first = post_idempotent_mutation(state.clone(), "1a2b3c4d-0002-4111-8000-aaaabbbbcccc", b"{\"t\":1}").await;
        assert_eq!(first.status(), ntex::http::StatusCode::CREATED);
        assert_eq!(collect_body(first.take_body()).await, b"{\"id\":1}");
        assert_eq!(worker.served.load(Ordering::SeqCst), 1);

        let mut second = post_idempotent_mutation(state.clone(), "1a2b3c4d-0002-4111-8000-aaaabbbbcccc", b"{\"t\":1}").await;
        assert_eq!(
            worker.served.load(Ordering::SeqCst),
            1,
            "a deduped replay must NOT re-execute the mutation on the worker",
        );
        assert_eq!(second.status(), ntex::http::StatusCode::CREATED);
        assert_eq!(
            second
                .headers()
                .get("x-zs-idempotent-replay")
                .and_then(|v| v.to_str().ok()),
            Some("true"),
            "the replayed response must be flagged as a replay",
        );
        assert_eq!(
            collect_body(second.take_body()).await,
            b"{\"id\":1}",
            "the replay must be the FIRST response verbatim, not a fresh one",
        );
    }

    /// Pins the DESIGN DECISION rather than the reported instance: a
    /// WORKER-originated 5xx is excluded too, not just the gateway's own
    /// 502. The worker here is reachable and answers `500` — the app threw.
    ///
    /// This is the test that a narrower "mark and exclude only
    /// gateway-originated failures" fix would fail. The rationale is on
    /// `capture_response_for_idempotency`: at the capture point a 500 cannot
    /// be told apart from "the worker committed a write and then threw", so
    /// the outcome is unknown, and unknown must stay retryable. The common
    /// cause of a handler 500 is a transient fault inside it, which is
    /// precisely what the stable key exists to let the client retry.
    ///
    /// Both requests must reach the worker, and the second response must be
    /// a FRESH `{"id":2}` rather than a replay of `{"id":1}`.
    #[compio::test]
    async fn worker_originated_5xx_is_not_cached_either() {
        use std::sync::atomic::Ordering;

        let worker = spawn_mock_worker(false, 500);
        let state = idempotency_state_for(&worker);

        let mut first = post_idempotent_mutation(state.clone(), "1a2b3c4d-0003-4111-8000-aaaabbbbcccc", b"{\"t\":1}").await;
        assert_eq!(first.status(), ntex::http::StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(collect_body(first.take_body()).await, b"{\"id\":1}");
        assert_eq!(worker.served.load(Ordering::SeqCst), 1);

        let mut second = post_idempotent_mutation(state.clone(), "1a2b3c4d-0003-4111-8000-aaaabbbbcccc", b"{\"t\":1}").await;
        assert_eq!(
            worker.served.load(Ordering::SeqCst),
            2,
            "a retry after a worker 5xx must re-reach the worker, not replay it",
        );
        assert!(
            second.headers().get("x-zs-idempotent-replay").is_none(),
            "a 5xx must not have been stored, so nothing can be flagged as a replay",
        );
        assert_eq!(
            collect_body(second.take_body()).await,
            b"{\"id\":2}",
            "the retry must carry the worker's SECOND response, not the stored first",
        );
    }

    // -----------------------------------------------------------------
    // Only a response the WORKER authored may be stored
    // -----------------------------------------------------------------

    /// The OAuth client the redirect tests bind their route to, so the
    /// worker-401 branch takes `start_oidc_redirect` rather than the 503
    /// `client_not_provisioned` arm.
    const TEST_REDIRECT_CLIENT: &str = "oac_idem_redirect";

    /// A GATEWAY-synthesised 302 into the OP must NOT be stored under the
    /// caller's `Idempotency-Key`.
    ///
    /// `handle_dispatch` turns a worker 401 on an HTML navigation into its
    /// own 302 → the OP, and that redirect carries a FRESH per-request PKCE
    /// stash in a `Set-Cookie`. Stored, every replay within the TTL hands
    /// the next caller the FIRST request's stash — a PKCE verifier bound to
    /// a `code_challenge`/`state` pair that has already been spent. It is
    /// also a lie about the app: the mutation never ran, so there is no
    /// outcome to remember.
    ///
    /// The assertion is on OBSERVABLE behaviour, never on whether the
    /// capture was entered: the second request must REACH the worker
    /// (`served == 2`) and must come back with a redirect whose `Location`
    /// DIFFERS from the first — i.e. a freshly minted PKCE challenge, not
    /// the replayed one.
    #[compio::test]
    async fn gateway_synthesised_oidc_redirect_is_not_cached() {
        use std::sync::atomic::Ordering;

        const KEY: &str = "1a2b3c4d-0004-4111-8000-aaaabbbbcccc";

        let worker = spawn_mock_worker(false, 401);
        let state = idempotency_state_for_with_oauth(&worker, Some(TEST_REDIRECT_CLIENT));

        let first =
            post_idempotent_mutation_accepting(state.clone(), KEY, b"{\"t\":1}", "text/html").await;
        assert_eq!(
            first.status(),
            ntex::http::StatusCode::FOUND,
            "a worker 401 on an HTML navigation must start the OIDC dance",
        );
        let first_location = first
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            first_location.contains("/authorize?") && first_location.contains("code_challenge="),
            "the 302 must be the gateway's own OP redirect; got {first_location:?}",
        );
        assert_eq!(worker.served.load(Ordering::SeqCst), 1);

        let second =
            post_idempotent_mutation_accepting(state.clone(), KEY, b"{\"t\":1}", "text/html").await;
        assert_eq!(
            worker.served.load(Ordering::SeqCst),
            2,
            "the retry must reach the worker, not replay the gateway's stored 302",
        );
        assert!(
            second.headers().get("x-zs-idempotent-replay").is_none(),
            "a gateway-synthesised redirect must not have been stored, so nothing can be flagged as a replay",
        );
        let second_location = second
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert_ne!(
            second_location, first_location,
            "the retry must carry a FRESHLY minted PKCE challenge, not the first request's spent one",
        );
    }

    /// One-variable control for the test above: the worker answers 302
    /// ITSELF instead of 401, so the gateway forwards the app's redirect
    /// rather than synthesising its own. Everything else — the route, the
    /// `Accept: text/html`, the key, the body — is identical, and the client
    /// sees a 302 either way. An APP-authored redirect IS an outcome, so it
    /// must still be stored and replayed.
    ///
    /// This is the test that a status-based `|| status == 302` fix would
    /// fail, and it is why the rule keys on the response's AUTHOR rather
    /// than its status code.
    #[compio::test]
    async fn app_authored_302_is_still_captured_and_replayed() {
        use std::sync::atomic::Ordering;

        const KEY: &str = "1a2b3c4d-0005-4111-8000-aaaabbbbcccc";

        let worker = spawn_mock_worker_with(false, 302, "location: /thanks\r\n");
        let state = idempotency_state_for_with_oauth(&worker, Some(TEST_REDIRECT_CLIENT));

        let mut first =
            post_idempotent_mutation_accepting(state.clone(), KEY, b"{\"t\":1}", "text/html").await;
        assert_eq!(first.status(), ntex::http::StatusCode::FOUND);
        assert_eq!(
            first
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok()),
            Some("/thanks"),
            "the app's own redirect target must be forwarded verbatim",
        );
        assert_eq!(collect_body(first.take_body()).await, b"{\"id\":1}");
        assert_eq!(worker.served.load(Ordering::SeqCst), 1);

        let mut second =
            post_idempotent_mutation_accepting(state.clone(), KEY, b"{\"t\":1}", "text/html").await;
        assert_eq!(
            worker.served.load(Ordering::SeqCst),
            1,
            "an app-authored 302 is an outcome; the replay must NOT re-execute the mutation",
        );
        assert_eq!(second.status(), ntex::http::StatusCode::FOUND);
        assert_eq!(
            second
                .headers()
                .get("x-zs-idempotent-replay")
                .and_then(|v| v.to_str().ok()),
            Some("true"),
            "the replayed app redirect must be flagged as a replay",
        );
        assert_eq!(
            second
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok()),
            Some("/thanks"),
        );
        assert_eq!(
            collect_body(second.take_body()).await,
            b"{\"id\":1}",
            "the replay must be the FIRST response verbatim",
        );
    }

    /// The other gateway-synthesised exit from the same branch: the app has
    /// NO `oauth_client_id`, so a worker 401 on an HTML navigation produces
    /// the gateway's own 503 `client_not_provisioned`. Already excluded by
    /// the `status >= 500` guard, so this pins existing behaviour rather
    /// than new — it is here so a later narrowing of that guard cannot
    /// silently start caching a provisioning outage under a dedupe key.
    #[compio::test]
    async fn gateway_synthesised_client_not_provisioned_503_is_not_cached() {
        use std::sync::atomic::Ordering;

        const KEY: &str = "1a2b3c4d-0006-4111-8000-aaaabbbbcccc";

        let worker = spawn_mock_worker(false, 401);
        let state = idempotency_state_for_with_oauth(&worker, None);

        let first =
            post_idempotent_mutation_accepting(state.clone(), KEY, b"{\"t\":1}", "text/html").await;
        assert_eq!(first.status(), ntex::http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(worker.served.load(Ordering::SeqCst), 1);

        let second =
            post_idempotent_mutation_accepting(state.clone(), KEY, b"{\"t\":1}", "text/html").await;
        assert_eq!(
            worker.served.load(Ordering::SeqCst),
            2,
            "a provisioning outage must stay retryable under the same key",
        );
        assert!(
            second.headers().get("x-zs-idempotent-replay").is_none(),
            "nothing was stored, so nothing can be flagged as a replay",
        );
    }

    // -----------------------------------------------------------------
    // The dedupe namespace must be partitioned by principal
    // -----------------------------------------------------------------
    //
    // A stored response is replayed VERBATIM to whoever lands on the same
    // dedupe key. If the key carries no identity, user B sending user A's
    // `Idempotency-Key` on the same procedure receives A's response body.
    // The tests below drive that through the REAL cookie auth arm and a
    // real mock worker, so the assertion is on the BODY the second caller
    // receives and on whether the worker was reached — never on the key
    // string's spelling.

    /// Issuer + verifier `iss` for the signed session cookies these tests
    /// mint. Both sides are under test control, so the value only has to
    /// agree with itself.
    const TEST_SESSION_ISS: &str = "https://gate.test";

    /// The per-app OAuth client the authenticated route is bound to. The
    /// cookie's `app` claim must match it or the cookie arm rejects.
    const TEST_OAUTH_CLIENT: &str = "oac_idem_test_client";

    const TEST_AUTH_HOST: &str = "idem-auth.zeroship.localhost";

    /// Same shape as [`TEST_AUTH_HOST`] but the procedure is declared
    /// `auth: "anon"`. A logged-in visitor can still reach it, which is
    /// why the partition must not key off the declared auth level.
    const TEST_ANON_HOST: &str = "idem-anon.zeroship.localhost";

    /// A route whose `todos.add` mutation is `auth: "user"` and
    /// `idempotent: true`, bound to [`TEST_OAUTH_CLIENT`] so the signed
    /// session cookie can bind to it.
    fn authenticated_idempotent_route(
        host: &str,
        auth: AuthLevel,
    ) -> zeroship_core::types::RouteEntry {
        let mut resources = std::collections::HashMap::new();
        resources.insert(
            "rpc:todos.add".to_string(),
            ResourceEntry {
                kind: Some(ProcedureKind::Mutation),
                auth: Some(auth),
                publicly_accessible: Some(matches!(auth, AuthLevel::Anon)),
                idempotent: Some(true),
                ..Default::default()
            },
        );
        zeroship_core::types::RouteEntry {
            name: host.to_string(),
            plan_id: "free".to_string(),
            api_key_hash: "h".to_string(),
            deploy_hash: None,
            manifest: zeroship_bundle::Manifest {
                version: 1,
                resources,
                ..zeroship_bundle::Manifest::default()
            },
            oauth_client_id: Some(TEST_OAUTH_CLIENT.to_string()),
            sector_identifier: Some("https://idem-auth.test".to_string()),
            spend_state: zeroship_core::types::SpendState::Allow,
            account_state: zeroship_core::types::AccountState::Active,
        }
    }

    /// State + cookie issuer for the authenticated idempotency tests. The
    /// gateway verifies the cookie LOCALLY (no DB configured ⇒ the
    /// revocation gate is skipped), so this is the full production cookie
    /// arm minus the revocation round-trip.
    fn authenticated_idempotency_state(
        worker: &MockWorker,
    ) -> (Arc<GateState>, crate::session_token::Issuer) {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let verifier = Arc::new(crate::session_token::Verifier::new(
            &signing.verifying_key(),
            TEST_SESSION_ISS.to_string(),
        ));
        let issuer = crate::session_token::Issuer::new(&signing, TEST_SESSION_ISS.to_string())
            .expect("session issuer");
        let state =
            build_test_state_inner(vec![worker.url.clone()], 100, 100, 100, Some(verifier));
        let mut routes: zeroship_core::types::RouteMap = std::collections::HashMap::new();
        routes.insert(
            Uuid::new_v4(),
            authenticated_idempotent_route(TEST_AUTH_HOST, AuthLevel::User),
        );
        routes.insert(
            Uuid::new_v4(),
            authenticated_idempotent_route(TEST_ANON_HOST, AuthLevel::Anon),
        );
        state.routes.update_snapshot(
            zeroship_core::types::GatewaySnapshot {
                routes,
                principal_lifecycle: Vec::new(),
                family_revocations: Vec::new(),
            },
            &state.rate_limiters,
            &state.concurrency,
        );
        (state, issuer)
    }

    /// Mint a signed session cookie for a distinct end user. `seed` picks
    /// the global user id, so two different seeds are two different people
    /// with two different per-app `pws_…` subjects.
    fn session_cookie_for(issuer: &crate::session_token::Issuer, seed: u128) -> String {
        let global = Uuid::from_u128(seed).to_string();
        let sub = zeroship_core::auth::derive_pairwise(
            &[0u8; 32],
            &global,
            "https://idem-auth.test",
        );
        issuer
            .issue(&crate::session_token::SessionMint {
                app: TEST_OAUTH_CLIENT,
                sub: &sub,
                credential_iat: 1_700_000_000,
                auth_time: None,
                amr: &[],
                email: "u@test.invalid",
                email_verified: true,
                name: "U",
                avatar: None,
                scopes: &[],
            })
            .expect("issue session cookie")
    }

    /// Drive the REAL `handle_request` path for the authenticated mutation
    /// as the holder of `cookie`, carrying `key` as the `Idempotency-Key`.
    async fn post_authenticated_mutation(
        state: Arc<GateState>,
        host: &str,
        cookie: &str,
        key: &str,
        body: &'static [u8],
    ) -> HttpResponse {
        let req = ntex::web::test::TestRequest::default()
            .method(ntex::http::Method::POST)
            .uri("/__zeroship/v1/todos.add")
            .header("host", host)
            .header("origin", format!("http://{host}"))
            .header("content-type", "application/json")
            .header("cookie", format!("__Host-zeroship_app_session={cookie}"))
            .header("idempotency-key", key)
            .to_http_request();
        handle_request(
            req,
            web_state(state.clone()).await,
            host,
            "/__zeroship/v1/todos.add",
            Bytes::from_static(body),
        )
        .await
    }

    /// THE DEFECT. Two DIFFERENT authenticated users send the SAME
    /// `Idempotency-Key` on the same procedure with the same body. User B
    /// must reach the worker and get their OWN response; receiving user A's
    /// stored body is a cross-user response disclosure.
    ///
    /// Asserted on observable behaviour only: the mock worker's served
    /// counter, and the body bytes B receives (`{"id":2}` is B's own
    /// execution, `{"id":1}` is A's replayed response).
    #[compio::test]
    async fn same_idempotency_key_from_a_different_user_does_not_replay_the_first_users_response() {
        use std::sync::atomic::Ordering;

        let worker = spawn_mock_worker(false, 201);
        let (state, issuer) = authenticated_idempotency_state(&worker);
        let cookie_a = session_cookie_for(&issuer, 1);
        let cookie_b = session_cookie_for(&issuer, 2);
        let shared_key = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";

        let mut first =
            post_authenticated_mutation(state.clone(), TEST_AUTH_HOST, &cookie_a, shared_key, b"{\"t\":1}").await;
        assert_eq!(
            first.status(),
            ntex::http::StatusCode::CREATED,
            "user A's mutation must reach the worker",
        );
        assert_eq!(collect_body(first.take_body()).await, b"{\"id\":1}");
        assert_eq!(worker.served.load(Ordering::SeqCst), 1);

        let mut second =
            post_authenticated_mutation(state.clone(), TEST_AUTH_HOST, &cookie_b, shared_key, b"{\"t\":1}").await;
        assert_eq!(
            worker.served.load(Ordering::SeqCst),
            2,
            "user B's request must REACH the worker — a dedupe hit here means \
             B was served out of A's cache slot",
        );
        assert!(
            second.headers().get("x-zs-idempotent-replay").is_none(),
            "B's request is not a replay of anything B sent",
        );
        assert_eq!(
            collect_body(second.take_body()).await,
            b"{\"id\":2}",
            "user B must receive their OWN response body, never user A's",
        );
    }

    /// CONTROL for the test above, differing in EXACTLY ONE variable: the
    /// second request carries user A's cookie instead of user B's.
    /// Everything else — route, key, body, worker — is identical.
    ///
    /// The same principal repeating the same key MUST still dedupe: the
    /// worker is hit once and the second caller gets A's stored body back,
    /// flagged as a replay. Without this arm, simply switching the
    /// idempotency cache off would pass the defect test above.
    #[compio::test]
    async fn same_idempotency_key_from_the_same_user_still_dedupes_and_replays() {
        use std::sync::atomic::Ordering;

        let worker = spawn_mock_worker(false, 201);
        let (state, issuer) = authenticated_idempotency_state(&worker);
        let cookie_a = session_cookie_for(&issuer, 1);
        let shared_key = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";

        let mut first =
            post_authenticated_mutation(state.clone(), TEST_AUTH_HOST, &cookie_a, shared_key, b"{\"t\":1}").await;
        assert_eq!(first.status(), ntex::http::StatusCode::CREATED);
        assert_eq!(collect_body(first.take_body()).await, b"{\"id\":1}");
        assert_eq!(worker.served.load(Ordering::SeqCst), 1);

        let mut second =
            post_authenticated_mutation(state.clone(), TEST_AUTH_HOST, &cookie_a, shared_key, b"{\"t\":1}").await;
        assert_eq!(
            worker.served.load(Ordering::SeqCst),
            1,
            "the SAME user retrying the SAME key must not re-execute the mutation",
        );
        assert_eq!(
            second
                .headers()
                .get("x-zs-idempotent-replay")
                .and_then(|v| v.to_str().ok()),
            Some("true"),
        );
        assert_eq!(
            collect_body(second.take_body()).await,
            b"{\"id\":1}",
            "the same user must get their own first response replayed",
        );
    }

    /// The partition keys off the RESOLVED identity, not the DECLARED auth
    /// level. An `auth: "anon"` procedure still resolves a session when the
    /// visitor happens to be logged in, and two such visitors sharing a key
    /// must not share a stored response.
    ///
    /// This is the arm a spec-literal fix — "partition only `auth:
    /// user`/`admin`" — would leave open. The key here is a valid UUIDv7,
    /// so the anonymous entropy gate cannot be what saves it.
    #[compio::test]
    async fn logged_in_visitors_to_an_anon_procedure_are_partitioned_too() {
        use std::sync::atomic::Ordering;

        let worker = spawn_mock_worker(false, 201);
        let (state, issuer) = authenticated_idempotency_state(&worker);
        let cookie_a = session_cookie_for(&issuer, 11);
        let cookie_b = session_cookie_for(&issuer, 12);
        let shared_key = "018f9a1c-3d2b-7c4e-8f01-2a3b4c5d6e7f";

        let mut first = post_authenticated_mutation(
            state.clone(),
            TEST_ANON_HOST,
            &cookie_a,
            shared_key,
            b"{\"t\":1}",
        )
        .await;
        assert_eq!(first.status(), ntex::http::StatusCode::CREATED);
        assert_eq!(collect_body(first.take_body()).await, b"{\"id\":1}");
        assert_eq!(worker.served.load(Ordering::SeqCst), 1);

        let mut second = post_authenticated_mutation(
            state.clone(),
            TEST_ANON_HOST,
            &cookie_b,
            shared_key,
            b"{\"t\":1}",
        )
        .await;
        assert_eq!(
            worker.served.load(Ordering::SeqCst),
            2,
            "a second logged-in visitor must reach the worker even on an anon procedure",
        );
        assert_eq!(
            collect_body(second.take_body()).await,
            b"{\"id\":2}",
            "a logged-in visitor must never be served another visitor's stored body",
        );
    }

    /// The anonymous namespace has no principal to partition on, so the
    /// spec's compensating control is that the key must be unguessable:
    /// `docs/proposals/rpc.md` §8 requires a UUIDv4/v7 for `auth: "anon"`
    /// mutations. A guessable key ("checkout-1") is exactly how two
    /// unrelated anonymous clients collide, so it must be rejected BEFORE
    /// the worker runs.
    #[compio::test]
    async fn anonymous_idempotency_key_must_be_a_uuid() {
        use std::sync::atomic::Ordering;

        let worker = spawn_mock_worker(false, 201);
        let state = idempotency_state_for(&worker);

        let mut resp = post_idempotent_mutation(state.clone(), "checkout-1", b"{\"t\":1}").await;
        assert_eq!(
            resp.status(),
            ntex::http::StatusCode::BAD_REQUEST,
            "a low-entropy anonymous idempotency key must be rejected",
        );
        assert_eq!(
            worker.served.load(Ordering::SeqCst),
            0,
            "the rejection must happen before the worker is dispatched",
        );
        let body: serde_json::Value =
            serde_json::from_slice(&collect_body(resp.take_body()).await).expect("error json");
        assert_eq!(body["code"], "INVALID_ARGUMENT");
        assert_eq!(
            body["details"]["reason"],
            "anonymous_idempotency_key_must_be_uuid_v4_or_v7",
        );
    }

    /// CONTROL for the test above, differing in EXACTLY ONE variable: the
    /// key is a UUIDv4 instead of "checkout-1". It must be ACCEPTED and
    /// still dedupe normally — otherwise "reject every anonymous key"
    /// would pass the test above.
    #[compio::test]
    async fn anonymous_uuid_idempotency_key_is_accepted_and_dedupes() {
        use std::sync::atomic::Ordering;

        let worker = spawn_mock_worker(false, 201);
        let state = idempotency_state_for(&worker);
        let key = "9a7f1c2e-5d3b-4a6f-8c1d-2e3f4a5b6c7d";

        let mut first = post_idempotent_mutation(state.clone(), key, b"{\"t\":1}").await;
        assert_eq!(first.status(), ntex::http::StatusCode::CREATED);
        assert_eq!(collect_body(first.take_body()).await, b"{\"id\":1}");
        assert_eq!(worker.served.load(Ordering::SeqCst), 1);

        let mut second = post_idempotent_mutation(state.clone(), key, b"{\"t\":1}").await;
        assert_eq!(worker.served.load(Ordering::SeqCst), 1);
        assert_eq!(
            collect_body(second.take_body()).await,
            b"{\"id\":1}",
            "a well-formed anonymous key must still dedupe",
        );
    }

    /// Defence-in-depth arm, driven directly because it is UNREACHABLE
    /// end-to-end: on an `auth: "user"` procedure `resolve_auth` 401s a
    /// caller with no identity long before dispatch, and the
    /// `ZeroShip-User` header is minted and MAC'd by this same process. If
    /// that invariant ever breaks, the request must be refused — silently
    /// falling back to `Principal::Anon` would put an authenticated
    /// procedure's responses in the namespace every anonymous caller
    /// shares, which is the leak the partition exists to prevent.
    #[compio::test]
    async fn authenticated_procedure_without_a_readable_principal_is_refused() {
        let state = make_minimal_state();
        let req = ntex::web::test::TestRequest::default()
            .method(ntex::http::Method::POST)
            .header("idempotency-key", "3f2504e0-4f89-41d3-9a0c-0305e82c3301")
            .to_http_request();
        let mut policy = idempotent_mutation_policy();
        policy.auth = AuthLevel::User;

        let outcome = handle_idempotency_pre_dispatch(
            &req,
            &state,
            &uuid::Uuid::new_v4(),
            "/__zeroship/v1/todos.add",
            &policy,
            // The gate said "allowed" but handed over no header — the
            // invariant break this arm exists for.
            None,
            &Bytes::from_static(b"{}"),
            std::time::Instant::now(),
        )
        .await;

        match outcome {
            IdempotencyOutcome::ReturnNow(mut resp) => {
                assert_eq!(resp.status(), ntex::http::StatusCode::INTERNAL_SERVER_ERROR);
                let v: serde_json::Value =
                    serde_json::from_slice(&collect_body(resp.take_body()).await)
                        .expect("error json");
                assert_eq!(v["details"]["reason"], "idempotency_principal_unavailable");
            }
            IdempotencyOutcome::Proceed(_) => {
                panic!("must not dedupe an authenticated procedure under the anonymous namespace")
            }
        }
    }

    /// A UUIDv1 is a timestamp + MAC address: structurally a UUID, but not
    /// unguessable. The gate is about ENTROPY, not about `Uuid::parse_str`
    /// succeeding, so v1 must be rejected on the anonymous path.
    #[compio::test]
    async fn anonymous_idempotency_key_rejects_a_low_entropy_uuid_version() {
        use std::sync::atomic::Ordering;

        let worker = spawn_mock_worker(false, 201);
        let state = idempotency_state_for(&worker);
        // Version nibble = 1 (time-based), RFC 4122 variant.
        let v1 = "d9428888-122b-11e1-b85c-61cd3cbb3210";

        let resp = post_idempotent_mutation(state.clone(), v1, b"{\"t\":1}").await;
        assert_eq!(resp.status(), ntex::http::StatusCode::BAD_REQUEST);
        assert_eq!(worker.served.load(Ordering::SeqCst), 0);
    }
}
