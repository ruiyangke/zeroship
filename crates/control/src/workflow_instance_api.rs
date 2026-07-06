//! Internal durable-workflow instance API.
//!
//! These routes are called by the runtime-side `env.workflows` binding and by
//! creator tooling that already holds the internal control credential. The app
//! scope is not request-body data: every handler derives it from the
//! authenticated channel header and binds `app_id` in every journal query.

use std::sync::Arc;

use ntex::http::StatusCode;
use ntex::web;
use ntex::web::types::{Json, Path, Query, State};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_auth::ratelimit::{self, Bucket, RateLimitDecision};
use zeroship_core::typed_id;

use crate::registry::RegistryError;
use crate::AppState;

pub const APP_ID_HEADER: &str = "x-zeroship-app-id";
pub const ALT_APP_ID_HEADER: &str = "zeroship-app-id";
pub const SIGNAL_REQUEST_BODY_BYTES: usize = 128 * 1024;
pub const SIGNAL_PAYLOAD_BYTES: usize = 64 * 1024;

const SIGNAL_RATE_LIMIT_CAPACITY: f64 = 60.0;
const SIGNAL_RATE_LIMIT_REFILL_PER_SEC: f64 = 60.0 / 60.0;

#[derive(Debug, Deserialize)]
pub struct CreateRunBody {
    pub input: Value,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default, alias = "onConflict")]
    pub on_conflict: Option<OnConflictBody>,
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

#[derive(Debug)]
struct ActiveDeploy {
    id: String,
    manifest_json: String,
}

#[derive(Debug)]
enum WorkflowApiError {
    BadRequest(String),
    NotFound(String),
    Conflict(String),
    PayloadTooLarge(String),
    RateLimited { retry_after_secs: f64 },
    RateLimitUnavailable(String),
    Database(String),
}

impl WorkflowApiError {
    fn response(self) -> web::HttpResponse {
        match self {
            Self::BadRequest(msg) => {
                web::HttpResponse::BadRequest().json(&json!({ "error": msg }))
            }
            Self::NotFound(msg) => web::HttpResponse::NotFound().json(&json!({ "error": msg })),
            Self::Conflict(msg) => web::HttpResponse::Conflict().json(&json!({
                "error": "RunConflict",
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
            Self::Database(msg) => infrastructure_error_response("workflow instance API", msg),
        }
    }
}

impl From<RegistryError> for WorkflowApiError {
    fn from(value: RegistryError) -> Self {
        match value {
            RegistryError::InvalidInput(msg) => Self::BadRequest(msg),
            RegistryError::NotFound(msg) => Self::NotFound(msg),
            RegistryError::AlreadyExists(msg) | RegistryError::Conflict(msg) => Self::Conflict(msg),
            RegistryError::Database(msg) => Self::Database(msg),
            RegistryError::FxUnresolved => Self::Database(value.to_string()),
        }
    }
}

fn infrastructure_error_response(
    context: &'static str,
    detail: impl std::fmt::Display,
) -> web::HttpResponse {
    let request_id = Uuid::new_v4();
    tracing::error!(
        request_id = %request_id,
        context,
        error = %detail,
        "control-plane infrastructure error"
    );
    web::HttpResponse::InternalServerError().json(&json!({ "error": "internal error" }))
}

fn check_control_auth(req: &web::HttpRequest, state: &AppState) -> Result<(), web::HttpResponse> {
    if state.insecure_dev {
        return Ok(());
    }
    let header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let token = zeroship_core::auth::extract_bearer(header);
    match token {
        Some(key)
            if !state.control_key.is_empty()
                && zeroship_core::auth::validate_control_key(
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
                "workflow instance API: auth rejected"
            );
            Err(web::HttpResponse::Unauthorized().json(&json!({ "error": "unauthorized" })))
        }
    }
}

fn app_id_from_channel(
    req: &web::HttpRequest,
    state: &AppState,
) -> Result<Uuid, web::HttpResponse> {
    check_control_auth(req, state)?;
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

    parse_app_id(raw).map_err(|msg| web::HttpResponse::BadRequest().json(&json!({ "error": msg })))
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
    "CASE \
        WHEN waiting_step_key LIKE 'wait:%' THEN 'waiting' \
        WHEN waiting_step_key LIKE 'sleep:%' THEN 'sleeping' \
        WHEN wake_at IS NULL OR wake_at <= now() THEN 'queued' \
        ELSE 'sleeping' \
     END"
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
        format!("{:.0}", secs.ceil().max(1.0).min(3600.0))
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
    let rows = conn
        .query(
            "SELECT id, manifest_json \
               FROM zeroship.app_deploys \
              WHERE app_id = $1 \
                AND activated_at IS NOT NULL \
              ORDER BY activated_at DESC, created_at DESC, id DESC \
              LIMIT 1",
            &[app_id],
        )
        .await
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
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

async fn insert_run<C>(
    conn: &C,
    app_id: &Uuid,
    workflow_name: &str,
    deploy_id: &str,
    input: &Value,
    dedup_key: Option<&String>,
    run_id: &str,
) -> Result<(), WorkflowApiError>
where
    C: compio_postgres::GenericClient + Sync,
{
    conn.execute(
        "INSERT INTO zeroship.workflow_runs \
            (id, workflow_name, app_id, deploy_id, state, input, dedup_key, wake_at, started_at) \
         VALUES ($1, $2, $3, $4, 'queued', $5, $6, now(), now())",
        &[
            &run_id,
            &workflow_name,
            app_id,
            &deploy_id,
            input,
            &dedup_key,
        ],
    )
    .await
    .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    Ok(())
}

async fn insert_run_on_conflict_do_nothing<C>(
    conn: &C,
    app_id: &Uuid,
    workflow_name: &str,
    deploy_id: &str,
    input: &Value,
    dedup_key: &String,
    run_id: &str,
) -> Result<Option<String>, WorkflowApiError>
where
    C: compio_postgres::GenericClient + Sync,
{
    let rows = conn
        .query(
            "INSERT INTO zeroship.workflow_runs \
                (id, workflow_name, app_id, deploy_id, state, input, dedup_key, wake_at, started_at) \
             VALUES ($1, $2, $3, $4, 'queued', $5, $6, now(), now()) \
             ON CONFLICT (app_id, workflow_name, dedup_key) DO NOTHING \
             RETURNING id",
            &[
                &run_id,
                &workflow_name,
                app_id,
                &deploy_id,
                input,
                &dedup_key,
            ],
        )
        .await
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    Ok(rows.first().map(|row| row.get("id")))
}

async fn existing_keyed_run<C>(
    conn: &C,
    app_id: &Uuid,
    workflow_name: &str,
    dedup_key: &String,
) -> Result<Option<String>, WorkflowApiError>
where
    C: compio_postgres::GenericClient + Sync,
{
    let rows = conn
        .query(
            "SELECT id \
               FROM zeroship.workflow_runs \
              WHERE app_id = $1 AND workflow_name = $2 AND dedup_key = $3 \
              LIMIT 1",
            &[app_id, &workflow_name, &dedup_key],
        )
        .await
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    Ok(rows.first().map(|row| row.get("id")))
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
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;

    let deploy = active_deploy_for_workflow(&tx, &app_id, &workflow_name).await?;

    let run_id = if let Some(key) = dedup_key.as_ref() {
        match policy {
            ConflictPolicy::Join => {
                let candidate = typed_id::new_workflow_run_id();
                if let Some(inserted) = insert_run_on_conflict_do_nothing(
                    &tx,
                    &app_id,
                    &workflow_name,
                    &deploy.id,
                    &body.input,
                    key,
                    &candidate,
                )
                .await?
                {
                    inserted
                } else {
                    existing_keyed_run(&tx, &app_id, &workflow_name, key)
                        .await?
                        .ok_or_else(|| {
                            WorkflowApiError::Database(
                                "workflow start conflict lost its incumbent".to_string(),
                            )
                        })?
                }
            }
            ConflictPolicy::Reject => {
                let candidate = typed_id::new_workflow_run_id();
                if let Some(inserted) = insert_run_on_conflict_do_nothing(
                    &tx,
                    &app_id,
                    &workflow_name,
                    &deploy.id,
                    &body.input,
                    key,
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
                tx.execute(
                    "UPDATE zeroship.workflow_runs \
                        SET state = 'cancelled', \
                            dedup_key = NULL, \
                            wake_at = NULL, \
                            output = NULL, \
                            error = NULL, \
                            output_kind = 'inline', \
                            output_hash = NULL, \
                            output_size = NULL, \
                            output_content_type = NULL, \
                            claimed_by = NULL, \
                            lease_expires = NULL, \
                            dispatch_nonce = NULL, \
                            claim_epoch = claim_epoch + 1 \
                      WHERE app_id = $1 AND workflow_name = $2 AND dedup_key = $3",
                    &[&app_id, &workflow_name, key],
                )
                .await
                .map_err(|e| WorkflowApiError::Database(e.to_string()))?;

                let candidate = typed_id::new_workflow_run_id();
                if let Some(inserted) = insert_run_on_conflict_do_nothing(
                    &tx,
                    &app_id,
                    &workflow_name,
                    &deploy.id,
                    &body.input,
                    key,
                    &candidate,
                )
                .await?
                {
                    inserted
                } else {
                    existing_keyed_run(&tx, &app_id, &workflow_name, key)
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
        let candidate = typed_id::new_workflow_run_id();
        insert_run(
            &tx,
            &app_id,
            &workflow_name,
            &deploy.id,
            &body.input,
            None,
            &candidate,
        )
        .await?;
        candidate
    };

    tx.commit()
        .await
        .map_err(|e| WorkflowApiError::Database(e.to_string()))?;
    Ok((StatusCode::CREATED, json!({ "id": run_id, "state": "queued" })))
}

async fn consume_signal_rate_limit(
    state: &AppState,
    app_id: Uuid,
) -> Result<(), WorkflowApiError> {
    let key = format!("control:workflow_signal:app:{app_id}");
    let bucket = Bucket {
        capacity: SIGNAL_RATE_LIMIT_CAPACITY,
        refill_per_sec: SIGNAL_RATE_LIMIT_REFILL_PER_SEC,
    };
    match ratelimit::consume_or_throttle(&state.control_pg, &key, bucket).await {
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
    let rows = match state
        .control_pg
        .query(
            "SELECT state, output, error, output_kind, output_hash, output_size, output_content_type \
               FROM zeroship.workflow_runs \
              WHERE id = $1 AND app_id = $2",
            &[&run_id, &app_id],
        )
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
    if let Err(e) = validate_signal_type(&body.signal_type) {
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
        Err(e) => return WorkflowApiError::Database(e.to_string()).response(),
    };
    let rows = match tx
        .query(
            "SELECT state, waiting_step_key \
               FROM zeroship.workflow_runs \
              WHERE id = $1 AND app_id = $2 \
              FOR UPDATE",
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
    let run_state: String = row.get("state");
    let waiting_step_key: Option<String> = row.get("waiting_step_key");
    let signal_id = typed_id::new_workflow_signal_id();
    if let Err(e) = tx
        .execute(
            "INSERT INTO zeroship.workflow_signals \
                (id, run_id, type, payload, origin, delivery) \
             VALUES ($1, $2, $3, $4, 'app', 'direct')",
            &[&signal_id, &run_id, &body.signal_type, &body.payload],
        )
        .await
    {
        return WorkflowApiError::Database(e.to_string()).response();
    }
    if run_state == "waiting"
        && waiting_key_matches_signal(waiting_step_key.as_deref(), &body.signal_type)
    {
        if let Err(e) = tx
            .execute(
                "UPDATE zeroship.workflow_runs \
                    SET wake_at = now() \
                  WHERE id = $1 AND app_id = $2",
                &[&run_id, &app_id],
            )
            .await
        {
            return WorkflowApiError::Database(e.to_string()).response();
        }
    }
    if let Err(e) = tx.commit().await {
        return WorkflowApiError::Database(e.to_string()).response();
    }
    web::HttpResponse::Accepted().json(&json!({ "id": signal_id }))
}

pub async fn pause_run(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    run_id: Path<String>,
) -> web::HttpResponse {
    control_transition(req, state, run_id, "pause").await
}

pub async fn resume_run(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    run_id: Path<String>,
) -> web::HttpResponse {
    control_transition(req, state, run_id, "resume").await
}

pub async fn cancel_run(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    run_id: Path<String>,
) -> web::HttpResponse {
    control_transition(req, state, run_id, "cancel").await
}

async fn control_transition(
    req: web::HttpRequest,
    state: State<Arc<AppState>>,
    run_id: Path<String>,
    op: &'static str,
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
    let rows = match tx
        .query(
            "SELECT state \
               FROM zeroship.workflow_runs \
              WHERE id = $1 AND app_id = $2 \
              FOR UPDATE",
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
                    "UPDATE zeroship.workflow_runs \
                        SET state = 'paused', \
                            claimed_by = NULL, \
                            lease_expires = NULL, \
                            dispatch_nonce = NULL, \
                            claim_epoch = claim_epoch + 1 \
                      WHERE id = $1 AND app_id = $2 \
                      RETURNING state",
                    &[&run_id, &app_id],
                )
                .await
                .map_err(|e| WorkflowApiError::Database(e.to_string()))
            }
        }
        "resume" => {
            if current != "paused" {
                Err(WorkflowApiError::Conflict(format!(
                    "cannot resume workflow run in state {current}"
                )))
            } else {
                let sql = format!(
                    "UPDATE zeroship.workflow_runs \
                        SET state = {}, \
                            claimed_by = NULL, \
                            lease_expires = NULL, \
                            dispatch_nonce = NULL \
                      WHERE id = $1 AND app_id = $2 \
                      RETURNING state",
                    restored_state_expr()
                );
                tx.query(&sql, &[&run_id, &app_id])
                    .await
                    .map_err(|e| WorkflowApiError::Database(e.to_string()))
            }
        }
        "cancel" => {
            if matches!(
                current.as_str(),
                "completed" | "failed" | "cancelled" | "stalled"
            ) {
                Err(WorkflowApiError::Conflict(format!(
                    "cannot cancel workflow run in state {current}"
                )))
            } else {
                tx.query(
                    "UPDATE zeroship.workflow_runs \
                        SET state = 'cancelled', \
                            wake_at = NULL, \
                            output = NULL, \
                            error = NULL, \
                            output_kind = 'inline', \
                            output_hash = NULL, \
                            output_size = NULL, \
                            output_content_type = NULL, \
                            claimed_by = NULL, \
                            lease_expires = NULL, \
                            dispatch_nonce = NULL, \
                            claim_epoch = claim_epoch + 1 \
                      WHERE id = $1 AND app_id = $2 \
                      RETURNING state",
                    &[&run_id, &app_id],
                )
                .await
                .map_err(|e| WorkflowApiError::Database(e.to_string()))
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
    if let Err(e) = tx.commit().await {
        return WorkflowApiError::Database(e.to_string()).response();
    }
    let state_value: String = rows
        .first()
        .map(|row| row.get("state"))
        .unwrap_or_else(|| current.clone());
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
    let rows = match state
        .control_pg
        .query(
            "SELECT id, workflow_name, state, created_at::text \
               FROM zeroship.workflow_runs \
              WHERE app_id = $1 \
                AND ($2::text IS NULL OR state = $2) \
                AND ($3::text IS NULL OR workflow_name = $3) \
              ORDER BY created_at DESC, id DESC \
              LIMIT $4 OFFSET $5",
            &[&app_id, &state_param, &workflow_param, &limit, &offset],
        )
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
    .service(web::resource("/internal/workflows/runs").route(web::get().to(list_runs)))
    .service(
        web::resource("/internal/workflows/runs/{run_id}")
            .route(web::get().to(get_run_status)),
    )
    .service(
        web::resource("/internal/workflows/runs/{run_id}/signal")
            .state(web::types::PayloadConfig::new(SIGNAL_REQUEST_BODY_BYTES))
            .route(web::post().to(signal_run)),
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
    );
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
