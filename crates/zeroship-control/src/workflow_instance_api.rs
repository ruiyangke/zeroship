//! Internal durable-workflow instance API.
//!
//! These routes are called by the runtime-side `env.workflows` binding and by
//! creator tooling that already holds the internal control credential. The app
//! scope is not request-body data: every handler derives it from the
//! authenticated channel header and binds `app_id` in every journal query.

use std::collections::BTreeSet;
use std::sync::Arc;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, TimeZone, Utc};
use compio_postgres::error::SqlState;
use ntex::http::StatusCode;
use ntex::web;
use ntex::web::types::{Json, Path, Query, State};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroship_authn::rate_limit::{self, Quota, RateLimitDecision};
use zeroship_core::{crypto, typed_id};
use zeroship_plugin_workflow::engine::{cap_exceeded, WORKFLOW_STATE_CAP_ERROR_CODE};
use zeroship_plugin_workflow::errors::WorkflowError;
use zeroship_plugin_workflow::store::pg::{self, WorkflowTables};

use crate::api::infrastructure_error_response;
use crate::cron::workflow_engine;
use crate::registry::RegistryError;
use crate::{workflow_limits, AppState};

pub const APP_ID_HEADER: &str = "x-zeroship-app-id";
pub const ALT_APP_ID_HEADER: &str = "zeroship-app-id";
pub const SIGNAL_REQUEST_BODY_BYTES: usize = 128 * 1024;
pub const SIGNAL_PAYLOAD_BYTES: usize = 64 * 1024;

const SIGNAL_RATE_LIMIT_CAPACITY: f64 = 60.0;
// Full-bucket refill window: the bucket refills from empty to
// `SIGNAL_RATE_LIMIT_CAPACITY` over this many seconds. Previously this was
// the literal `60.0 / 60.0`, i.e. capacity divided by a duplicated literal
// `60.0` rather than by this named window — correct today only because the
// window and the capacity both happen to be 60; changing either constant
// independently would have silently decoupled the refill rate from the
// capacity it's meant to track.
const SIGNAL_RATE_LIMIT_REFILL_WINDOW_SECS: f64 = 60.0;
const SIGNAL_RATE_LIMIT_REFILL_PER_SEC: f64 =
    SIGNAL_RATE_LIMIT_CAPACITY / SIGNAL_RATE_LIMIT_REFILL_WINDOW_SECS;
const SIGNAL_TOKEN_MAX_TYPES: usize = 16;
const SIGNAL_TOKEN_MAX_TTL_SECS: i64 = 24 * 60 * 60;
const SIGNAL_TOKEN_TIMESTAMP_TOLERANCE_SECS: i64 = 1;
const SIGNAL_TOPIC_BYTES: usize = 512;
const SIGNAL_KEY_VERIFIER: &str = "bearer-signing";
const SIGNAL_KEY_KEK: &str = "master:v1";
const SIGNAL_TOKEN_REPLAY_PREFIX: &str = "wst-sha256:";

#[derive(Debug, Deserialize)]
pub struct CreateRunBody {
    pub input: Value,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default, alias = "onConflict")]
    pub on_conflict: Option<OnConflictBody>,
}

#[derive(Debug, Deserialize)]
pub struct StartManyBody {
    pub items: Vec<StartManyItem>,
    #[serde(default, alias = "onConflict")]
    pub on_conflict: Option<OnConflictBody>,
}

#[derive(Debug, Deserialize)]
pub struct StartManyItem {
    pub input: Value,
    #[serde(default)]
    pub key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct StepOutputPath {
    run_id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
pub struct StepOutputQuery {
    #[serde(default)]
    occurrence: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum OnConflictBody {
    Policy(String),
    Object { policy: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConflictPolicy {
    Join,
    Reject,
    Replace,
}

impl OnConflictBody {
    fn policy(&self) -> Result<ConflictPolicy, String> {
        let raw = match self {
            Self::Policy(policy) => policy.as_str(),
            Self::Object { policy } => policy.as_str(),
        };
        match raw {
            "join" => Ok(ConflictPolicy::Join),
            "reject" => Ok(ConflictPolicy::Reject),
            "replace" => Ok(ConflictPolicy::Replace),
            other => Err(format!(
                "invalid onConflict policy '{other}' (expected join, reject, or replace)"
            )),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SignalBody {
    #[serde(rename = "type")]
    pub signal_type: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Deserialize)]
pub struct CreateSignalTokenBody {
    pub types: Vec<String>,
    pub ttl: String,
}

#[derive(Debug, Deserialize)]
pub struct IngressSignalBody {
    pub token: String,
    #[serde(default, rename = "type")]
    pub signal_type: Option<String>,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Deserialize)]
pub struct PublishTopicBody {
    #[serde(rename = "type")]
    pub signal_type: String,
    #[serde(default)]
    pub payload: Value,
    #[serde(default, alias = "idempotencyKey")]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RestartBody {
    #[serde(default)]
    pub from: Option<RestartTargetBody>,
    #[serde(default)]
    pub deploy: Option<RestartDeployBody>,
}

#[derive(Debug, Default, Deserialize)]
pub struct CancelBody {
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RestartTargetBody {
    pub name: String,
    #[serde(default)]
    pub occurrence: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum RestartDeployBody {
    Pin(String),
    Object { pin: String },
}

impl RestartDeployBody {
    fn pin(&self) -> &str {
        match self {
            Self::Pin(pin) => pin.as_str(),
            Self::Object { pin } => pin.as_str(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ListRunsQuery {
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default, alias = "workflowName")]
    pub workflow_name: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RunListItem {
    id: String,
    workflow_name: String,
    state: String,
    created_at: String,
}

#[derive(Debug, Clone)]
struct ActiveDeploy {
    id: String,
    manifest_json: String,
}

#[derive(Debug)]
enum WorkflowApiError {
    BadRequest(String),
    Unauthorized(String),
    Forbidden(String),
    NotFound(String),
    Conflict(String),
    LimitExceeded(String),
    Restart(String),
    JournalCapExceeded(String),
    PayloadTooLarge(String),
    RateLimited { retry_after_secs: f64 },
    RateLimitUnavailable(String),
    Unavailable(String),
    Database(String),
}

impl WorkflowApiError {
    fn response(self) -> web::HttpResponse {
        match self {
            Self::BadRequest(msg) => {
                web::HttpResponse::BadRequest().json(&json!({ "error": msg }))
            }
            Self::Unauthorized(msg) => {
                web::HttpResponse::Unauthorized().json(&json!({ "error": msg }))
            }
            Self::Forbidden(msg) => {
                web::HttpResponse::Forbidden().json(&json!({ "error": msg }))
            }
            Self::NotFound(msg) => web::HttpResponse::NotFound().json(&json!({ "error": msg })),
            Self::Conflict(msg) => web::HttpResponse::Conflict().json(&json!({
                "error": "RunConflict",
                "message": msg,
            })),
            Self::LimitExceeded(msg) => web::HttpResponse::TooManyRequests().json(&json!({
                "error": "LimitExceededError",
                "message": msg,
            })),
            Self::Restart(msg) => web::HttpResponse::Conflict().json(&json!({
                "error": "RestartError",
                "message": msg,
            })),
            Self::JournalCapExceeded(msg) => web::HttpResponse::TooManyRequests().json(&json!({
                "error": WORKFLOW_STATE_CAP_ERROR_CODE,
                "message": msg,
            })),
            Self::PayloadTooLarge(msg) => {
                web::HttpResponse::build(StatusCode::PAYLOAD_TOO_LARGE)
                    .json(&json!({ "error": msg }))
            }
            Self::RateLimited { retry_after_secs } => web::HttpResponse::TooManyRequests()
                .header("retry-after", retry_after_header(retry_after_secs))
                .json(&json!({ "error": "rate limited" })),
            Self::RateLimitUnavailable(msg) => {
                tracing::error!(error = %msg, "workflow instance API: rate limiter unavailable");
                web::HttpResponse::ServiceUnavailable()
                    .header("retry-after", "1")
                    .json(&json!({ "error": "rate limit unavailable" }))
            }
            Self::Unavailable(msg) => web::HttpResponse::ServiceUnavailable()
                .header("retry-after", "30")
                .json(&json!({ "error": msg })),
            Self::Database(msg) => infrastructure_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "workflow instance API",
                msg,
            ),
        }
    }
}

impl From<RegistryError> for WorkflowApiError {
    fn from(value: RegistryError) -> Self {
        match value {
            RegistryError::InvalidInput(msg) => Self::BadRequest(msg),
            RegistryError::NotFound(msg) => Self::NotFound(msg),
            RegistryError::AlreadyExists(msg)
            | RegistryError::Conflict(msg)
            | RegistryError::ReservedName(msg) => Self::Conflict(msg),
            RegistryError::Database(msg) => Self::Database(msg),
            RegistryError::FxUnresolved => Self::Database(value.to_string()),
            // Not reachable from here - no workflow surface commits a deploy -
            // but the conversion has to be total, and the schema precondition is
            // a conflict wherever it surfaces, never a 500.
            RegistryError::SchemaNotApplied { .. } => Self::Conflict(value.to_string()),
        }
    }
}

impl From<WorkflowError> for WorkflowApiError {
    fn from(value: WorkflowError) -> Self {
        match value {
            WorkflowError::Invalid(msg) => Self::BadRequest(msg),
            WorkflowError::CompensableCarry(msg) => Self::BadRequest(format!("CompensableCarryError: {msg}")),
            WorkflowError::Deadlock(msg) => Self::Database(format!("retryable deadlock: {msg}")),
            WorkflowError::Db(msg) => Self::Database(msg),
        }
    }
}

fn workflow_pg_error(error: compio_postgres::Error) -> WorkflowApiError {
    workflow_pg_error_from_parts(error.code(), error.to_string())
}

fn workflow_pg_error_from_parts(code: Option<&SqlState>, message: String) -> WorkflowApiError {
    if code == Some(&SqlState::T_R_DEADLOCK_DETECTED) {
        WorkflowApiError::Database(format!("retryable deadlock: {message}"))
    } else {
        WorkflowApiError::Database(message)
    }
}

fn check_app_scoped_auth(
    req: &web::HttpRequest,
    state: &AppState,
    app_id: &Uuid,
) -> Result<(), web::HttpResponse> {
    let header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let token = zeroship_core::auth::extract_bearer(header);
    match token {
        Some(key)
            if !state.control_key.is_empty()
                && zeroship_core::auth::validate_app_scoped_control_token(
                    key,
                    state.control_key.expose_secret(),
                    &app_id.to_string(),
                ) => Ok(()),
        _ => {
            tracing::warn!(
                method = %req.method(),
                path = %req.path(),
                "workflow instance API: auth rejected"
            );
            Err(web::HttpResponse::Unauthorized().json(&json!({ "error": "unauthorized" })))
        }
    }
}

fn check_control_auth(
    req: &web::HttpRequest,
    state: &AppState,
) -> Result<(), web::HttpResponse> {
    let header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let token = zeroship_core::auth::extract_bearer(header);
    match token {
        Some(key)
            if !state.control_key.is_empty()
                && zeroship_core::auth::constant_time_eq(
                    key,
                    state.control_key.expose_secret(),
                ) =>
        {
            Ok(())
        }
        _ => {
            tracing::warn!(
                method = %req.method(),
                path = %req.path(),
                "workflow ingress API: auth rejected"
            );
            Err(web::HttpResponse::Unauthorized().json(&json!({ "error": "unauthorized" })))
        }
    }
}

fn app_id_from_channel(
    req: &web::HttpRequest,
    state: &AppState,
) -> Result<Uuid, web::HttpResponse> {
    let raw = [APP_ID_HEADER, ALT_APP_ID_HEADER, "x-app-id", "x-app"]
        .into_iter()
        .find_map(|name| req.headers().get(name).and_then(|v| v.to_str().ok()))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            web::HttpResponse::BadRequest().json(&json!({
                "error": format!("missing {APP_ID_HEADER} header")
            }))
        })?;

    let app_id = parse_app_id(raw)
        .map_err(|msg| web::HttpResponse::BadRequest().json(&json!({ "error": msg })))?;
    check_app_scoped_auth(req, state, &app_id)?;
    Ok(app_id)
}

fn parse_app_id(raw: &str) -> Result<Uuid, String> {
    Uuid::parse_str(raw).or_else(|_| {
        typed_id::parse_with_prefix(raw, typed_id::APP_PREFIX)
            .map_err(|e| format!("invalid app id: {e}"))
    })
}

fn validate_workflow_name(name: &str) -> Result<(), WorkflowApiError> {
    if name.is_empty() || name.len() > 128 {
        return Err(WorkflowApiError::BadRequest(
            "workflow name must be 1-128 bytes".to_string(),
        ));
    }
    if name.starts_with("__zs.") {
        return Err(WorkflowApiError::BadRequest(
            "workflow name uses a reserved prefix".to_string(),
        ));
    }
    Ok(())
}

fn normalize_key(key: Option<String>) -> Result<Option<String>, WorkflowApiError> {
    let Some(key) = key else {
        return Ok(None);
    };
    if key.is_empty() {
        return Err(WorkflowApiError::BadRequest(
            "workflow key must not be empty".to_string(),
        ));
    }
    if key.len() > 1024 {
        return Err(WorkflowApiError::BadRequest(
            "workflow key must be at most 1024 bytes".to_string(),
        ));
    }
    Ok(Some(key))
}

fn validate_signal_type(signal_type: &str) -> Result<(), WorkflowApiError> {
    if signal_type.is_empty() || signal_type.len() > 256 {
        return Err(WorkflowApiError::BadRequest(
            "signal type must be 1-256 bytes".to_string(),
        ));
    }
    Ok(())
}

fn validate_ingress_signal_type(signal_type: &str) -> Result<(), WorkflowApiError> {
    validate_signal_type(signal_type)?;
    if signal_type.starts_with("__zs.") {
        return Err(WorkflowApiError::Forbidden(
            "signal type uses a reserved prefix".to_string(),
        ));
    }
    Ok(())
}

fn validate_topic(topic: &str) -> Result<(), WorkflowApiError> {
    if topic.is_empty() || topic.len() > SIGNAL_TOPIC_BYTES {
        return Err(WorkflowApiError::BadRequest(format!(
            "signal topic must be 1-{SIGNAL_TOPIC_BYTES} bytes"
        )));
    }
    if topic.starts_with("__zs.") {
        return Err(WorkflowApiError::Forbidden(
            "signal topic uses a reserved prefix".to_string(),
        ));
    }
    Ok(())
}

fn validate_token_types(types: &[String]) -> Result<Vec<String>, WorkflowApiError> {
    if types.is_empty() {
        return Err(WorkflowApiError::BadRequest(
            "signal token must authorize at least one type".to_string(),
        ));
    }
    if types.len() > SIGNAL_TOKEN_MAX_TYPES {
        return Err(WorkflowApiError::BadRequest(format!(
            "signal token authorizes too many types (max {SIGNAL_TOKEN_MAX_TYPES})"
        )));
    }
    let mut deduped = BTreeSet::new();
    for signal_type in types {
        validate_ingress_signal_type(signal_type)?;
        deduped.insert(signal_type.clone());
    }
    Ok(deduped.into_iter().collect())
}

fn parse_duration_secs(raw: &str) -> Result<i64, WorkflowApiError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(WorkflowApiError::BadRequest(
            "duration must not be empty".to_string(),
        ));
    }
    if let Some(ms) = parse_iso_duration_ms(trimmed).or_else(|| parse_suffix_duration_ms(trimmed)) {
        let secs = (ms + 999) / 1000;
        if secs <= 0 {
            return Err(WorkflowApiError::BadRequest(
                "duration must be greater than zero".to_string(),
            ));
        }
        return Ok(secs);
    }
    Err(WorkflowApiError::BadRequest(format!(
        "invalid duration {trimmed:?}"
    )))
}

fn parse_iso_duration_ms(raw: &str) -> Option<i64> {
    let rest = raw.strip_prefix('P')?;
    let (date, time) = rest.split_once('T').unwrap_or((rest, ""));
    let mut total_ms = 0f64;
    if let Some(days) = date.strip_suffix('D') {
        if days.is_empty() {
            return None;
        }
        total_ms += days.parse::<f64>().ok()? * 86_400_000.0;
    } else if !date.is_empty() {
        return None;
    }
    let mut number = String::new();
    for ch in time.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            number.push(ch);
            continue;
        }
        if number.is_empty() {
            return None;
        }
        let value = number.parse::<f64>().ok()?;
        number.clear();
        match ch {
            'H' => total_ms += value * 3_600_000.0,
            'M' => total_ms += value * 60_000.0,
            'S' => total_ms += value * 1_000.0,
            _ => return None,
        }
    }
    if !number.is_empty() || total_ms <= 0.0 || !total_ms.is_finite() {
        return None;
    }
    Some(total_ms.ceil() as i64)
}

fn parse_suffix_duration_ms(raw: &str) -> Option<i64> {
    let units = [
        ("ms", 1.0),
        ("s", 1_000.0),
        ("m", 60_000.0),
        ("h", 3_600_000.0),
        ("d", 86_400_000.0),
    ];
    for (suffix, multiplier) in units {
        let Some(number) = raw.strip_suffix(suffix) else {
            continue;
        };
        if number.is_empty() {
            return None;
        }
        let value = number.parse::<f64>().ok()?;
        let ms = value * multiplier;
        if ms <= 0.0 || !ms.is_finite() {
            return None;
        }
        return Some(ms.ceil() as i64);
    }
    raw.parse::<i64>().ok().filter(|v| *v > 0).map(|secs| secs * 1000)
}

fn validate_token_ttl(raw: &str) -> Result<i64, WorkflowApiError> {
    let secs = parse_duration_secs(raw)?;
    if secs > SIGNAL_TOKEN_MAX_TTL_SECS {
        return Err(WorkflowApiError::BadRequest(format!(
            "signal token ttl exceeds {SIGNAL_TOKEN_MAX_TTL_SECS} seconds"
        )));
    }
    Ok(secs)
}

fn validate_run_id(run_id: &str) -> Result<(), WorkflowApiError> {
    typed_id::parse_with_prefix(run_id, typed_id::WORKFLOW_RUN_PREFIX)
        .map(|_| ())
        .map_err(|e| WorkflowApiError::BadRequest(format!("invalid run id: {e}")))
}

fn signal_payload_size(payload: &Value) -> Result<usize, WorkflowApiError> {
    let bytes = serde_json::to_vec(payload)
        .map_err(|e| WorkflowApiError::BadRequest(format!("signal payload is not JSON: {e}")))?;
    if bytes.len() > SIGNAL_PAYLOAD_BYTES {
        return Err(WorkflowApiError::PayloadTooLarge(format!(
            "signal payload exceeds {SIGNAL_PAYLOAD_BYTES} bytes"
        )));
    }
    Ok(bytes.len())
}

fn waiting_key_matches_signal(waiting_step_key: Option<&str>, signal_type: &str) -> bool {
    let Some(key) = waiting_step_key else {
        return false;
    };
    let parts: Vec<&str> = key.split(':').collect();
    matches!(parts.as_slice(), ["wait", _, _, ty] | ["wait", _, _, ty, _] if *ty == signal_type)
}

fn restored_state_expr() -> &'static str {
    "COALESCE(paused_from_status, CASE \
        WHEN waiting_step_key LIKE 'wait:%' THEN 'waiting' \
        WHEN waiting_step_key LIKE 'child:%' THEN 'waiting' \
        WHEN waiting_step_key LIKE 'sleep:%' THEN 'sleeping' \
        WHEN wake_at IS NULL OR wake_at <= now() THEN 'queued' \
        ELSE 'sleeping' \
     END)"
}

fn resume_wake_frontier_sql(tables: &WorkflowTables) -> String {
    format!(
        "(SELECT MIN(wake_at) \
            FROM ( \
                  SELECT MIN(wake_at) AS wake_at \
                    FROM {steps} \
                   WHERE run_id = r.id \
                     AND state = 'running' \
                     AND wake_at IS NOT NULL \
                  HAVING MIN(wake_at) IS NOT NULL \
                  UNION ALL \
                  SELECT now() AS wake_at \
                   WHERE EXISTS ( \
                         SELECT 1 \
                           FROM {signals} sig \
                          WHERE sig.run_id = r.id \
                            AND sig.consumed_by IS NULL \
                            AND ( \
                                (r.waiting_step_key LIKE 'wait:%' \
                                 AND split_part(r.waiting_step_key, ':', 4) = sig.type) \
                                OR EXISTS ( \
                                    SELECT 1 \
                                      FROM {steps} child \
                                     WHERE child.run_id = r.id \
                                       AND child.kind = 'child' \
                                       AND child.state = 'running' \
                                       AND child.signal_type = sig.type \
                                ) \
                            ) \
                      ) \
                 ) frontier)",
        steps = tables.steps,
        signals = tables.signals,
    )
}

fn status_output(row: &compio_postgres::Row) -> Value {
    let output_kind: String = row.get("output_kind");
    if output_kind == "blob" {
        let hash: Option<String> = row.get("output_hash");
        let size: Option<i64> = row.get("output_size");
        let content_type: Option<String> = row.get("output_content_type");
        if let (Some(hash), Some(size)) = (hash, size) {
            return json!({
                "kind": "ref",
                "ref": format!("wfblob:sha256:{hash}"),
                "hash": hash,
                "size": size,
                "contentType": content_type.unwrap_or_else(|| "application/octet-stream".to_string()),
            });
        }
    }
    row.get::<_, Option<Value>>("output").unwrap_or(Value::Null)
}

fn retry_after_header(secs: f64) -> String {
    if secs.is_finite() {
        format!("{:.0}", secs.ceil().clamp(1.0, 3600.0))
    } else {
        "60".to_string()
    }
}

async fn active_deploy_for_workflow<C>(
    conn: &C,
    app_id: &Uuid,
    workflow_name: &str,
) -> Result<ActiveDeploy, WorkflowApiError>
where
    C: compio_postgres::GenericClient + Sync,
{
    ensure_app_workflows_enabled(conn, app_id).await?;
    let rows = conn
        .query(
            "SELECT d.id, d.manifest_json \
               FROM zeroship.app_deploys d \
               JOIN zeroship.apps app ON app.id = d.app_id \
              WHERE d.app_id = $1 \
                AND app.archived_at IS NULL \
                AND d.activated_at IS NOT NULL \
              ORDER BY d.activated_at DESC, d.created_at DESC, d.id DESC \
              LIMIT 1",
            &[app_id],
        )
        .await
        .map_err(workflow_pg_error)?;
    let Some(row) = rows.first() else {
        return Err(WorkflowApiError::BadRequest(
            "app has no active deploy".to_string(),
        ));
    };
    let deploy = ActiveDeploy {
        id: row.get("id"),
        manifest_json: row.get("manifest_json"),
    };
    match manifest_declares_workflow(&deploy.manifest_json, workflow_name) {
        Ok(true) => Ok(deploy),
        Ok(false) => Err(WorkflowApiError::BadRequest(format!(
            "workflow '{workflow_name}' is not declared by the active deploy"
        ))),
        Err(e) => Err(WorkflowApiError::BadRequest(format!(
            "active deploy manifest is invalid: {e}"
        ))),
    }
}

async fn active_deploy_id_for_app<C>(conn: &C, app_id: &Uuid) -> Result<String, WorkflowApiError>
where
    C: compio_postgres::GenericClient + Sync,
{
    ensure_app_workflows_enabled(conn, app_id).await?;
    let rows = conn
        .query(
            "SELECT d.id \
               FROM zeroship.app_deploys d \
               JOIN zeroship.apps app ON app.id = d.app_id \
              WHERE d.app_id = $1 \
                AND app.archived_at IS NULL \
                AND d.activated_at IS NOT NULL \
              ORDER BY d.activated_at DESC, d.created_at DESC, d.id DESC \
              LIMIT 1",
            &[app_id],
        )
        .await
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    rows.first()
        .map(|row| row.get("id"))
        .ok_or_else(|| WorkflowApiError::BadRequest("app has no active deploy".to_string()))
}

fn signal_key_aad(app_id: &Uuid, kid: &str) -> Vec<u8> {
    format!("workflow-signal-key:{app_id}:{kid}").into_bytes()
}

fn signal_token_replay_key(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    format!("{SIGNAL_TOKEN_REPLAY_PREFIX}{}", hex::encode(digest))
}

fn master_crypto_key(state: &AppState) -> [u8; 32] {
    crypto::derive_key(state.master_key.expose_secret())
}

fn encrypt_signal_secret(state: &AppState, app_id: &Uuid, kid: &str, secret: &[u8]) -> Result<Vec<u8>, WorkflowApiError> {
    let key = master_crypto_key(state);
    crypto::encrypt(&key, &signal_key_aad(app_id, kid), secret)
        .map_err(|e| WorkflowApiError::Database(format!("encrypt workflow signal key: {e}")))
}

fn decrypt_signal_secret(
    state: &AppState,
    app_id: &Uuid,
    kid: &str,
    secret_ct: &[u8],
) -> Result<Vec<u8>, WorkflowApiError> {
    let key = master_crypto_key(state);
    crypto::decrypt(&key, &signal_key_aad(app_id, kid), secret_ct)
        .map_err(|e| WorkflowApiError::Database(format!("decrypt workflow signal key: {e}")))
}

async fn active_or_create_signal_secret<C>(
    conn: &C,
    state: &AppState,
    app_id: &Uuid,
) -> Result<Vec<u8>, WorkflowApiError>
where
    C: compio_postgres::GenericClient + Sync,
{
    let rows = conn
        .query(
            "SELECT kid, secret_ct \
               FROM zeroship.workflow_signal_keys \
              WHERE app_id = $1 \
                AND verifier = $2 \
                AND status IN ('active', 'next', 'retiring') \
              ORDER BY CASE status WHEN 'active' THEN 0 WHEN 'next' THEN 1 ELSE 2 END, \
                       created_at DESC, id DESC \
              LIMIT 1",
            &[app_id, &SIGNAL_KEY_VERIFIER],
        )
        .await
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    if let Some(row) = rows.first() {
        let kid: String = row.get("kid");
        let secret_ct: Vec<u8> = row.get("secret_ct");
        return decrypt_signal_secret(state, app_id, &kid, &secret_ct);
    }

    let id = typed_id::new_workflow_signal_key_id();
    let kid = id.clone();
    let mut secret = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut secret);
    let secret_ct = encrypt_signal_secret(state, app_id, &kid, &secret)?;
    conn.execute(
        "INSERT INTO zeroship.workflow_signal_keys \
            (id, app_id, kid, verifier, secret_ct, secret_kek, status) \
         VALUES ($1, $2, $3, $4, $5, $6, 'active')",
        &[
            &id,
            app_id,
            &kid,
            &SIGNAL_KEY_VERIFIER,
            &secret_ct,
            &SIGNAL_KEY_KEK,
        ],
    )
    .await
    .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    Ok(secret.to_vec())
}

async fn load_signal_verification_secrets(
    state: &AppState,
    app_id: &Uuid,
) -> Result<Vec<Vec<u8>>, WorkflowApiError> {
    let mut conn = state.registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    let rows = tx
        .query(
            "SELECT kid, secret_ct \
               FROM zeroship.workflow_signal_keys \
              WHERE app_id = $1 \
                AND verifier = $2 \
                AND status IN ('active', 'next', 'retiring') \
              ORDER BY CASE status WHEN 'active' THEN 0 WHEN 'next' THEN 1 ELSE 2 END, \
                       created_at DESC, id DESC",
            &[app_id, &SIGNAL_KEY_VERIFIER],
        )
        .await
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    tx.commit()
        .await
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    let mut secrets = Vec::with_capacity(rows.len());
    for row in rows {
        let kid: String = row.get("kid");
        let secret_ct: Vec<u8> = row.get("secret_ct");
        secrets.push(decrypt_signal_secret(state, app_id, &kid, &secret_ct)?);
    }
    Ok(secrets)
}

fn unverified_signal_token_app_id(token: &str) -> Result<Uuid, WorkflowApiError> {
    let body = token
        .strip_prefix(typed_id::WORKFLOW_SIGNAL_TOKEN_PREFIX)
        .and_then(|s| s.strip_prefix('_'))
        .ok_or_else(|| WorkflowApiError::Unauthorized("invalid signal token".to_string()))?;
    let (payload_b64, _) = body
        .split_once('.')
        .ok_or_else(|| WorkflowApiError::Unauthorized("invalid signal token".to_string()))?;
    let payload = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| WorkflowApiError::Unauthorized("invalid signal token".to_string()))?;
    let value: Value = serde_json::from_slice(&payload)
        .map_err(|_| WorkflowApiError::Unauthorized("invalid signal token".to_string()))?;
    let app_id = value
        .get("app_id")
        .and_then(Value::as_str)
        .ok_or_else(|| WorkflowApiError::Unauthorized("invalid signal token".to_string()))?;
    parse_app_id(app_id).map_err(|_| WorkflowApiError::Unauthorized("invalid signal token".to_string()))
}

async fn verify_signal_token(
    state: &AppState,
    token: &str,
) -> Result<typed_id::WorkflowSignalTokenClaims, WorkflowApiError> {
    let app_id = unverified_signal_token_app_id(token)?;
    let secrets = load_signal_verification_secrets(state, &app_id).await?;
    for secret in secrets {
        match typed_id::verify_workflow_signal_token(token, &secret) {
            Ok(claims) => return Ok(claims),
            Err(_) => continue,
        }
    }
    Err(WorkflowApiError::Unauthorized(
        "invalid signal token".to_string(),
    ))
}

fn validate_signal_token_time(
    claims: &typed_id::WorkflowSignalTokenClaims,
    now_unix: i64,
) -> Result<(), WorkflowApiError> {
    if claims.exp.saturating_add(SIGNAL_TOKEN_TIMESTAMP_TOLERANCE_SECS) < now_unix {
        return Err(WorkflowApiError::Unauthorized(
            "signal token expired".to_string(),
        ));
    }
    Ok(())
}

fn claims_expiry(claims: &typed_id::WorkflowSignalTokenClaims) -> Result<DateTime<Utc>, WorkflowApiError> {
    Utc.timestamp_opt(claims.exp, 0)
        .single()
        .ok_or_else(|| WorkflowApiError::Unauthorized("invalid signal token expiry".to_string()))
}

fn resolve_ingress_signal_type(
    claims: &typed_id::WorkflowSignalTokenClaims,
    requested: Option<String>,
) -> Result<String, WorkflowApiError> {
    let signal_type = match requested {
        Some(signal_type) => signal_type,
        None if claims.types.len() == 1 => claims.types[0].clone(),
        None => {
            return Err(WorkflowApiError::BadRequest(
                "signal type is required when a token authorizes multiple types".to_string(),
            ))
        }
    };
    validate_ingress_signal_type(&signal_type)?;
    if !claims.types.iter().any(|allowed| allowed == &signal_type) {
        return Err(WorkflowApiError::Forbidden(
            "signal token does not authorize this type".to_string(),
        ));
    }
    Ok(signal_type)
}

fn manifest_declares_workflow(raw: &str, workflow_name: &str) -> Result<bool, String> {
    let manifest: Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    let Some(workflows) = manifest
        .get("workflows")
        .or_else(|| manifest.get("workflow"))
        .or_else(|| manifest.get("durable_workflows"))
        .or_else(|| manifest.get("durableWorkflows"))
    else {
        return Ok(false);
    };
    Ok(workflow_container_has(workflows, workflow_name))
}

fn workflow_container_has(value: &Value, workflow_name: &str) -> bool {
    match value {
        Value::Array(items) => items.iter().any(|item| match item {
            Value::String(name) => name == workflow_name,
            Value::Object(map) => map
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| name == workflow_name),
            _ => false,
        }),
        Value::Object(map) => map.contains_key(workflow_name),
        _ => false,
    }
}

async fn check_create_journal_capacity<C>(
    conn: &C,
    app_id: &Uuid,
    input_journal_bytes: i64,
) -> Result<(), WorkflowApiError>
where
    C: compio_postgres::GenericClient + Sync,
{
    workflow_limits::lock_app_journal_accounting(conn, app_id)
        .await
        .map_err(WorkflowApiError::from)?;
    let limits = workflow_limits::workflow_journal_limits_for_app(conn, app_id)
        .await
        .map_err(WorkflowApiError::from)?;
    if cap_exceeded(0, input_journal_bytes, limits.run_max_bytes) {
        return Err(WorkflowApiError::JournalCapExceeded(format!(
            "workflow run input exceeds per-run journal cap ({} > {})",
            input_journal_bytes, limits.run_max_bytes
        )));
    }
    check_app_journal_capacity(conn, app_id, input_journal_bytes, limits.app_max_bytes).await
}

async fn ensure_app_workflows_enabled<C>(
    conn: &C,
    app_id: &Uuid,
) -> Result<(), WorkflowApiError>
where
    C: compio_postgres::GenericClient + Sync,
{
    if crate::workflow_rollout::workflows_enabled_for_app(conn, app_id)
        .await
        .map_err(WorkflowApiError::from)?
    {
        return Ok(());
    }
    Err(WorkflowApiError::Forbidden(
        "workflows are not enabled for this app or plan".to_string(),
    ))
}

async fn ensure_public_ingress_enabled(state: &AppState) -> Result<(), WorkflowApiError> {
    let conn = state.registry.conn().await.map_err(WorkflowApiError::from)?;
    if crate::workflow_rollout::ingress_disabled(&conn)
        .await
        .map_err(WorkflowApiError::from)?
    {
        return Err(WorkflowApiError::Unavailable(
            "workflow signal ingress is disabled".to_string(),
        ));
    }
    Ok(())
}

async fn check_signal_journal_capacity<C>(
    conn: &C,
    app_id: &Uuid,
    payload_journal_bytes: i64,
) -> Result<(), WorkflowApiError>
where
    C: compio_postgres::GenericClient + Sync,
{
    workflow_limits::lock_app_journal_accounting(conn, app_id)
        .await
        .map_err(WorkflowApiError::from)?;
    let limits = workflow_limits::workflow_journal_limits_for_app(conn, app_id)
        .await
        .map_err(WorkflowApiError::from)?;
    check_app_journal_capacity(conn, app_id, payload_journal_bytes, limits.app_max_bytes).await
}

async fn provision_workflow_journal<C>(
    conn: &C,
    app_id: &Uuid,
) -> Result<WorkflowTables, WorkflowApiError>
where
    C: compio_postgres::GenericClient + Sync,
{
    workflow_engine::provision_tables(conn, app_id)
        .await
        .map_err(WorkflowApiError::from)
}

async fn check_app_journal_capacity<C>(
    conn: &C,
    app_id: &Uuid,
    delta: i64,
    app_max_bytes: i64,
) -> Result<(), WorkflowApiError>
where
    C: compio_postgres::GenericClient + Sync,
{
    let current = workflow_limits::app_journal_bytes(conn, app_id)
        .await
        .map_err(WorkflowApiError::from)?;
    if cap_exceeded(current, delta, app_max_bytes) {
        return Err(WorkflowApiError::JournalCapExceeded(format!(
            "workflow app journal cap exceeded (current {current} + delta {delta} > {app_max_bytes})"
        )));
    }
    Ok(())
}

/// Grouped inputs shared by [`insert_run`], [`insert_run_on_conflict_do_nothing`],
/// and [`join_or_create_keyed_run`] — kept as a struct rather than individual
/// parameters purely to stay under clippy's `too_many_arguments` threshold;
/// every field is still required and read exactly once.
struct NewRun<'a> {
    app_id: &'a Uuid,
    workflow_name: &'a str,
    deploy_id: &'a str,
    input: &'a Value,
    input_journal_bytes: i64,
    dedup_key: Option<&'a String>,
    started_at: Option<DateTime<Utc>>,
}

async fn insert_run<C>(
    conn: &C,
    tables: &WorkflowTables,
    run: NewRun<'_>,
    run_id: &str,
) -> Result<(), WorkflowApiError>
where
    C: compio_postgres::GenericClient + Sync,
{
    let NewRun {
        app_id,
        workflow_name,
        deploy_id,
        input,
        input_journal_bytes,
        dedup_key,
        started_at,
    } = run;
    let sql = format!(
        "INSERT INTO {runs} \
            (id, workflow_name, app_id, deploy_id, state, input, journal_bytes, dedup_key, wake_at, started_at) \
         VALUES ($1, $2, $3, $4, 'queued', $5, $6, $7, now(), COALESCE($8, now()))",
        runs = tables.runs
    );
    conn.execute(
        &sql,
        &[
            &run_id,
            &workflow_name,
            app_id,
            &deploy_id,
            input,
            &input_journal_bytes,
            &dedup_key,
            &started_at,
        ],
    )
    .await
    .map_err(workflow_pg_error)?;
    Ok(())
}

async fn insert_run_on_conflict_do_nothing<C>(
    conn: &C,
    tables: &WorkflowTables,
    run: NewRun<'_>,
    run_id: &str,
) -> Result<Option<String>, WorkflowApiError>
where
    C: compio_postgres::GenericClient + Sync,
{
    let NewRun {
        app_id,
        workflow_name,
        deploy_id,
        input,
        input_journal_bytes,
        dedup_key,
        started_at,
    } = run;
    let sql = format!(
        "INSERT INTO {runs} \
                (id, workflow_name, app_id, deploy_id, state, input, journal_bytes, dedup_key, wake_at, started_at) \
             VALUES ($1, $2, $3, $4, 'queued', $5, $6, $7, now(), COALESCE($8, now())) \
             ON CONFLICT (app_id, workflow_name, dedup_key) DO NOTHING \
             RETURNING id",
        runs = tables.runs
    );
    let rows = conn
        .query(
            &sql,
            &[
                &run_id,
                &workflow_name,
                app_id,
                &deploy_id,
                input,
                &input_journal_bytes,
                &dedup_key,
                &started_at,
            ],
        )
        .await
        .map_err(workflow_pg_error)?;
    Ok(rows.first().map(|row| row.get("id")))
}

async fn existing_keyed_run<C>(
    conn: &C,
    tables: &WorkflowTables,
    app_id: &Uuid,
    workflow_name: &str,
    dedup_key: &String,
) -> Result<Option<String>, WorkflowApiError>
where
    C: compio_postgres::GenericClient + Sync,
{
    let sql = format!(
        "SELECT id \
               FROM {runs} \
              WHERE app_id = $1 AND workflow_name = $2 AND dedup_key = $3 \
              LIMIT 1",
        runs = tables.runs
    );
    let rows = conn
        .query(&sql, &[app_id, &workflow_name, &dedup_key])
        .await
        .map_err(workflow_pg_error)?;
    Ok(rows.first().map(|row| row.get("id")))
}

/// Requires `run.dedup_key` to be `Some` — this is the "keyed" join-or-create
/// path; an unkeyed run should call [`insert_run`] directly.
async fn join_or_create_keyed_run<C>(
    tx: &C,
    tables: &WorkflowTables,
    run: NewRun<'_>,
) -> Result<String, WorkflowApiError>
where
    C: compio_postgres::GenericClient + Sync,
{
    let app_id = run.app_id;
    let workflow_name = run.workflow_name;
    let input_journal_bytes = run.input_journal_bytes;
    let key = run
        .dedup_key
        .expect("join_or_create_keyed_run requires a dedup_key");
    if let Some(existing) = existing_keyed_run(tx, tables, app_id, workflow_name, key).await? {
        return Ok(existing);
    }
    check_create_journal_capacity(tx, app_id, input_journal_bytes).await?;
    let candidate = typed_id::new_workflow_run_id();
    if let Some(inserted) =
        insert_run_on_conflict_do_nothing(tx, tables, run, &candidate).await?
    {
        return Ok(inserted);
    }
    existing_keyed_run(tx, tables, app_id, workflow_name, key)
        .await?
        .ok_or_else(|| {
            WorkflowApiError::Database("workflow start conflict lost its incumbent".to_string())
        })
}

pub(crate) async fn start_scheduled_workflow_run<C>(
    tx: &C,
    app_id: &Uuid,
    workflow_name: &str,
    deploy_id: &str,
    input: &Value,
    dedup_key: &str,
    started_at: DateTime<Utc>,
) -> Result<String, RegistryError>
where
    C: compio_postgres::GenericClient + Sync,
{
    validate_workflow_name(workflow_name).map_err(workflow_api_error_to_registry)?;
    ensure_app_workflows_enabled(tx, app_id)
        .await
        .map_err(workflow_api_error_to_registry)?;
    let tables = provision_workflow_journal(tx, app_id)
        .await
        .map_err(workflow_api_error_to_registry)?;
    let key = normalize_key(Some(dedup_key.to_string()))
        .map_err(workflow_api_error_to_registry)?
        .ok_or_else(|| RegistryError::InvalidInput("scheduled workflow key is missing".to_string()))?;
    let input_journal_bytes = pg::json_column_size(tx, input)
        .await
        .map_err(WorkflowApiError::from)
        .map_err(workflow_api_error_to_registry)?;
    workflow_limits::lock_app_journal_accounting(tx, app_id).await?;
    join_or_create_keyed_run(
        tx,
        &tables,
        NewRun {
            app_id,
            workflow_name,
            deploy_id,
            input,
            input_journal_bytes,
            dedup_key: Some(&key),
            started_at: Some(started_at),
        },
    )
    .await
    .map_err(workflow_api_error_to_registry)
}

fn workflow_api_error_to_registry(error: WorkflowApiError) -> RegistryError {
    match error {
        WorkflowApiError::BadRequest(msg) | WorkflowApiError::PayloadTooLarge(msg) => {
            RegistryError::InvalidInput(msg)
        }
        WorkflowApiError::NotFound(msg) => RegistryError::NotFound(msg),
        WorkflowApiError::Conflict(msg) | WorkflowApiError::Restart(msg) => {
            RegistryError::Conflict(msg)
        }
        WorkflowApiError::JournalCapExceeded(msg)
        | WorkflowApiError::LimitExceeded(msg)
        | WorkflowApiError::RateLimitUnavailable(msg) => RegistryError::Conflict(msg),
        WorkflowApiError::RateLimited { retry_after_secs } => RegistryError::Conflict(format!(
            "scheduled workflow start rate limited; retry after {retry_after_secs:.0}s"
        )),
        WorkflowApiError::Unauthorized(msg) | WorkflowApiError::Forbidden(msg) => {
            RegistryError::InvalidInput(msg)
        }
        WorkflowApiError::Unavailable(msg) => RegistryError::Conflict(msg),
        WorkflowApiError::Database(msg) => RegistryError::Database(msg),
    }
}

async fn create_run_inner(
    state: &AppState,
    app_id: Uuid,
    workflow_name: String,
    body: CreateRunBody,
) -> Result<(StatusCode, Value), WorkflowApiError> {
    validate_workflow_name(&workflow_name)?;
    let policy = match body.on_conflict.as_ref() {
        Some(value) => value.policy().map_err(WorkflowApiError::BadRequest)?,
        None => ConflictPolicy::Join,
    };
    let dedup_key = normalize_key(body.key)?;

    let mut conn = state.registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(workflow_pg_error)?;

    ensure_app_workflows_enabled(&tx, &app_id).await?;
    let tables = provision_workflow_journal(&tx, &app_id).await?;
    let deploy = active_deploy_for_workflow(&tx, &app_id, &workflow_name).await?;
    let input_journal_bytes = pg::json_column_size(&tx, &body.input)
        .await
        .map_err(WorkflowApiError::from)?;
    workflow_limits::lock_app_journal_accounting(&tx, &app_id)
        .await
        .map_err(WorkflowApiError::from)?;
    let mut cascade_run_ids = Vec::new();

    let run_id = if let Some(key) = dedup_key.as_ref() {
        match policy {
            ConflictPolicy::Join => {
                join_or_create_keyed_run(
                    &tx,
                    &tables,
                    NewRun {
                        app_id: &app_id,
                        workflow_name: &workflow_name,
                        deploy_id: &deploy.id,
                        input: &body.input,
                        input_journal_bytes,
                        dedup_key: Some(key),
                        started_at: None,
                    },
                )
                .await?
            }
            ConflictPolicy::Reject => {
                if existing_keyed_run(&tx, &tables, &app_id, &workflow_name, key)
                    .await?
                    .is_some()
                {
                    return Err(WorkflowApiError::Conflict(
                        "workflow run already exists for key".to_string(),
                    ));
                }
                check_create_journal_capacity(&tx, &app_id, input_journal_bytes).await?;
                let candidate = typed_id::new_workflow_run_id();
                if let Some(inserted) = insert_run_on_conflict_do_nothing(
                    &tx,
                    &tables,
                    NewRun {
                        app_id: &app_id,
                        workflow_name: &workflow_name,
                        deploy_id: &deploy.id,
                        input: &body.input,
                        input_journal_bytes,
                        dedup_key: Some(key),
                        started_at: None,
                    },
                    &candidate,
                )
                .await?
                {
                    inserted
                } else {
                    return Err(WorkflowApiError::Conflict(
                        "workflow run already exists for key".to_string(),
                    ));
                }
            }
            ConflictPolicy::Replace => {
                check_create_journal_capacity(&tx, &app_id, input_journal_bytes).await?;
                let cancelled = tx.query(
                    &format!("UPDATE {runs} \
                        SET state = 'cancelled', \
                            dedup_key = NULL, \
                            wake_at = NULL, \
                            terminal_at = now(), \
                            output = NULL, \
                            error = NULL, \
                            output_kind = 'inline', \
                            output_hash = NULL, \
                            output_size = NULL, \
                            output_content_type = NULL, \
                            paused_from_status = NULL, \
                            claimed_by = NULL, \
                            lease_expires = NULL, \
                            dispatch_nonce = NULL \
                      WHERE app_id = $1 AND workflow_name = $2 AND dedup_key = $3 \
                      RETURNING id", runs = tables.runs),
                    &[&app_id, &workflow_name, key],
                )
                .await
                .map_err(workflow_pg_error)?;
                for row in cancelled {
                    let cancelled_run_id: String = row.get("id");
                    cascade_run_ids.extend(
                        workflow_engine::cascade_cancel_children_for_app(
                            &tx,
                            &tables,
                            &cancelled_run_id,
                        )
                        .await?,
                    );
                }

                let candidate = typed_id::new_workflow_run_id();
                if let Some(inserted) = insert_run_on_conflict_do_nothing(
                    &tx,
                    &tables,
                    NewRun {
                        app_id: &app_id,
                        workflow_name: &workflow_name,
                        deploy_id: &deploy.id,
                        input: &body.input,
                        input_journal_bytes,
                        dedup_key: Some(key),
                        started_at: None,
                    },
                    &candidate,
                )
                .await?
                {
                    inserted
                } else {
                    existing_keyed_run(&tx, &tables, &app_id, &workflow_name, key)
                        .await?
                        .ok_or_else(|| {
                            WorkflowApiError::Database(
                                "workflow replace conflict lost its winner".to_string(),
                            )
                        })?
                }
            }
        }
    } else {
        check_create_journal_capacity(&tx, &app_id, input_journal_bytes).await?;
        let candidate = typed_id::new_workflow_run_id();
        insert_run(
            &tx,
            &tables,
            NewRun {
                app_id: &app_id,
                workflow_name: &workflow_name,
                deploy_id: &deploy.id,
                input: &body.input,
                input_journal_bytes,
                dedup_key: None,
                started_at: None,
            },
            &candidate,
        )
        .await?;
        candidate
    };

    // Timer registration happens INSIDE the transaction. Registering after the
    // commit meant a registration failure returned 500 for a run that was
    // already durable and queued with `wake_at = now()`; the inflight reaper
    // then adopted and executed it, so an unkeyed client retry of that 500
    // started a second run and the workflow ran twice.
    workflow_engine::register_run_timer_in_tx(state, &tx, &run_id)
        .await
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    for cascade_run_id in &cascade_run_ids {
        workflow_engine::register_run_timer_in_tx(state, &tx, cascade_run_id)
            .await
            .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    }
    tx.commit()
        .await
        .map_err(workflow_pg_error)?;
    Ok((StatusCode::CREATED, json!({ "id": run_id, "state": "queued" })))
}

async fn start_many_inner(
    state: &AppState,
    app_id: Uuid,
    workflow_name: String,
    body: StartManyBody,
) -> Result<Value, WorkflowApiError> {
    validate_workflow_name(&workflow_name)?;
    if body.items.len() > workflow_engine::DEFAULT_MAX_START_MANY_BATCH {
        return Err(WorkflowApiError::LimitExceeded(format!(
            "startMany batch exceeds maxStartManyBatch ({} > {})",
            body.items.len(),
            workflow_engine::DEFAULT_MAX_START_MANY_BATCH
        )));
    }
    let policy = match body.on_conflict.as_ref() {
        Some(value) => value.policy().map_err(WorkflowApiError::BadRequest)?,
        None => ConflictPolicy::Join,
    };

    let mut conn = state.registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(workflow_pg_error)?;
    ensure_app_workflows_enabled(&tx, &app_id).await?;
    let tables = provision_workflow_journal(&tx, &app_id).await?;
    let deploy = active_deploy_for_workflow(&tx, &app_id, &workflow_name).await?;
    workflow_limits::lock_app_journal_accounting(&tx, &app_id)
        .await
        .map_err(WorkflowApiError::from)?;

    let mut results = Vec::with_capacity(body.items.len());
    let mut run_ids = Vec::with_capacity(body.items.len());
    let mut cascade_run_ids = Vec::new();
    let mut seen_keys = BTreeSet::new();
    for item in body.items {
        let input_journal_bytes = pg::json_column_size(&tx, &item.input)
            .await
            .map_err(WorkflowApiError::from)?;
        let key = normalize_key(item.key)?;
        if let Some(key) = key.as_ref() {
            let duplicate_in_batch = !seen_keys.insert(key.clone());
            let existing = existing_keyed_run(&tx, &tables, &app_id, &workflow_name, key).await?;
            match policy {
                ConflictPolicy::Join => {
                    let created = existing.is_none() && !duplicate_in_batch;
                    let run_id = join_or_create_keyed_run(
                        &tx,
                        &tables,
                        NewRun {
                            app_id: &app_id,
                            workflow_name: &workflow_name,
                            deploy_id: &deploy.id,
                            input: &item.input,
                            input_journal_bytes,
                            dedup_key: Some(key),
                            started_at: None,
                        },
                    )
                    .await?;
                    results.push(json!({
                        "run": { "id": run_id },
                        "id": run_id,
                        "runId": run_id,
                        "created": created,
                        "conflict": if created { Value::Null } else { json!("duplicate") },
                    }));
                    if created {
                        run_ids.push(run_id);
                    }
                }
                ConflictPolicy::Reject => {
                    if existing.is_some() || duplicate_in_batch {
                        results.push(json!({
                            "run": Value::Null,
                            "created": false,
                            "conflict": "rejected",
                        }));
                        continue;
                    }
                    check_create_journal_capacity(&tx, &app_id, input_journal_bytes).await?;
                    let run_id = typed_id::new_workflow_run_id();
                    insert_run(
                        &tx,
                        &tables,
                        NewRun {
                            app_id: &app_id,
                            workflow_name: &workflow_name,
                            deploy_id: &deploy.id,
                            input: &item.input,
                            input_journal_bytes,
                            dedup_key: Some(key),
                            started_at: None,
                        },
                        &run_id,
                    )
                    .await?;
                    results.push(json!({
                        "run": { "id": run_id },
                        "id": run_id,
                        "runId": run_id,
                        "created": true,
                    }));
                    run_ids.push(run_id);
                }
                ConflictPolicy::Replace => {
                    let cancelled = tx.query(
                        &format!("UPDATE {runs} \
                            SET state = 'cancelled', \
                                dedup_key = NULL, \
                                wake_at = NULL, \
                                terminal_at = now(), \
                                output = NULL, \
                                error = NULL, \
                                output_kind = 'inline', \
                                output_hash = NULL, \
                                output_size = NULL, \
                                output_content_type = NULL, \
                                paused_from_status = NULL, \
                                claimed_by = NULL, \
                                lease_expires = NULL, \
                                dispatch_nonce = NULL \
                          WHERE app_id = $1 AND workflow_name = $2 AND dedup_key = $3 \
                          RETURNING id", runs = tables.runs),
                        &[&app_id, &workflow_name, key],
                    )
                    .await
                    .map_err(workflow_pg_error)?;
                    for row in cancelled {
                        let cancelled_run_id: String = row.get("id");
                        cascade_run_ids.extend(
                            workflow_engine::cascade_cancel_children_for_app(
                                &tx,
                                &tables,
                                &cancelled_run_id,
                            )
                            .await?,
                        );
                    }
                    check_create_journal_capacity(&tx, &app_id, input_journal_bytes).await?;
                    let run_id = typed_id::new_workflow_run_id();
                    insert_run(
                        &tx,
                        &tables,
                        NewRun {
                            app_id: &app_id,
                            workflow_name: &workflow_name,
                            deploy_id: &deploy.id,
                            input: &item.input,
                            input_journal_bytes,
                            dedup_key: Some(key),
                            started_at: None,
                        },
                        &run_id,
                    )
                    .await?;
                    results.push(json!({
                        "run": { "id": run_id },
                        "id": run_id,
                        "runId": run_id,
                        "created": true,
                        "conflict": if existing.is_some() || duplicate_in_batch { json!("replaced") } else { Value::Null },
                    }));
                    run_ids.push(run_id);
                }
            }
        } else {
            check_create_journal_capacity(&tx, &app_id, input_journal_bytes).await?;
            let run_id = typed_id::new_workflow_run_id();
            insert_run(
                &tx,
                &tables,
                NewRun {
                    app_id: &app_id,
                    workflow_name: &workflow_name,
                    deploy_id: &deploy.id,
                    input: &item.input,
                    input_journal_bytes,
                    dedup_key: None,
                    started_at: None,
                },
                &run_id,
            )
            .await?;
            results.push(json!({
                "run": { "id": run_id },
                "id": run_id,
                "runId": run_id,
                "created": true,
            }));
            run_ids.push(run_id);
        }
    }

    // Same atomicity rule as `create_run_inner`: every run this batch created
    // gets its timer in the batch's own transaction, so a registration failure
    // rolls the whole batch back instead of leaving reaper-adoptable runs
    // behind an error response.
    for run_id in &run_ids {
        workflow_engine::register_run_timer_in_tx(state, &tx, run_id)
            .await
            .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    }
    for run_id in &cascade_run_ids {
        workflow_engine::register_run_timer_in_tx(state, &tx, run_id)
            .await
            .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    }
    tx.commit()
        .await
        .map_err(workflow_pg_error)?;
    Ok(json!({ "results": results }))
}

async fn consume_signal_rate_limit(
    state: &AppState,
    app_id: Uuid,
) -> Result<(), WorkflowApiError> {
    let key = format!("control:workflow_signal:app:{app_id}");
    let quota = Quota {
        capacity: SIGNAL_RATE_LIMIT_CAPACITY,
        refill_per_sec: SIGNAL_RATE_LIMIT_REFILL_PER_SEC,
    };
    match rate_limit::consume(&state.control_pg, &key, quota).await {
        Ok(RateLimitDecision::Allowed) => Ok(()),
        Ok(RateLimitDecision::Throttled(limited)) => Err(WorkflowApiError::RateLimited {
            retry_after_secs: limited.retry_after_secs,
        }),
        Err(e) => Err(WorkflowApiError::RateLimitUnavailable(e.to_string())),
    }
}

pub async fn create_run(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    workflow_name: Path<String>,
    body: Json<CreateRunBody>,
) -> web::HttpResponse {
    let app_id = match app_id_from_channel(&req, &state) {
        Ok(app_id) => app_id,
        Err(resp) => return resp,
    };
    match create_run_inner(&state, app_id, workflow_name.into_inner(), body.into_inner()).await {
        Ok((status, value)) => web::HttpResponse::build(status).json(&value),
        Err(e) => e.response(),
    }
}

pub async fn start_many_runs(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    workflow_name: Path<String>,
    body: Json<StartManyBody>,
) -> web::HttpResponse {
    let app_id = match app_id_from_channel(&req, &state) {
        Ok(app_id) => app_id,
        Err(resp) => return resp,
    };
    match start_many_inner(&state, app_id, workflow_name.into_inner(), body.into_inner()).await {
        Ok(value) => web::HttpResponse::Ok().json(&value),
        Err(e) => e.response(),
    }
}

pub async fn get_run_status(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    run_id: Path<String>,
) -> web::HttpResponse {
    let app_id = match app_id_from_channel(&req, &state) {
        Ok(app_id) => app_id,
        Err(resp) => return resp,
    };
    let run_id = run_id.into_inner();
    if let Err(e) = validate_run_id(&run_id) {
        return e.response();
    }
    let tables = WorkflowTables::for_app_id(&app_id);
    let sql = format!(
        "SELECT state, output, error, output_kind, output_hash, output_size, output_content_type \
               FROM {runs} \
              WHERE id = $1 AND app_id = $2",
        runs = tables.runs
    );
    let rows = match state
        .control_pg
        .query(&sql, &[&run_id, &app_id])
        .await
    {
        Ok(rows) => rows,
        Err(e) => return WorkflowApiError::Database(e.to_string()).response(),
    };
    let Some(row) = rows.first() else {
        return WorkflowApiError::NotFound("workflow run not found".to_string()).response();
    };
    let state_value: String = row.get("state");
    let error: Option<Value> = row.get("error");
    web::HttpResponse::Ok().json(&json!({
        "state": state_value,
        "output": status_output(row),
        "error": error,
    }))
}

pub async fn get_run_output(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    run_id: Path<String>,
) -> web::HttpResponse {
    let app_id = match app_id_from_channel(&req, &state) {
        Ok(app_id) => app_id,
        Err(resp) => return resp,
    };
    let run_id = run_id.into_inner();
    if let Err(e) = validate_run_id(&run_id) {
        return e.response();
    }
    let tables = WorkflowTables::for_app_id(&app_id);
    let sql = format!(
        "SELECT output, output_kind, output_hash, output_size, output_content_type \
               FROM {runs} \
              WHERE id = $1 AND app_id = $2",
        runs = tables.runs
    );
    let rows = match state
        .control_pg
        .query(&sql, &[&run_id, &app_id])
        .await
    {
        Ok(rows) => rows,
        Err(e) => return WorkflowApiError::Database(e.to_string()).response(),
    };
    let Some(row) = rows.first() else {
        return WorkflowApiError::NotFound("workflow run not found".to_string()).response();
    };
    output_row_response(&req, &state, app_id, row).await
}

pub async fn get_step_output(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    path: Path<StepOutputPath>,
    query: Query<StepOutputQuery>,
) -> web::HttpResponse {
    let app_id = match app_id_from_channel(&req, &state) {
        Ok(app_id) => app_id,
        Err(resp) => return resp,
    };
    let path = path.into_inner();
    if let Err(e) = validate_run_id(&path.run_id) {
        return e.response();
    }
    let occurrence = query.occurrence.unwrap_or(0);
    if occurrence < 0 {
        return WorkflowApiError::BadRequest("step occurrence must be >= 0".to_string())
            .response();
    }
    let tables = WorkflowTables::for_app_id(&app_id);
    let sql = format!(
        "SELECT s.output, s.output_kind, s.output_hash, s.output_size, s.output_content_type \
               FROM {steps} s \
               JOIN {runs} r ON r.id = s.run_id \
              WHERE s.run_id = $1 \
                AND r.app_id = $2 \
                AND s.name = $3 \
                AND s.name_occurrence = $4 \
                AND s.state = 'completed' \
              ORDER BY s.ordinal \
              LIMIT 1",
        steps = tables.steps,
        runs = tables.runs
    );
    let rows = match state
        .control_pg
        .query(&sql, &[&path.run_id, &app_id, &path.name, &occurrence])
        .await
    {
        Ok(rows) => rows,
        Err(e) => return WorkflowApiError::Database(e.to_string()).response(),
    };
    let Some(row) = rows.first() else {
        return WorkflowApiError::NotFound("workflow step output not found".to_string())
            .response();
    };
    output_row_response(&req, &state, app_id, row).await
}

async fn output_row_response(
    req: &web::HttpRequest,
    state: &AppState,
    app_id: Uuid,
    row: &compio_postgres::Row,
) -> web::HttpResponse {
    let output_kind: String = row.get("output_kind");
    let content_type: String = row
        .get::<_, Option<String>>("output_content_type")
        .unwrap_or_else(|| "application/json".to_string());
    let body = if output_kind == "blob" {
        let hash: Option<String> = row.get("output_hash");
        let size: Option<i64> = row.get("output_size");
        let Some(hash) = hash else {
            return WorkflowApiError::Database("blob output missing hash".to_string()).response();
        };
        let data = match state.workflow_blob_store.get_blob(&hash).await {
            Ok(data) => data,
            Err(e) => {
                return infrastructure_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "workflow output blob read",
                    e,
                );
            }
        };
        if let Some(expected) = size {
            if expected >= 0 && data.len() as i64 != expected {
                return infrastructure_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "workflow output blob read",
                    format!("blob size mismatch for {hash}"),
                );
            }
        }
        data.to_vec()
    } else {
        let output: Option<Value> = row.get("output");
        match serde_json::to_vec(&output.unwrap_or(Value::Null)) {
            Ok(bytes) => bytes,
            Err(e) => return WorkflowApiError::Database(e.to_string()).response(),
        }
    };

    let total = body.len();
    let range = match parse_bytes_range(req, total) {
        Ok(range) => range,
        Err(resp) => return resp,
    };
    let (status, start, end) = match range {
        Some((start, end)) => (StatusCode::PARTIAL_CONTENT, start, end),
        None if total == 0 => (StatusCode::OK, 0, 0),
        None => (StatusCode::OK, 0, total - 1),
    };
    let slice = if total == 0 {
        Vec::new()
    } else {
        body[start..=end].to_vec()
    };

    record_workflow_output_read_usage(state, &app_id, slice.len() as i64).await;

    let mut builder = web::HttpResponse::build(status);
    builder.header("content-type", content_type);
    builder.header("accept-ranges", "bytes");
    builder.header("content-length", slice.len().to_string());
    if status == StatusCode::PARTIAL_CONTENT {
        builder.header(
            "content-range",
            format!("bytes {start}-{end}/{total}"),
        );
    }
    builder.body(slice)
}

fn parse_bytes_range(
    req: &web::HttpRequest,
    total: usize,
) -> Result<Option<(usize, usize)>, web::HttpResponse> {
    let Some(raw) = req.headers().get("range").and_then(|value| value.to_str().ok()) else {
        return Ok(None);
    };
    let Some(spec) = raw.strip_prefix("bytes=") else {
        return Err(web::HttpResponse::build(StatusCode::RANGE_NOT_SATISFIABLE).finish());
    };
    if spec.contains(',') || total == 0 {
        let mut builder = web::HttpResponse::build(StatusCode::RANGE_NOT_SATISFIABLE);
        builder.header("content-range", format!("bytes */{total}"));
        return Err(builder.finish());
    }
    let Some((start_raw, end_raw)) = spec.split_once('-') else {
        return Err(web::HttpResponse::build(StatusCode::RANGE_NOT_SATISFIABLE).finish());
    };
    let (start, end) = if start_raw.is_empty() {
        let suffix = match end_raw.parse::<usize>() {
            Ok(value) if value > 0 => value,
            _ => return Err(web::HttpResponse::build(StatusCode::RANGE_NOT_SATISFIABLE).finish()),
        };
        let start = total.saturating_sub(suffix);
        (start, total - 1)
    } else {
        let start = match start_raw.parse::<usize>() {
            Ok(value) => value,
            Err(_) => return Err(web::HttpResponse::build(StatusCode::RANGE_NOT_SATISFIABLE).finish()),
        };
        let end = if end_raw.is_empty() {
            total - 1
        } else {
            match end_raw.parse::<usize>() {
                Ok(value) => value.min(total - 1),
                Err(_) => {
                    return Err(web::HttpResponse::build(StatusCode::RANGE_NOT_SATISFIABLE)
                        .finish());
                }
            }
        };
        (start, end)
    };
    if start >= total || start > end {
        let mut builder = web::HttpResponse::build(StatusCode::RANGE_NOT_SATISFIABLE);
        builder.header("content-range", format!("bytes */{total}"));
        return Err(builder.finish());
    }
    Ok(Some((start, end)))
}

async fn record_workflow_output_read_usage(state: &AppState, app_id: &Uuid, bytes: i64) {
    let mut deltas = vec![("storage_ops".to_string(), 1)];
    if bytes > 0 {
        deltas.push(("storage_egress_bytes".to_string(), bytes));
        deltas.push(("egress_bytes".to_string(), bytes));
    }
    let metering = crate::metering::Metering::new(state.registry.clone());
    if let Err(e) = metering
        .record_direct(app_id, &deltas, state.billing_stream.as_ref())
        .await
    {
        tracing::warn!(
            app_id = %app_id,
            error = %e,
            "workflow output read metering failed"
        );
    }
}

pub async fn signal_run(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    run_id: Path<String>,
    body: Json<SignalBody>,
) -> web::HttpResponse {
    let app_id = match app_id_from_channel(&req, &state) {
        Ok(app_id) => app_id,
        Err(resp) => return resp,
    };
    let run_id = run_id.into_inner();
    if let Err(e) = validate_run_id(&run_id) {
        return e.response();
    }
    let body = body.into_inner();
    if let Err(e) = validate_ingress_signal_type(&body.signal_type) {
        return e.response();
    }
    if let Err(e) = signal_payload_size(&body.payload) {
        return e.response();
    }
    if let Err(e) = consume_signal_rate_limit(&state, app_id).await {
        return e.response();
    }

    let mut conn = match state.registry.conn().await {
        Ok(conn) => conn,
        Err(e) => return WorkflowApiError::from(e).response(),
    };
    let tx = match conn.transaction().await {
        Ok(tx) => tx,
        Err(e) => return workflow_pg_error(e).response(),
    };
    if let Err(e) = ensure_app_workflows_enabled(&tx, &app_id).await {
        return e.response();
    }
    let tables = match provision_workflow_journal(&tx, &app_id).await {
        Ok(tables) => tables,
        Err(e) => return e.response(),
    };
    let payload_journal_bytes = match pg::json_column_size(&tx, &body.payload).await {
        Ok(bytes) => bytes,
        Err(e) => return WorkflowApiError::from(e).response(),
    };
    if let Err(e) = workflow_limits::lock_app_journal_accounting(&tx, &app_id).await {
        return WorkflowApiError::from(e).response();
    }
    let lock_sql = format!(
        "SELECT state, waiting_step_key \
               FROM {runs} \
              WHERE id = $1 AND app_id = $2 \
              FOR UPDATE",
        runs = tables.runs
    );
    let rows = match tx
        .query(&lock_sql, &[&run_id, &app_id])
        .await
    {
        Ok(rows) => rows,
        Err(e) => return workflow_pg_error(e).response(),
    };
    let Some(row) = rows.first() else {
        let _ = tx.commit().await;
        return WorkflowApiError::NotFound("workflow run not found".to_string()).response();
    };
    let run_state: String = row.get("state");
    let waiting_step_key: Option<String> = row.get("waiting_step_key");
    if let Err(e) = check_signal_journal_capacity(&tx, &app_id, payload_journal_bytes).await {
        return e.response();
    }
    let signal_id = typed_id::new_workflow_signal_id();
    let insert_signal_sql = format!(
        "INSERT INTO {signals} \
                (id, run_id, type, payload, origin, delivery) \
             VALUES ($1, $2, $3, $4, 'app', 'direct')",
        signals = tables.signals
    );
    if let Err(e) = tx
        .execute(
            &insert_signal_sql,
            &[&signal_id, &run_id, &body.signal_type, &body.payload],
        )
        .await
    {
        return WorkflowApiError::Database(e.to_string()).response();
    }
    let wakes_run = run_state == "waiting"
        && waiting_key_matches_signal(waiting_step_key.as_deref(), &body.signal_type);
    if wakes_run {
        let wake_sql = format!(
            "UPDATE {runs} \
                    SET wake_at = now() \
                  WHERE id = $1 AND app_id = $2",
            runs = tables.runs
        );
        if let Err(e) = tx
            .execute(&wake_sql, &[&run_id, &app_id])
            .await
        {
            return WorkflowApiError::Database(e.to_string()).response();
        }
    }
    // Registered in-transaction: the wake_at UPDATE above and the timer row it
    // implies have to become visible together, or a registration failure
    // reports an error over a durably woken run the reaper will pick up anyway.
    if wakes_run {
        if let Err(e) = workflow_engine::register_run_timer_in_tx(&state, &tx, &run_id).await {
            return WorkflowApiError::from(e).response();
        }
    }
    if let Err(e) = tx.commit().await {
        return WorkflowApiError::Database(e.to_string()).response();
    }
    web::HttpResponse::Accepted().json(&json!({ "id": signal_id }))
}

pub async fn create_run_signal_token(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    run_id: Path<String>,
    body: Json<CreateSignalTokenBody>,
) -> web::HttpResponse {
    let app_id = match app_id_from_channel(&req, &state) {
        Ok(app_id) => app_id,
        Err(resp) => return resp,
    };
    let run_id = run_id.into_inner();
    if let Err(e) = validate_run_id(&run_id) {
        return e.response();
    }
    match create_run_signal_token_inner(&state, app_id, &run_id, body.into_inner()).await {
        Ok(value) => web::HttpResponse::Ok().json(&value),
        Err(e) => e.response(),
    }
}

async fn create_run_signal_token_inner(
    state: &AppState,
    app_id: Uuid,
    run_id: &str,
    body: CreateSignalTokenBody,
) -> Result<Value, WorkflowApiError> {
    let types = validate_token_types(&body.types)?;
    let ttl_secs = validate_token_ttl(&body.ttl)?;
    let mut conn = state.registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    ensure_app_workflows_enabled(&tx, &app_id).await?;
    let tables = provision_workflow_journal(&tx, &app_id).await?;
    let run_sql = format!(
        "SELECT state, signal_epoch \
               FROM {runs} \
              WHERE id = $1 AND app_id = $2",
        runs = tables.runs
    );
    let rows = tx
        .query(&run_sql, &[&run_id, &app_id])
        .await
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    let Some(row) = rows.first() else {
        tx.commit()
            .await
            .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
        return Err(WorkflowApiError::NotFound(
            "workflow run not found".to_string(),
        ));
    };
    let run_state: String = row.get("state");
    if is_terminal_or_compensating(&run_state) {
        tx.commit()
            .await
            .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
        return Err(WorkflowApiError::Conflict(format!(
            "workflow run cannot receive external signals while {run_state}"
        )));
    }
    let epoch: i32 = row.get("signal_epoch");
    let secret = active_or_create_signal_secret(&tx, state, &app_id).await?;
    tx.commit()
        .await
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;

    let exp = Utc::now().timestamp().saturating_add(ttl_secs);
    let claims = typed_id::WorkflowSignalTokenClaims {
        app_id: app_id.to_string(),
        run_id: Some(run_id.to_string()),
        topic: None,
        types,
        exp,
        epoch: i64::from(epoch),
    };
    let token = typed_id::sign_workflow_signal_token(&claims, &secret)
        .map_err(WorkflowApiError::Database)?;
    Ok(json!({
        "token": token,
        "expiresAt": claims_expiry(&claims)?.to_rfc3339(),
    }))
}

pub async fn create_topic_signal_token(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    topic: Path<String>,
    body: Json<CreateSignalTokenBody>,
) -> web::HttpResponse {
    let app_id = match app_id_from_channel(&req, &state) {
        Ok(app_id) => app_id,
        Err(resp) => return resp,
    };
    let topic = topic.into_inner();
    if let Err(e) = validate_topic(&topic) {
        return e.response();
    }
    match create_topic_signal_token_inner(&state, app_id, &topic, body.into_inner()).await {
        Ok(value) => web::HttpResponse::Ok().json(&value),
        Err(e) => e.response(),
    }
}

async fn create_topic_signal_token_inner(
    state: &AppState,
    app_id: Uuid,
    topic: &str,
    body: CreateSignalTokenBody,
) -> Result<Value, WorkflowApiError> {
    let types = validate_token_types(&body.types)?;
    let ttl_secs = validate_token_ttl(&body.ttl)?;
    let mut conn = state.registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    let _deploy_id = active_deploy_id_for_app(&tx, &app_id).await?;
    let secret = active_or_create_signal_secret(&tx, state, &app_id).await?;
    tx.commit()
        .await
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;

    let exp = Utc::now().timestamp().saturating_add(ttl_secs);
    let claims = typed_id::WorkflowSignalTokenClaims {
        app_id: app_id.to_string(),
        run_id: None,
        topic: Some(topic.to_string()),
        types,
        exp,
        epoch: 0,
    };
    let token = typed_id::sign_workflow_signal_token(&claims, &secret)
        .map_err(WorkflowApiError::Database)?;
    Ok(json!({
        "token": token,
        "expiresAt": claims_expiry(&claims)?.to_rfc3339(),
    }))
}

pub async fn publish_topic_signal(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    topic: Path<String>,
    body: Json<PublishTopicBody>,
) -> web::HttpResponse {
    let app_id = match app_id_from_channel(&req, &state) {
        Ok(app_id) => app_id,
        Err(resp) => return resp,
    };
    let topic = topic.into_inner();
    if let Err(e) = validate_topic(&topic) {
        return e.response();
    }
    match publish_topic_signal_inner(&state, app_id, &topic, body.into_inner(), "app").await {
        Ok(value) => web::HttpResponse::Accepted().json(&value),
        Err(e) => e.response(),
    }
}

async fn publish_topic_signal_inner(
    state: &AppState,
    app_id: Uuid,
    topic: &str,
    body: PublishTopicBody,
    origin: &str,
) -> Result<Value, WorkflowApiError> {
    validate_ingress_signal_type(&body.signal_type)?;
    signal_payload_size(&body.payload)?;
    let idempotency_key = body
        .idempotency_key
        .filter(|key| !key.trim().is_empty())
        .unwrap_or_else(typed_id::new_workflow_broadcast_id);
    insert_topic_broadcast(
        state,
        TopicBroadcast {
            app_id,
            topic,
            signal_type: &body.signal_type,
            payload: &body.payload,
            origin,
            idempotency_key: &idempotency_key,
            expires_at: Utc::now() + chrono::Duration::seconds(SIGNAL_TOKEN_MAX_TTL_SECS),
        },
    )
    .await
}

pub async fn ingress_signal(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    body: Json<IngressSignalBody>,
) -> web::HttpResponse {
    if let Err(resp) = check_control_auth(&req, &state) {
        return resp;
    }
    if let Err(e) = ensure_public_ingress_enabled(&state).await {
        return e.response();
    }
    match ingress_signal_inner(&state, body.into_inner()).await {
        Ok(value) => web::HttpResponse::Accepted().json(&value),
        Err(e) => e.response(),
    }
}

pub async fn force_signal_fanout_tick(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Err(resp) = check_control_auth(&req, &state) {
        return resp;
    }
    match crate::cron::workflow_signal_fanout::tick(&state).await {
        Ok(stats) => web::HttpResponse::Ok().json(&json!({
            "broadcasts": stats.broadcasts,
            "deliveries": stats.deliveries,
        })),
        Err(e) => WorkflowApiError::from(e).response(),
    }
}

async fn ingress_signal_inner(
    state: &AppState,
    body: IngressSignalBody,
) -> Result<Value, WorkflowApiError> {
    if body.token.trim().is_empty() {
        return Err(WorkflowApiError::Unauthorized(
            "signal token is required".to_string(),
        ));
    }
    signal_payload_size(&body.payload)?;
    let claims = verify_signal_token(state, &body.token).await?;
    validate_signal_token_time(&claims, Utc::now().timestamp())?;
    let signal_type = resolve_ingress_signal_type(&claims, body.signal_type)?;
    let app_id = parse_app_id(&claims.app_id)
        .map_err(|_| WorkflowApiError::Unauthorized("invalid signal token".to_string()))?;
    {
        let conn = state.registry.conn().await.map_err(WorkflowApiError::from)?;
        ensure_app_workflows_enabled(&conn, &app_id).await?;
    }
    let idempotency_key = signal_token_replay_key(&body.token);
    match (claims.run_id.as_deref(), claims.topic.as_deref()) {
        (Some(run_id), None) => {
            deliver_ingress_run_signal(
                state,
                app_id,
                run_id,
                &signal_type,
                &body.payload,
                claims.epoch,
                &idempotency_key,
            )
            .await
        }
        (None, Some(topic)) => {
            validate_topic(topic)?;
            insert_topic_broadcast(
                state,
                TopicBroadcast {
                    app_id,
                    topic,
                    signal_type: &signal_type,
                    payload: &body.payload,
                    origin: "ingress",
                    idempotency_key: &idempotency_key,
                    expires_at: claims_expiry(&claims)?,
                },
            )
            .await
        }
        _ => Err(WorkflowApiError::Unauthorized(
            "invalid signal token".to_string(),
        )),
    }
}

fn is_terminal_or_compensating(state: &str) -> bool {
    matches!(
        state,
        "completed" | "failed" | "cancelled" | "stalled" | "compensating"
    )
}

async fn deliver_ingress_run_signal(
    state: &AppState,
    app_id: Uuid,
    run_id: &str,
    signal_type: &str,
    payload: &Value,
    token_epoch: i64,
    idempotency_key: &str,
) -> Result<Value, WorkflowApiError> {
    validate_run_id(run_id)?;
    let mut conn = state.registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    ensure_app_workflows_enabled(&tx, &app_id).await?;
    let tables = provision_workflow_journal(&tx, &app_id).await?;
    let payload_journal_bytes = pg::json_column_size(&tx, payload)
        .await
        .map_err(WorkflowApiError::from)?;
    workflow_limits::lock_app_journal_accounting(&tx, &app_id)
        .await
        .map_err(WorkflowApiError::from)?;
    let lock_sql = format!(
        "SELECT state, waiting_step_key, signal_epoch \
               FROM {runs} \
              WHERE id = $1 AND app_id = $2 \
              FOR UPDATE",
        runs = tables.runs
    );
    let rows = tx
        .query(&lock_sql, &[&run_id, &app_id])
        .await
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    let Some(row) = rows.first() else {
        tx.commit()
            .await
            .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
        return Err(WorkflowApiError::NotFound(
            "workflow run not found".to_string(),
        ));
    };
    let run_state: String = row.get("state");
    if is_terminal_or_compensating(&run_state) {
        tx.commit()
            .await
            .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
        return Err(WorkflowApiError::Conflict(format!(
            "workflow run cannot receive external signals while {run_state}"
        )));
    }
    let current_epoch: i32 = row.get("signal_epoch");
    if i64::from(current_epoch) != token_epoch {
        tx.commit()
            .await
            .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
        return Err(WorkflowApiError::Forbidden(
            "signal token is stale".to_string(),
        ));
    }
    let waiting_step_key: Option<String> = row.get("waiting_step_key");
    check_signal_journal_capacity(&tx, &app_id, payload_journal_bytes).await?;
    let signal_id = typed_id::new_workflow_signal_id();
    let insert_sql = format!(
        "INSERT INTO {signals} \
                (id, run_id, type, payload, origin, delivery, idempotency_key) \
             VALUES ($1, $2, $3, $4, 'ingress', 'direct', $5)",
        signals = tables.signals
    );
    let inserted = tx
        .execute(
            &insert_sql,
            &[&signal_id, &run_id, &signal_type, payload, &idempotency_key],
        )
        .await;
    if let Err(e) = inserted {
        if e.code() == Some(&SqlState::UNIQUE_VIOLATION) {
            return Err(WorkflowApiError::Conflict(
                "signal token replay rejected".to_string(),
            ));
        }
        return Err(WorkflowApiError::Database(e.to_string()));
    }
    let wakes_run =
        run_state == "waiting" && waiting_key_matches_signal(waiting_step_key.as_deref(), signal_type);
    if wakes_run {
        let wake_sql = format!(
            "UPDATE {runs} \
                SET wake_at = now() \
              WHERE id = $1 AND app_id = $2",
            runs = tables.runs
        );
        tx.execute(
            &wake_sql,
            &[&run_id, &app_id],
        )
        .await
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    }
    if wakes_run {
        workflow_engine::register_run_timer_in_tx(state, &tx, run_id)
            .await
            .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    }
    tx.commit()
        .await
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    Ok(json!({ "id": signal_id, "runId": run_id }))
}

/// Grouped inputs for [`insert_topic_broadcast`] — kept as a struct rather
/// than individual parameters purely to stay under clippy's
/// `too_many_arguments` threshold; every field is still required and read
/// exactly once.
struct TopicBroadcast<'a> {
    app_id: Uuid,
    topic: &'a str,
    signal_type: &'a str,
    payload: &'a Value,
    origin: &'a str,
    idempotency_key: &'a str,
    expires_at: DateTime<Utc>,
}

async fn insert_topic_broadcast(
    state: &AppState,
    broadcast: TopicBroadcast<'_>,
) -> Result<Value, WorkflowApiError> {
    let TopicBroadcast {
        app_id,
        topic,
        signal_type,
        payload,
        origin,
        idempotency_key,
        expires_at,
    } = broadcast;
    validate_topic(topic)?;
    validate_ingress_signal_type(signal_type)?;
    signal_payload_size(payload)?;
    let mut conn = state.registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    let deploy_id = active_deploy_id_for_app(&tx, &app_id).await?;
    let broadcast_id = typed_id::new_workflow_broadcast_id();
    let inserted = tx
        .execute(
            "INSERT INTO zeroship.workflow_broadcasts \
                (id, app_id, topic, type, payload, origin, idempotency_key, deploy_id, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            &[
                &broadcast_id,
                &app_id,
                &topic,
                &signal_type,
                payload,
                &origin,
                &idempotency_key,
                &deploy_id,
                &expires_at,
            ],
        )
        .await;
    if let Err(e) = inserted {
        if e.code() == Some(&SqlState::UNIQUE_VIOLATION) {
            return Err(WorkflowApiError::Conflict(
                "signal token replay rejected".to_string(),
            ));
        }
        return Err(WorkflowApiError::Database(e.to_string()));
    }
    tx.commit()
        .await
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    Ok(json!({ "id": broadcast_id, "topic": topic }))
}

pub async fn pause_run(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    run_id: Path<String>,
) -> web::HttpResponse {
    control_transition(req, state, run_id, "pause", None).await
}

pub async fn resume_run(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    run_id: Path<String>,
) -> web::HttpResponse {
    control_transition(req, state, run_id, "resume", None).await
}

pub async fn cancel_run(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    run_id: Path<String>,
    body: Option<Json<CancelBody>>,
) -> web::HttpResponse {
    control_transition(req, state, run_id, "cancel", body.map(|body| body.into_inner())).await
}

pub async fn restart_run(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    run_id: Path<String>,
    body: Json<RestartBody>,
) -> web::HttpResponse {
    let app_id = match app_id_from_channel(&req, &state) {
        Ok(app_id) => app_id,
        Err(resp) => return resp,
    };
    let run_id = run_id.into_inner();
    if let Err(e) = validate_run_id(&run_id) {
        return e.response();
    }
    match restart_run_inner(&state, app_id, &run_id, body.into_inner()).await {
        Ok(value) => web::HttpResponse::Ok().json(&value),
        Err(e) => e.response(),
    }
}

async fn restart_run_inner(
    state: &AppState,
    app_id: Uuid,
    run_id: &str,
    body: RestartBody,
) -> Result<Value, WorkflowApiError> {
    let full_restart = body.from.is_none();
    let deploy_pin = body
        .deploy
        .as_ref()
        .map(RestartDeployBody::pin)
        .unwrap_or(if full_restart { "latest" } else { "started" });
    if !matches!(deploy_pin, "latest" | "started") {
        return Err(WorkflowApiError::Restart(format!(
            "invalid restart deploy pin '{deploy_pin}'"
        )));
    }
    if !full_restart && deploy_pin != "started" {
        return Err(WorkflowApiError::Restart(
            "partial restart cannot change deploy pin".to_string(),
        ));
    }

    let mut conn = state.registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    ensure_app_workflows_enabled(&tx, &app_id).await?;
    let tables = provision_workflow_journal(&tx, &app_id).await?;

    tx.query("SELECT pg_advisory_xact_lock(hashtext($1)::bigint)", &[&run_id])
        .await
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;

    let rows = tx
        .query(
            &format!("SELECT workflow_name, deploy_id, output_kind, output_hash \
               FROM {runs} \
              WHERE id = $1 AND app_id = $2 \
              FOR UPDATE", runs = tables.runs),
            &[&run_id, &app_id],
        )
        .await
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    let Some(row) = rows.first() else {
        tx.commit()
            .await
            .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
        return Err(WorkflowApiError::NotFound(
            "workflow run not found".to_string(),
        ));
    };
    let workflow_name: String = row.get("workflow_name");
    let current_deploy_id: String = row.get("deploy_id");
    let output_kind: String = row.get("output_kind");
    let output_hash: Option<String> = row.get("output_hash");

    let target_ordinal = if let Some(target) = body.from.as_ref() {
        if target.name.is_empty() {
            return Err(WorkflowApiError::BadRequest(
                "restart target name must not be empty".to_string(),
            ));
        }
        let occurrence = target.occurrence.unwrap_or(0);
        if occurrence < 0 {
            return Err(WorkflowApiError::BadRequest(
                "restart target occurrence must be >= 0".to_string(),
            ));
        }
        let rows = tx
            .query(
                &format!("SELECT ordinal \
                   FROM {steps} \
                  WHERE run_id = $1 AND name = $2 AND name_occurrence = $3",
                    steps = tables.steps
                ),
                &[&run_id, &target.name, &occurrence],
            )
            .await
            .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
        let Some(row) = rows.first() else {
            return Err(WorkflowApiError::NotFound(
                "workflow restart target not found".to_string(),
            ));
        };
        row.get::<_, i32>("ordinal")
    } else {
        0
    };

    if target_ordinal > 0 {
        let rows = tx
            .query(
                &format!("SELECT 1 \
                   FROM {steps} \
                  WHERE run_id = $1 \
                    AND ordinal < $2 \
                    AND compensation_finished_at IS NOT NULL \
                  LIMIT 1", steps = tables.steps),
                &[&run_id, &target_ordinal],
            )
            .await
            .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
        if !rows.is_empty() {
            return Err(WorkflowApiError::Restart(
                "cannot partial-restart past a completed compensation; use a full restart"
                    .to_string(),
            ));
        }
    }

    let target_deploy_id = if full_restart && deploy_pin == "latest" {
        active_deploy_for_workflow(&tx, &app_id, &workflow_name)
            .await?
            .id
    } else {
        current_deploy_id.clone()
    };
    let signal_epoch_bump: i32 = if target_deploy_id != current_deploy_id {
        1
    } else {
        0
    };

    tx.execute(
        &format!("UPDATE {blobs} b \
            SET refcount = GREATEST(refcount - 1, 0), \
                last_referenced_at = now() \
           FROM {steps} s \
          WHERE s.run_id = $1 \
            AND s.ordinal >= $2 \
            AND s.output_kind = 'blob' \
            AND b.hash = s.output_hash", blobs = tables.blobs, steps = tables.steps),
        &[&run_id, &target_ordinal],
    )
    .await
    .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    if output_kind == "blob" {
        if let Some(hash) = output_hash.as_ref() {
            tx.execute(
                &format!("UPDATE {blobs} \
                    SET refcount = GREATEST(refcount - 1, 0), \
                        last_referenced_at = now() \
                  WHERE hash = $1", blobs = tables.blobs),
                &[hash],
            )
            .await
            .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
        }
    }

    tx.execute(
        &format!("UPDATE {signals} \
            SET consumed_by = NULL \
          WHERE consumed_by = $1 \
            AND delivery <> 'topic' \
            AND id IN ( \
                SELECT consumed_signal_id \
                  FROM {steps} \
                 WHERE run_id = $1 \
                   AND ordinal >= $2 \
                   AND consumed_signal_id IS NOT NULL \
            )", signals = tables.signals, steps = tables.steps),
        &[&run_id, &target_ordinal],
    )
    .await
    .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    tx.execute(
        &format!("DELETE FROM {signals} \
          WHERE run_id = $1 \
            AND delivery = 'topic' \
            AND id IN ( \
                SELECT consumed_signal_id \
                  FROM {steps} \
                 WHERE run_id = $1 \
                   AND ordinal >= $2 \
                   AND consumed_signal_id IS NOT NULL \
            )", signals = tables.signals, steps = tables.steps),
        &[&run_id, &target_ordinal],
    )
    .await
    .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    tx.execute(
        &format!("DELETE FROM {subscriptions} \
          WHERE run_id = $1 AND ordinal >= $2",
            subscriptions = tables.subscriptions
        ),
        &[&run_id, &target_ordinal],
    )
    .await
    .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    tx.execute(
        &format!("DELETE FROM {steps} \
          WHERE run_id = $1 AND ordinal >= $2",
            steps = tables.steps
        ),
        &[&run_id, &target_ordinal],
    )
    .await
    .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    tx.execute(
        &format!("UPDATE {steps} \
            SET compensation_state = 'pending', \
                compensation_attempt = 0, \
                compensation_wake_at = NULL, \
                compensation_error = NULL, \
                compensation_batch_id = NULL \
          WHERE run_id = $1 \
            AND ordinal < $2 \
            AND compensation_state = 'running'",
            steps = tables.steps
        ),
        &[&run_id, &target_ordinal],
    )
    .await
    .map_err(|e| WorkflowApiError::Database(e.to_string()))?;

    let restarted_from: Option<i32> = (!full_restart).then_some(target_ordinal);
    let restarted_by = format!("app:{app_id}");
    tx.execute(
        &format!("UPDATE {runs} \
            SET state = 'queued', \
                wake_at = now(), \
                terminal_at = NULL, \
                output = NULL, \
                error = NULL, \
                output_kind = 'inline', \
                output_hash = NULL, \
                output_size = NULL, \
                output_content_type = NULL, \
                compensation_target = NULL, \
                compensation_outcome = NULL, \
                next_ordinal = $2, \
                stuck_strikes = 0, \
                waiting_step_key = NULL, \
                paused_from_status = NULL, \
                claimed_by = NULL, \
                lease_expires = NULL, \
                dispatch_nonce = NULL, \
                deploy_id = $3, \
                signal_epoch = signal_epoch + $4, \
                restart_count = restart_count + 1, \
                restarted_at = now(), \
                restarted_from_ordinal = $5, \
                restarted_by = $6 \
          WHERE id = $1 AND app_id = $7",
            runs = tables.runs
        ),
        &[
            &run_id,
            &target_ordinal,
            &target_deploy_id,
            &signal_epoch_bump,
            &restarted_from,
            &restarted_by,
            &app_id,
        ],
    )
    .await
    .map_err(|e| WorkflowApiError::Database(e.to_string()))?;

    workflow_engine::register_run_timer_in_tx(state, &tx, run_id)
        .await
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    tx.commit()
        .await
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;

    Ok(json!({
        "runId": run_id,
        "id": run_id,
        "state": "queued",
        "restartedFromOrdinal": restarted_from,
        "pinnedTo": target_deploy_id,
    }))
}

async fn control_transition(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    run_id: Path<String>,
    op: &'static str,
    cancel_body: Option<CancelBody>,
) -> web::HttpResponse {
    let app_id = match app_id_from_channel(&req, &state) {
        Ok(app_id) => app_id,
        Err(resp) => return resp,
    };
    let run_id = run_id.into_inner();
    if let Err(e) = validate_run_id(&run_id) {
        return e.response();
    }
    let mut conn = match state.registry.conn().await {
        Ok(conn) => conn,
        Err(e) => return WorkflowApiError::from(e).response(),
    };
    let tx = match conn.transaction().await {
        Ok(tx) => tx,
        Err(e) => return WorkflowApiError::Database(e.to_string()).response(),
    };
    if op == "resume" {
        if let Err(e) = ensure_app_workflows_enabled(&tx, &app_id).await {
            return e.response();
        }
    }
    let tables = match provision_workflow_journal(&tx, &app_id).await {
        Ok(tables) => tables,
        Err(e) => return e.response(),
    };
    let rows = match tx
        .query(
            &format!("SELECT state \
               FROM {runs} \
              WHERE id = $1 AND app_id = $2 \
              FOR UPDATE", runs = tables.runs),
            &[&run_id, &app_id],
        )
        .await
    {
        Ok(rows) => rows,
        Err(e) => return WorkflowApiError::Database(e.to_string()).response(),
    };
    let Some(row) = rows.first() else {
        let _ = tx.commit().await;
        return WorkflowApiError::NotFound("workflow run not found".to_string()).response();
    };
    let current: String = row.get("state");
    let result = match op {
        "pause" => {
            if matches!(
                current.as_str(),
                "completed" | "failed" | "cancelled" | "stalled"
            ) {
                Err(WorkflowApiError::Conflict(format!(
                    "cannot pause workflow run in state {current}"
                )))
            } else {
                tx.query(
                    &format!("UPDATE {runs} \
                        SET state = 'paused', \
                            terminal_at = NULL, \
                            wake_at = NULL, \
                            paused_from_status = CASE \
                                WHEN state = 'paused' THEN paused_from_status \
                                ELSE state \
                            END, \
                            claimed_by = CASE \
                                WHEN state = 'running' AND claimed_by IS NOT NULL THEN claimed_by \
                                ELSE NULL \
                            END, \
                            lease_expires = CASE \
                                WHEN state = 'running' AND claimed_by IS NOT NULL THEN lease_expires \
                                ELSE NULL \
                            END, \
                            dispatch_nonce = CASE \
                                WHEN state = 'running' AND claimed_by IS NOT NULL THEN dispatch_nonce \
                                ELSE NULL \
                            END \
                      WHERE id = $1 AND app_id = $2 \
                      RETURNING state",
                        runs = tables.runs
                    ),
                    &[&run_id, &app_id],
                )
                .await
                .map_err(workflow_pg_error)
            }
        }
        "resume" => {
            if current != "paused" {
                Err(WorkflowApiError::Conflict(format!(
                    "cannot resume workflow run in state {current}"
                )))
            } else {
                let wake_frontier = resume_wake_frontier_sql(&tables);
                let sql = format!(
                    "UPDATE {runs} AS r \
                        SET state = {}, \
                            wake_at = CASE \
                                WHEN {} IN ('queued','running') THEN COALESCE({}, now()) \
                                WHEN {} IN ('sleeping','waiting','compensating') THEN {} \
                                ELSE NULL \
                            END, \
                            terminal_at = NULL, \
                            paused_from_status = NULL \
                      WHERE r.id = $1 AND r.app_id = $2 \
                      RETURNING state",
                    restored_state_expr(),
                    restored_state_expr(),
                    wake_frontier,
                    restored_state_expr(),
                    wake_frontier,
                    runs = tables.runs,
                );
                tx.query(&sql, &[&run_id, &app_id])
                    .await
                    .map_err(workflow_pg_error)
            }
        }
        "cancel" => {
            let mode = cancel_body
                .as_ref()
                .and_then(|body| body.mode.as_deref())
                .unwrap_or("abort");
            if !matches!(mode, "abort" | "compensate") {
                Err(WorkflowApiError::BadRequest(format!(
                    "invalid cancel mode '{mode}'"
                )))
            } else if matches!(
                current.as_str(),
                "completed" | "failed" | "cancelled" | "stalled"
            ) {
                Err(WorkflowApiError::Conflict(format!(
                    "cannot cancel workflow run in state {current}"
                )))
            } else if mode == "compensate" {
                let pending_row = match tx
                    .query_one(
                        &format!("SELECT COUNT(*)::bigint AS n \
                           FROM {steps} \
                          WHERE run_id = $1 AND compensation_state = 'pending'",
                            steps = tables.steps
                        ),
                        &[&run_id],
                    )
                    .await
                {
                    Ok(row) => row,
                    Err(e) => return workflow_pg_error(e).response(),
                };
                let pending = pending_row.get::<_, i64>("n");
                if pending > 0 {
                    let progress = match tx
                        .query_one(
                            &format!("SELECT \
                                COUNT(*) FILTER (WHERE compensation_state IS NOT NULL)::bigint AS total, \
                                COUNT(*) FILTER (WHERE compensation_state = 'completed')::bigint AS completed, \
                                COUNT(*) FILTER (WHERE compensation_state = 'failed')::bigint AS failed \
                               FROM {steps} \
                              WHERE run_id = $1",
                                steps = tables.steps
                            ),
                            &[&run_id],
                        )
                        .await
                    {
                        Ok(row) => row,
                        Err(e) => return workflow_pg_error(e).response(),
                    };
                    let error = json!({
                        "type": "ChildCancelledError",
                        "message": "child workflow was cancelled",
                        "retryable": false,
                        "compensation": {
                            "total": progress.get::<_, i64>("total"),
                            "completed": progress.get::<_, i64>("completed"),
                            "failed": progress.get::<_, i64>("failed"),
                        },
                    });
                    tx.query(
                        &format!("UPDATE {runs} \
                            SET state = 'compensating', \
                                wake_at = now(), \
                                terminal_at = NULL, \
                                output = NULL, \
                                error = $3, \
                                output_kind = 'inline', \
                                output_hash = NULL, \
                                output_size = NULL, \
                                output_content_type = NULL, \
                                compensation_target = 'cancelled', \
                                compensation_outcome = NULL, \
                                waiting_step_key = NULL, \
                                paused_from_status = NULL, \
                                claimed_by = NULL, \
                                lease_expires = NULL, \
                                dispatch_nonce = NULL \
                          WHERE id = $1 AND app_id = $2 \
                          RETURNING state",
                            runs = tables.runs
                        ),
                        &[&run_id, &app_id, &error],
                    )
                    .await
                    .map_err(workflow_pg_error)
                } else {
                    tx.query(
                        &format!("UPDATE {runs} \
                            SET state = 'cancelled', \
                                wake_at = NULL, \
                                terminal_at = now(), \
                                output = NULL, \
                                error = NULL, \
                                output_kind = 'inline', \
                                output_hash = NULL, \
                                output_size = NULL, \
                                output_content_type = NULL, \
                                paused_from_status = NULL, \
                                claimed_by = NULL, \
                                lease_expires = NULL, \
                                dispatch_nonce = NULL \
                          WHERE id = $1 AND app_id = $2 \
                          RETURNING state",
                            runs = tables.runs
                        ),
                        &[&run_id, &app_id],
                    )
                    .await
                    .map_err(workflow_pg_error)
                }
            } else {
                tx.query(
                    &format!("UPDATE {runs} \
                        SET state = 'cancelled', \
                            wake_at = NULL, \
                            terminal_at = now(), \
                            output = NULL, \
                            error = NULL, \
                            output_kind = 'inline', \
                            output_hash = NULL, \
                            output_size = NULL, \
                            output_content_type = NULL, \
                            paused_from_status = NULL, \
                            claimed_by = NULL, \
                            lease_expires = NULL, \
                            dispatch_nonce = NULL \
                      WHERE id = $1 AND app_id = $2 \
                      RETURNING state",
                        runs = tables.runs
                    ),
                    &[&run_id, &app_id],
                )
                .await
                .map_err(workflow_pg_error)
            }
        }
        _ => unreachable!(),
    };

    let rows = match result {
        Ok(rows) => rows,
        Err(e) => {
            let _ = tx.commit().await;
            return e.response();
        }
    };
    let state_value: String = rows
        .first()
        .map(|row| row.get("state"))
        .unwrap_or_else(|| current.clone());
    let mut cascade_run_ids = Vec::new();
    if op == "cancel" && state_value != "compensating" {
        match workflow_engine::cascade_cancel_children(&tx, &run_id).await {
            Ok(ids) => cascade_run_ids = ids,
            Err(e) => {
                let _ = tx.commit().await;
                return WorkflowApiError::from(e).response();
            }
        }
    }
    // Same shape as the start path, same fix: the state change and its timer
    // registration commit together, so an error response never leaves a run
    // whose durable state says "run me".
    if let Err(e) = workflow_engine::register_run_timer_in_tx(&state, &tx, &run_id).await {
        return WorkflowApiError::Database(e.to_string()).response();
    }
    for cascade_run_id in &cascade_run_ids {
        if let Err(e) = workflow_engine::register_run_timer_in_tx(&state, &tx, cascade_run_id).await
        {
            return WorkflowApiError::Database(e.to_string()).response();
        }
    }
    if let Err(e) = tx.commit().await {
        return workflow_pg_error(e).response();
    }
    web::HttpResponse::Ok().json(&json!({ "state": state_value }))
}

pub async fn list_runs(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    query: Query<ListRunsQuery>,
) -> web::HttpResponse {
    let app_id = match app_id_from_channel(&req, &state) {
        Ok(app_id) => app_id,
        Err(resp) => return resp,
    };
    let state_filter = query.state.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty());
    if let Some(state) = state_filter {
        if !matches!(
            state,
            "queued"
                | "running"
                | "sleeping"
                | "waiting"
                | "paused"
                | "stalled"
                | "compensating"
                | "completed"
                | "failed"
                | "cancelled"
        ) {
            return WorkflowApiError::BadRequest("invalid state filter".to_string()).response();
        }
    }
    let workflow_filter = query
        .workflow_name
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let offset = query.offset.unwrap_or(0).max(0);
    let state_param = state_filter.map(str::to_string);
    let workflow_param = workflow_filter.map(str::to_string);
    let tables = WorkflowTables::for_app_id(&app_id);
    let sql = format!(
        "SELECT id, workflow_name, state, created_at::text \
               FROM {runs} \
              WHERE app_id = $1 \
                AND ($2::text IS NULL OR state = $2) \
                AND ($3::text IS NULL OR workflow_name = $3) \
              ORDER BY created_at DESC, id DESC \
              LIMIT $4 OFFSET $5",
        runs = tables.runs
    );
    let rows = match state
        .control_pg
        .query(&sql, &[&app_id, &state_param, &workflow_param, &limit, &offset])
        .await
    {
        Ok(rows) => rows,
        Err(e) => return WorkflowApiError::Database(e.to_string()).response(),
    };
    let runs: Vec<RunListItem> = rows
        .iter()
        .map(|row| RunListItem {
            id: row.get("id"),
            workflow_name: row.get("workflow_name"),
            state: row.get("state"),
            created_at: row.get("created_at"),
        })
        .collect();
    web::HttpResponse::Ok().json(&json!({
        "runs": runs,
        "limit": limit,
        "offset": offset,
    }))
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/internal/workflows/{workflow_name}/runs")
            .route(web::post().to(create_run)),
    )
    .service(
        web::resource("/internal/workflows/{workflow_name}/runs/startMany")
            .state(web::types::PayloadConfig::new(SIGNAL_REQUEST_BODY_BYTES))
            .route(web::post().to(start_many_runs)),
    )
    .service(web::resource("/internal/workflows/runs").route(web::get().to(list_runs)))
    .service(
        web::resource("/internal/workflows/runs/{run_id}")
            .route(web::get().to(get_run_status)),
    )
    .service(
        web::resource("/internal/workflows/runs/{run_id}/output")
            .route(web::get().to(get_run_output)),
    )
    .service(
        web::resource("/internal/workflows/runs/{run_id}/steps/{name}/output")
            .route(web::get().to(get_step_output)),
    )
    .service(
        web::resource("/internal/workflows/runs/{run_id}/signal")
            .state(web::types::PayloadConfig::new(SIGNAL_REQUEST_BODY_BYTES))
            .route(web::post().to(signal_run)),
    )
    .service(
        web::resource("/internal/workflows/runs/{run_id}/signal-token")
            .state(web::types::PayloadConfig::new(SIGNAL_REQUEST_BODY_BYTES))
            .route(web::post().to(create_run_signal_token)),
    )
    .service(
        web::resource("/internal/workflows/topics/{topic}/signal-token")
            .state(web::types::PayloadConfig::new(SIGNAL_REQUEST_BODY_BYTES))
            .route(web::post().to(create_topic_signal_token)),
    )
    .service(
        web::resource("/internal/workflows/topics/{topic}/broadcast")
            .state(web::types::PayloadConfig::new(SIGNAL_REQUEST_BODY_BYTES))
            .route(web::post().to(publish_topic_signal)),
    )
    .service(
        web::resource("/internal/workflows/signals/ingress")
            .state(web::types::PayloadConfig::new(SIGNAL_REQUEST_BODY_BYTES))
            .route(web::post().to(ingress_signal)),
    )
    .service(
        web::resource("/internal/workflows/signals/fanout/tick")
            .route(web::post().to(force_signal_fanout_tick)),
    )
    .service(
        web::resource("/internal/workflows/runs/{run_id}/pause")
            .route(web::post().to(pause_run)),
    )
    .service(
        web::resource("/internal/workflows/runs/{run_id}/resume")
            .route(web::post().to(resume_run)),
    )
    .service(
        web::resource("/internal/workflows/runs/{run_id}/cancel")
            .route(web::post().to(cancel_run)),
    )
    .service(
        web::resource("/internal/workflows/runs/{run_id}/restart")
            .route(web::post().to(restart_run)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::infrastructure_error_test_support::{assert_logged_trace_id, capture};
    use ntex::util::{stream_recv, BytesMut};

    async fn body_json(mut resp: web::HttpResponse) -> Value {
        let mut body = resp.take_body();
        let mut buf = BytesMut::new();
        while let Some(item) = stream_recv(&mut body).await {
            buf.extend_from_slice(&item.expect("body chunk"));
        }
        serde_json::from_slice(&buf).expect("body is JSON")
    }

    #[compio::test]
    async fn database_error_response_carries_trace_id() {
        let (resp, events) = capture(|| {
            WorkflowApiError::Database(
                "postgres://operator-secret/workflows is unavailable".to_string(),
            )
            .response()
        });
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let body = body_json(resp).await;
        assert_eq!(
            body.get("error").and_then(Value::as_str),
            Some("internal error")
        );
        let trace_id = body
            .get("trace_id")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("workflow infrastructure body must carry trace_id: {body}"));
        Uuid::parse_str(trace_id)
            .unwrap_or_else(|e| panic!("trace_id must be a UUID ({trace_id}): {e}"));
        assert_logged_trace_id(&events, trace_id);
        assert_eq!(
            body.as_object().map(serde_json::Map::len),
            Some(2),
            "body must carry only error and trace_id: {body}"
        );
        assert!(
            !body.to_string().contains("operator-secret"),
            "the database detail must stay out of the body: {body}"
        );
    }

    #[test]
    fn manifest_workflow_shape_accepts_array_and_map() {
        assert!(
            manifest_declares_workflow(r#"{"workflows":["Checkout"]}"#, "Checkout").unwrap()
        );
        assert!(
            manifest_declares_workflow(r#"{"workflows":{"Checkout":{"concurrency":1}}}"#, "Checkout")
                .unwrap()
        );
        assert!(
            !manifest_declares_workflow(r#"{"workflows":["Other"]}"#, "Checkout").unwrap()
        );
    }

    #[test]
    fn waiting_key_match_uses_signal_type_slot() {
        assert!(waiting_key_matches_signal(
            Some("wait:7:approved:payment.succeeded:60000"),
            "payment.succeeded"
        ));
        assert!(!waiting_key_matches_signal(
            Some("wait:7:approved:payment.failed"),
            "payment.succeeded"
        ));
    }

    #[test]
    fn token_duration_parser_accepts_iso_and_suffix_forms() {
        assert_eq!(parse_duration_secs("PT5M").unwrap(), 300);
        assert_eq!(parse_duration_secs("2.5s").unwrap(), 3);
        assert_eq!(parse_duration_secs("1500ms").unwrap(), 2);
        assert!(parse_duration_secs("PT0S").is_err());
        assert!(validate_token_ttl("P2D").is_err());
    }

    #[test]
    fn signal_token_time_enforces_expiry_with_tolerance() {
        let claims = typed_id::WorkflowSignalTokenClaims {
            app_id: Uuid::nil().to_string(),
            run_id: Some("run_abc".to_string()),
            topic: None,
            types: vec!["go".to_string()],
            exp: 100,
            epoch: 0,
        };
        assert!(validate_signal_token_time(&claims, 101).is_ok());
        assert!(validate_signal_token_time(&claims, 102).is_err());
    }

    #[test]
    fn ingress_signal_type_must_be_authorized() {
        let claims = typed_id::WorkflowSignalTokenClaims {
            app_id: Uuid::nil().to_string(),
            run_id: Some("run_abc".to_string()),
            topic: None,
            types: vec!["go".to_string()],
            exp: 100,
            epoch: 0,
        };
        assert_eq!(resolve_ingress_signal_type(&claims, None).unwrap(), "go");
        assert!(resolve_ingress_signal_type(&claims, Some("stop".to_string())).is_err());
        assert!(resolve_ingress_signal_type(&claims, Some("__zs.stop".to_string())).is_err());
    }

    #[test]
    fn workflow_pg_error_marks_deadlocks_retryable() {
        let error = workflow_pg_error_from_parts(
            Some(&SqlState::T_R_DEADLOCK_DETECTED),
            "deadlock detected".to_string(),
        );
        assert!(matches!(
            error,
            WorkflowApiError::Database(msg)
                if msg.starts_with("retryable deadlock:") && msg.contains("deadlock detected")
        ));
    }

    #[test]
    fn token_replay_key_is_stable_and_non_plaintext() {
        let key_a = signal_token_replay_key("wst_a.b");
        let key_b = signal_token_replay_key("wst_a.b");
        let key_c = signal_token_replay_key("wst_a.c");
        assert_eq!(key_a, key_b);
        assert_ne!(key_a, key_c);
        assert!(key_a.starts_with(SIGNAL_TOKEN_REPLAY_PREFIX));
        assert!(!key_a.contains("wst_a.b"));
    }
}
