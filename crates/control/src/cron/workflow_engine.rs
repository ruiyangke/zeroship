//! Durable-workflow engine scheduler.
//!
//! This cron owns only the control-plane scheduling core: due-run claiming,
//! lease heartbeats, the dispatch seam, and the idempotent outcome apply txn.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use compio_postgres::error::SqlState;
use compio_postgres::GenericClient;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
use zeroship_core::typed_id;

use crate::registry::RegistryError;
use crate::{AppState, Registry};

/// Default tick cadence. Workflow wake latency is intentionally a scheduler
/// knob, not a correctness bound; DW-23 will measure and tune it.
pub const DEFAULT_TICK_SECS: u64 = 1;
const GATEWAY_WORKFLOW_DISPATCH_PATH: &str = "/__zeroship/internal/workflow-dispatch";
const GATEWAY_DISPATCH_TIMEOUT: Duration = Duration::from_secs(35);
const BACKPRESSURE_PARK_MS: i64 = 1_000;

static INFLIGHT_DISPATCHES: AtomicUsize = AtomicUsize::new(0);
static OWNER_ID: OnceLock<String> = OnceLock::new();

fn default_owner_id() -> String {
    OWNER_ID
        .get_or_init(|| format!("control-wf-{}", std::process::id()))
        .clone()
}

#[derive(Debug, Clone)]
pub struct WorkflowEngineConfig {
    /// Number of distinct apps considered in one tick.
    pub batch_apps: i64,
    /// Max due rows claimed per app in one tick.
    pub per_app_fair_limit: i64,
    /// Per-app live dispatch cap. Counts only fresh `running` + `claimed_by`
    /// rows, never queued/sleeping/waiting rows.
    pub max_inflight_per_app: i64,
    /// Process-local detached dispatch cap.
    pub max_inflight_dispatch: usize,
    /// Lease staleness duration in milliseconds.
    pub claim_ttl_ms: i64,
    /// Heartbeat cadence while a detached dispatch is in flight.
    pub heartbeat_ms: u64,
    /// Stable owner id written into `claimed_by`.
    pub owner_id: String,
}

impl Default for WorkflowEngineConfig {
    fn default() -> Self {
        Self {
            batch_apps: 32,
            per_app_fair_limit: 4,
            max_inflight_per_app: 16,
            max_inflight_dispatch: 64,
            claim_ttl_ms: 120_000,
            heartbeat_ms: 5_000,
            owner_id: default_owner_id(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalStep {
    pub ordinal: i32,
    pub name: String,
    pub kind: String,
    pub state: String,
    pub output: Option<Value>,
    pub error: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepRequest {
    pub run_id: String,
    pub app_id: Uuid,
    pub workflow_name: String,
    pub deploy_id: String,
    pub deploy_hash: String,
    pub dispatch_nonce: String,
    #[serde(default)]
    pub input: Option<Value>,
    pub started_at: DateTime<Utc>,
    pub journal: Vec<JournalStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepCheckpoint {
    pub ordinal: i32,
    pub name: String,
    pub name_occurrence: i32,
    pub kind: String,
    pub state: String,
    pub output: Option<Value>,
    pub error: Option<Value>,
    pub wake_at: Option<DateTime<Utc>>,
    pub signal_type: Option<String>,
    pub max_signal_age_ms: Option<i64>,
    pub consumed_signal_id: Option<String>,
}

impl StepCheckpoint {
    #[must_use]
    pub fn completed_run(ordinal: i32, name: impl Into<String>, output: Value) -> Self {
        Self {
            ordinal,
            name: name.into(),
            name_occurrence: 0,
            kind: "run".to_string(),
            state: "completed".to_string(),
            output: Some(output),
            error: None,
            wake_at: None,
            signal_type: None,
            max_signal_age_ms: None,
            consumed_signal_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum RunUpdate {
    Queued,
    Sleeping {
        #[serde(rename = "wakeAt")]
        wake_at: Option<DateTime<Utc>>,
    },
    Waiting {
        #[serde(rename = "wakeAt")]
        wake_at: Option<DateTime<Utc>>,
    },
    Completed { output: Option<Value> },
    Failed { error: Value },
    Cancelled,
}

impl RunUpdate {
    fn state(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Sleeping { .. } => "sleeping",
            Self::Waiting { .. } => "waiting",
            Self::Completed { .. } => "completed",
            Self::Failed { .. } => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn wake_at(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Queued => Some(Utc::now()),
            Self::Sleeping { wake_at } | Self::Waiting { wake_at } => *wake_at,
            _ => None,
        }
    }

    fn output(&self) -> Option<Value> {
        match self {
            Self::Completed { output } => output.clone(),
            _ => None,
        }
    }

    fn error(&self) -> Option<Value> {
        match self {
            Self::Failed { error } => Some(error.clone()),
            _ => None,
        }
    }

    fn waiting_step_key(&self, checkpoints: &[StepCheckpoint]) -> Option<String> {
        match self {
            Self::Sleeping { .. } => checkpoints
                .iter()
                .find(|s| s.kind == "sleep" && s.state == "running")
                .map(|s| format!("sleep:{}:{}", s.ordinal, s.name)),
            Self::Waiting { .. } => checkpoints
                .iter()
                .find(|s| s.kind == "wait_signal" && s.state == "running")
                .map(|s| {
                    format!(
                        "wait:{}:{}:{}{}",
                        s.ordinal,
                        s.name,
                        s.signal_type.as_deref().unwrap_or(s.name.as_str()),
                        s.max_signal_age_ms
                            .map(|age| format!(":{age}"))
                            .unwrap_or_default()
                    )
                }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepResult {
    pub run_id: String,
    pub dispatch_nonce: String,
    pub checkpoints: Vec<StepCheckpoint>,
    pub run_update: RunUpdate,
}

impl StepResult {
    #[must_use]
    pub fn requeue(run_id: String, dispatch_nonce: String) -> Self {
        Self {
            run_id,
            dispatch_nonce,
            checkpoints: Vec::new(),
            run_update: RunUpdate::Queued,
        }
    }
}

#[async_trait(?Send)]
pub trait StepDispatcher: Send + Sync {
    async fn dispatch(&self, request: StepRequest) -> DispatchOutcome;
}

#[derive(Debug, Clone)]
pub enum DispatchOutcome {
    Completed(StepResult),
    Backpressure {
        run_id: String,
        dispatch_nonce: String,
        reason: String,
    },
}

impl DispatchOutcome {
    fn backpressure(request: &StepRequest, reason: impl Into<String>) -> Self {
        Self::Backpressure {
            run_id: request.run_id.clone(),
            dispatch_nonce: request.dispatch_nonce.clone(),
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Default)]
pub struct StubStepDispatcher;

#[async_trait(?Send)]
impl StepDispatcher for StubStepDispatcher {
    async fn dispatch(&self, request: StepRequest) -> DispatchOutcome {
        DispatchOutcome::Completed(StepResult::requeue(request.run_id, request.dispatch_nonce))
    }
}

#[derive(Debug, Clone)]
pub struct GatewayStepDispatcher {
    gateway_url: String,
}

impl GatewayStepDispatcher {
    #[must_use]
    pub fn new(gateway_url: impl Into<String>) -> Self {
        Self {
            gateway_url: gateway_url.into().trim_end_matches('/').to_string(),
        }
    }
}

#[async_trait(?Send)]
impl StepDispatcher for GatewayStepDispatcher {
    async fn dispatch(&self, request: StepRequest) -> DispatchOutcome {
        if self.gateway_url.is_empty() {
            return DispatchOutcome::backpressure(&request, "gateway URL is not configured");
        }
        let body = match serde_json::to_vec(&request) {
            Ok(body) => body,
            Err(e) => {
                return DispatchOutcome::backpressure(
                    &request,
                    format!("serialize StepRequest: {e}"),
                );
            }
        };
        let url = format!("{}{}", self.gateway_url, GATEWAY_WORKFLOW_DISPATCH_PATH);
        let client = cyper::Client::new();
        let builder = match client.post(&url) {
            Ok(builder) => builder,
            Err(e) => {
                return DispatchOutcome::backpressure(
                    &request,
                    format!("build gateway workflow dispatch request: {e}"),
                );
            }
        };
        let builder = match builder.header("content-type", "application/json") {
            Ok(builder) => builder,
            Err(e) => {
                return DispatchOutcome::backpressure(
                    &request,
                    format!("set gateway workflow dispatch content-type: {e}"),
                );
            }
        };
        let response = match compio::time::timeout(
            GATEWAY_DISPATCH_TIMEOUT,
            builder.body(body).send(),
        )
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(e)) => {
                return DispatchOutcome::backpressure(
                    &request,
                    format!("gateway workflow dispatch transport: {e}"),
                );
            }
            Err(_) => {
                return DispatchOutcome::backpressure(
                    &request,
                    "gateway workflow dispatch timeout",
                );
            }
        };

        let status = response.status().as_u16();
        let bytes = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(e) => {
                return DispatchOutcome::backpressure(
                    &request,
                    format!("read gateway workflow dispatch body: {e}"),
                );
            }
        };

        if status == 402 || status >= 500 {
            let body_snippet: String = String::from_utf8_lossy(&bytes).chars().take(200).collect();
            return DispatchOutcome::backpressure(
                &request,
                format!("gateway workflow dispatch HTTP {status}: {body_snippet}"),
            );
        }
        if !(200..300).contains(&status) {
            let body_snippet: String = String::from_utf8_lossy(&bytes).chars().take(200).collect();
            return DispatchOutcome::backpressure(
                &request,
                format!("gateway workflow dispatch rejected HTTP {status}: {body_snippet}"),
            );
        }

        match serde_json::from_slice::<StepResult>(&bytes) {
            Ok(result) => DispatchOutcome::Completed(result),
            Err(e) => DispatchOutcome::backpressure(
                &request,
                format!("parse gateway StepResult: {e}"),
            ),
        }
    }
}

/// Cron entry point. Loops forever; one deterministic [`tick`] per cadence.
#[allow(clippy::future_not_send)]
pub async fn run(state: Arc<AppState>, tick_secs: u64) {
    tracing::info!(tick_secs, "control workflow_engine cron starting");
    loop {
        match tick(&state).await {
            Ok(n) if n > 0 => tracing::info!(claimed = n, "workflow_engine tick claimed runs"),
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "workflow_engine tick failed"),
        }
        compio::time::sleep(Duration::from_secs(tick_secs)).await;
    }
}

/// Run one claim/dispatch scheduler pass. Public for deterministic tests.
#[allow(clippy::future_not_send)]
pub async fn tick(state: &AppState) -> Result<usize, RegistryError> {
    tick_with_dispatcher(
        state,
        Arc::new(GatewayStepDispatcher::new(state.gateway_url.clone())),
        WorkflowEngineConfig::default(),
    )
    .await
}

/// Testable tick variant with an injected dispatch seam.
#[allow(clippy::future_not_send)]
pub async fn tick_with_dispatcher<D>(
    state: &AppState,
    dispatcher: Arc<D>,
    config: WorkflowEngineConfig,
) -> Result<usize, RegistryError>
where
    D: StepDispatcher + 'static,
{
    let current = INFLIGHT_DISPATCHES.load(Ordering::SeqCst);
    if current >= config.max_inflight_dispatch {
        return Ok(0);
    }
    let available = config.max_inflight_dispatch - current;
    let claims = claim_due_batch(&state.registry, &config, available).await?;
    let claimed = claims.len();
    for claim in claims {
        spawn_dispatch(state.registry.clone(), dispatcher.clone(), config.clone(), claim);
    }
    Ok(claimed)
}

#[derive(Debug, Clone)]
struct ClaimedRun {
    request: StepRequest,
}

#[derive(Debug, Clone)]
struct CandidateRun {
    run_id: String,
    app_id: Uuid,
    workflow_name: String,
    deploy_id: String,
    deploy_hash: String,
    input: Option<Value>,
    started_at: DateTime<Utc>,
    waiting_step_key: Option<String>,
}

async fn claim_due_batch(
    registry: &Registry,
    config: &WorkflowEngineConfig,
    available: usize,
) -> Result<Vec<ClaimedRun>, RegistryError> {
    let mut conn = registry.conn().await?;
    let tx = conn.transaction().await.map_err(RegistryError::from)?;
    let limit = i64::try_from(available)
        .unwrap_or(i64::MAX)
        .min(config.batch_apps.saturating_mul(config.per_app_fair_limit));
    if limit <= 0 {
        tx.commit().await.map_err(RegistryError::from)?;
        return Ok(Vec::new());
    }

    let rows = tx
        .query(
            "WITH due_apps AS ( \
               SELECT app_id, MIN(wake_at) AS first_wake \
                 FROM zeroship.workflow_runs \
                WHERE wake_at <= now() \
                  AND state IN ('queued','running','sleeping','waiting') \
                  AND (claimed_by IS NULL OR lease_expires IS NULL OR lease_expires <= now()) \
                GROUP BY app_id \
                ORDER BY first_wake, app_id \
                LIMIT $1 \
             ) \
             SELECT r.id, r.app_id, r.workflow_name, r.deploy_id, d.deploy_hash, \
                    r.input, r.started_at, r.waiting_step_key \
               FROM due_apps a \
               CROSS JOIN LATERAL ( \
                 SELECT id, app_id, workflow_name, deploy_id, input, started_at, waiting_step_key, wake_at \
                   FROM zeroship.workflow_runs \
                  WHERE app_id = a.app_id \
                    AND wake_at <= now() \
                    AND state IN ('queued','running','sleeping','waiting') \
                    AND (claimed_by IS NULL OR lease_expires IS NULL OR lease_expires <= now()) \
                  ORDER BY wake_at, id \
                  LIMIT $2 \
                  FOR UPDATE SKIP LOCKED \
               ) r \
               JOIN zeroship.app_deploys d ON d.id = r.deploy_id \
              ORDER BY r.app_id, r.wake_at, r.id \
              LIMIT $3",
            &[
                &config.batch_apps,
                &config.per_app_fair_limit,
                &limit,
            ],
        )
        .await
        .map_err(RegistryError::from)?;

    let mut claimed = Vec::new();
    for row in rows {
        let candidate = CandidateRun {
            run_id: row.get("id"),
            app_id: row.get("app_id"),
            workflow_name: row.get("workflow_name"),
            deploy_id: row.get("deploy_id"),
            deploy_hash: row.get("deploy_hash"),
            input: row.get("input"),
            started_at: row.get("started_at"),
            waiting_step_key: row.get("waiting_step_key"),
        };

        tx.batch_execute("SAVEPOINT workflow_claim_row")
            .await
            .map_err(RegistryError::from)?;
        match claim_one_locked(&tx, config, candidate).await {
            Ok(Some(run)) => {
                tx.batch_execute("RELEASE SAVEPOINT workflow_claim_row")
                    .await
                    .map_err(RegistryError::from)?;
                claimed.push(run);
            }
            Ok(None) => {
                tx.batch_execute("RELEASE SAVEPOINT workflow_claim_row")
                    .await
                    .map_err(RegistryError::from)?;
            }
            Err(e) => {
                tracing::warn!(error = %e, "workflow_engine: quarantining poison claim row");
                tx.batch_execute("ROLLBACK TO SAVEPOINT workflow_claim_row")
                    .await
                    .map_err(RegistryError::from)?;
                tx.batch_execute("RELEASE SAVEPOINT workflow_claim_row")
                    .await
                    .map_err(RegistryError::from)?;
            }
        }
    }

    tx.commit().await.map_err(RegistryError::from)?;
    Ok(claimed)
}

async fn claim_one_locked<C>(
    tx: &C,
    config: &WorkflowEngineConfig,
    candidate: CandidateRun,
) -> Result<Option<ClaimedRun>, RegistryError>
where
    C: GenericClient + Sync,
{
    let inflight = tx
        .query(
            "SELECT COUNT(*)::bigint AS n \
               FROM zeroship.workflow_runs \
              WHERE app_id = $1 \
                AND id <> $2 \
                AND state = 'running' \
                AND claimed_by IS NOT NULL \
                AND lease_expires IS NOT NULL \
                AND lease_expires > now()",
            &[&candidate.app_id, &candidate.run_id],
        )
        .await
        .map_err(RegistryError::from)?;
    let inflight: i64 = inflight[0].get("n");
    if inflight >= config.max_inflight_per_app {
        return Ok(None);
    }

    let dispatch_nonce = typed_id::new_workflow_dispatch_id();
    let lease_expires = Utc::now() + chrono::Duration::milliseconds(config.claim_ttl_ms);
    if let Some(key) = candidate.waiting_step_key.as_deref() {
        resolve_due_waiting_step(tx, &candidate.run_id, key, &dispatch_nonce).await?;
    }

    let rows = tx
        .query(
            "UPDATE zeroship.workflow_runs \
                SET claimed_by = $1, \
                    lease_expires = $2, \
                    dispatch_nonce = $3, \
                    claim_epoch = claim_epoch + 1, \
                    state = 'running', \
                    last_dispatch_at = now() \
              WHERE id = $4 \
                AND wake_at <= now() \
                AND state IN ('queued','running','sleeping','waiting') \
                AND (claimed_by IS NULL OR lease_expires IS NULL OR lease_expires <= now()) \
              RETURNING id",
            &[
                &config.owner_id,
                &lease_expires,
                &dispatch_nonce,
                &candidate.run_id,
            ],
        )
        .await
        .map_err(RegistryError::from)?;
    if rows.is_empty() {
        return Ok(None);
    }

    let journal = load_journal(tx, &candidate.run_id).await?;
    Ok(Some(ClaimedRun {
        request: StepRequest {
            run_id: candidate.run_id,
            app_id: candidate.app_id,
            workflow_name: candidate.workflow_name,
            deploy_id: candidate.deploy_id,
            deploy_hash: candidate.deploy_hash,
            dispatch_nonce,
            input: candidate.input,
            started_at: candidate.started_at,
            journal,
        },
    }))
}

async fn load_journal<C>(conn: &C, run_id: &str) -> Result<Vec<JournalStep>, RegistryError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            "SELECT ordinal, name, kind, state, output, error \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 \
              ORDER BY ordinal",
            &[&run_id],
        )
        .await
        .map_err(RegistryError::from)?;
    Ok(rows
        .into_iter()
        .map(|row| JournalStep {
            ordinal: row.get("ordinal"),
            name: row.get("name"),
            kind: row.get("kind"),
            state: row.get("state"),
            output: row.get("output"),
            error: row.get("error"),
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WaitingStep {
    Sleep {
        ordinal: i32,
        name: String,
    },
    WaitSignal {
        ordinal: i32,
        name: String,
        signal_type: String,
        max_signal_age_ms: Option<i64>,
    },
}

fn parse_waiting_step_key(key: &str) -> Result<WaitingStep, RegistryError> {
    let parts: Vec<&str> = key.split(':').collect();
    match parts.as_slice() {
        ["sleep", ordinal, name] => {
            let ordinal = ordinal.parse::<i32>().map_err(|_| {
                RegistryError::InvalidInput(format!("invalid sleep waiting_step_key ordinal: {key}"))
            })?;
            Ok(WaitingStep::Sleep {
                ordinal,
                name: (*name).to_string(),
            })
        }
        ["wait", ordinal, name, signal_type] => {
            let ordinal = ordinal.parse::<i32>().map_err(|_| {
                RegistryError::InvalidInput(format!("invalid wait waiting_step_key ordinal: {key}"))
            })?;
            Ok(WaitingStep::WaitSignal {
                ordinal,
                name: (*name).to_string(),
                signal_type: (*signal_type).to_string(),
                max_signal_age_ms: None,
            })
        }
        ["wait", ordinal, name, signal_type, max_age] => {
            let ordinal = ordinal.parse::<i32>().map_err(|_| {
                RegistryError::InvalidInput(format!("invalid wait waiting_step_key ordinal: {key}"))
            })?;
            let max_signal_age_ms = max_age.parse::<i64>().map_err(|_| {
                RegistryError::InvalidInput(format!("invalid wait waiting_step_key max age: {key}"))
            })?;
            Ok(WaitingStep::WaitSignal {
                ordinal,
                name: (*name).to_string(),
                signal_type: (*signal_type).to_string(),
                max_signal_age_ms: Some(max_signal_age_ms),
            })
        }
        _ => Err(RegistryError::InvalidInput(format!(
            "unrecognized waiting_step_key: {key}"
        ))),
    }
}

async fn resolve_due_waiting_step<C>(
    tx: &C,
    run_id: &str,
    key: &str,
    dispatch_nonce: &str,
) -> Result<(), RegistryError>
where
    C: GenericClient + Sync,
{
    match parse_waiting_step_key(key)? {
        WaitingStep::Sleep { ordinal, name } => {
            insert_resolved_step(
                tx,
                &StepCheckpoint {
                    ordinal,
                    name,
                    name_occurrence: 0,
                    kind: "sleep".to_string(),
                    state: "completed".to_string(),
                    output: None,
                    error: None,
                    wake_at: None,
                    signal_type: None,
                    max_signal_age_ms: None,
                    consumed_signal_id: None,
                },
                run_id,
                dispatch_nonce,
            )
            .await?;
        }
        WaitingStep::WaitSignal {
            ordinal,
            name,
            signal_type,
            max_signal_age_ms,
        } => {
            let min_created_at =
                max_signal_age_ms.map(|age| Utc::now() - chrono::Duration::milliseconds(age));
            let signal = tx
                .query(
                    "SELECT id, payload, created_at, origin, delivery, topic \
                       FROM zeroship.workflow_signals \
                      WHERE run_id = $1 \
                        AND type = $2 \
                        AND consumed_by IS NULL \
                        AND ($3::timestamptz IS NULL OR created_at >= $3) \
                      ORDER BY created_at, id \
                      LIMIT 1 \
                      FOR UPDATE SKIP LOCKED",
                    &[&run_id, &signal_type, &min_created_at],
                )
                .await
                .map_err(RegistryError::from)?;
            let (output, consumed_signal_id) = match signal.first() {
                Some(row) => {
                    let signal_id: String = row.get("id");
                    let payload: Option<Value> = row.get("payload");
                    let created_at: DateTime<Utc> = row.get("created_at");
                    let origin: String = row.get("origin");
                    let delivery: String = row.get("delivery");
                    let topic: Option<String> = row.get("topic");
                    tx.execute(
                        "UPDATE zeroship.workflow_signals \
                            SET consumed_by = $1 \
                          WHERE id = $2 AND consumed_by IS NULL",
                        &[&run_id, &signal_id],
                    )
                    .await
                    .map_err(RegistryError::from)?;
                    (
                        Some(serde_json::json!({
                            "type": signal_type,
                            "payload": payload,
                            "receivedAt": created_at.to_rfc3339(),
                            "origin": origin,
                            "delivery": delivery,
                            "topic": topic,
                        })),
                        Some(signal_id),
                    )
                }
                None => (Some(Value::Null), None),
            };
            insert_resolved_step(
                tx,
                &StepCheckpoint {
                    ordinal,
                    name,
                    name_occurrence: 0,
                    kind: "wait_signal".to_string(),
                    state: "completed".to_string(),
                    output,
                    error: None,
                    wake_at: None,
                    signal_type: Some(signal_type),
                    max_signal_age_ms,
                    consumed_signal_id,
                },
                run_id,
                dispatch_nonce,
            )
            .await?;
        }
    }

    tx.execute(
        "UPDATE zeroship.workflow_runs SET waiting_step_key = NULL WHERE id = $1",
        &[&run_id],
    )
    .await
    .map_err(RegistryError::from)?;
    Ok(())
}

fn spawn_dispatch<D>(
    registry: Registry,
    dispatcher: Arc<D>,
    config: WorkflowEngineConfig,
    claim: ClaimedRun,
) where
    D: StepDispatcher + 'static,
{
    INFLIGHT_DISPATCHES.fetch_add(1, Ordering::SeqCst);
    let heartbeat = spawn_heartbeat(
        registry.clone(),
        config.owner_id.clone(),
        claim.request.run_id.clone(),
        claim.request.dispatch_nonce.clone(),
        config.claim_ttl_ms,
        Duration::from_millis(config.heartbeat_ms),
    );
    compio::runtime::spawn(async move {
        let outcome = dispatcher.dispatch(claim.request).await;
        heartbeat.store(false, Ordering::SeqCst);
        match outcome {
            DispatchOutcome::Completed(result) => {
                match apply_step_result_on_registry(&registry, &config.owner_id, result.clone()).await {
                    Ok(_) => {}
                    Err(ApplyError::Deadlock(msg)) => {
                        tracing::warn!(error = %msg, run_id = %result.run_id, "workflow_engine: apply deadlock, requeueing claim");
                        if let Err(e) =
                            requeue_claim(&registry, &config.owner_id, &result.run_id, &result.dispatch_nonce).await
                        {
                            tracing::error!(error = %e, run_id = %result.run_id, "workflow_engine: deadlock requeue failed");
                        }
                    }
                    Err(ApplyError::Db(e)) => {
                        tracing::error!(error = %e, run_id = %result.run_id, "workflow_engine: apply failed");
                    }
                }
            }
            DispatchOutcome::Backpressure {
                run_id,
                dispatch_nonce,
                reason,
            } => {
                tracing::warn!(
                    run_id = %run_id,
                    reason = %reason,
                    "workflow_engine: gateway backpressure, parking claim"
                );
                if let Err(e) =
                    park_backpressure_claim(&registry, &config.owner_id, &run_id, &dispatch_nonce).await
                {
                    tracing::error!(error = %e, run_id = %run_id, "workflow_engine: backpressure park failed");
                }
            }
        }
        INFLIGHT_DISPATCHES.fetch_sub(1, Ordering::SeqCst);
    })
    .detach();
}

fn spawn_heartbeat(
    registry: Registry,
    owner_id: String,
    run_id: String,
    dispatch_nonce: String,
    claim_ttl_ms: i64,
    interval: Duration,
) -> Arc<AtomicBool> {
    let active = Arc::new(AtomicBool::new(true));
    let heartbeat_active = Arc::clone(&active);
    compio::runtime::spawn(async move {
        while heartbeat_active.load(Ordering::SeqCst) {
            compio::time::sleep(interval).await;
            if !heartbeat_active.load(Ordering::SeqCst) {
                break;
            }
            let beat = async {
                let conn = registry.conn().await?;
                let lease_expires = Utc::now() + chrono::Duration::milliseconds(claim_ttl_ms);
                conn.execute(
                    "UPDATE zeroship.workflow_runs \
                        SET lease_expires = $1 \
                      WHERE id = $2 \
                        AND claimed_by = $3 \
                        AND dispatch_nonce = $4",
                    &[&lease_expires, &run_id, &owner_id, &dispatch_nonce],
                )
                .await
                .map_err(RegistryError::from)?;
                Ok::<(), RegistryError>(())
            }
            .await;
            if let Err(e) = beat {
                tracing::warn!(error = %e, run_id = %run_id, "workflow_engine heartbeat failed");
            }
        }
    })
    .detach();
    active
}

#[derive(Debug)]
enum ApplyError {
    Deadlock(String),
    Db(RegistryError),
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Deadlock(msg) => write!(f, "deadlock: {msg}"),
            Self::Db(e) => write!(f, "{e}"),
        }
    }
}

impl From<RegistryError> for ApplyError {
    fn from(value: RegistryError) -> Self {
        Self::Db(value)
    }
}

fn map_apply_error(e: compio_postgres::Error) -> ApplyError {
    if e.code() == Some(&SqlState::T_R_DEADLOCK_DETECTED) {
        ApplyError::Deadlock(e.to_string())
    } else {
        ApplyError::Db(RegistryError::from(e))
    }
}

/// Public deterministic apply path for tests and future control handlers.
#[allow(clippy::future_not_send)]
pub async fn apply_step_result(
    state: &AppState,
    owner_id: &str,
    result: StepResult,
) -> Result<bool, RegistryError> {
    apply_step_result_on_registry(&state.registry, owner_id, result)
        .await
        .map_err(|e| match e {
            ApplyError::Deadlock(msg) => RegistryError::Database(format!("retryable deadlock: {msg}")),
            ApplyError::Db(e) => e,
        })
}

async fn apply_step_result_on_registry(
    registry: &Registry,
    owner_id: &str,
    mut result: StepResult,
) -> Result<bool, ApplyError> {
    result.checkpoints.sort_by_key(|s| s.ordinal);
    let mut conn = registry.conn().await.map_err(ApplyError::Db)?;
    let tx = conn.transaction().await.map_err(map_apply_error)?;

    // Row-lock-first: this is intentionally the first statement in the txn.
    let rows = tx
        .query(
            "SELECT claimed_by, state, dispatch_nonce \
               FROM zeroship.workflow_runs \
              WHERE id = $1 \
              FOR UPDATE",
            &[&result.run_id],
        )
        .await
        .map_err(map_apply_error)?;
    let Some(row) = rows.first() else {
        tx.commit().await.map_err(map_apply_error)?;
        return Ok(false);
    };
    let claimed_by: Option<String> = row.get("claimed_by");
    let state: String = row.get("state");
    let dispatch_nonce: Option<String> = row.get("dispatch_nonce");
    if claimed_by.as_deref() != Some(owner_id)
        || dispatch_nonce.as_deref() != Some(result.dispatch_nonce.as_str())
        || !matches!(state.as_str(), "running" | "paused")
    {
        tx.commit().await.map_err(map_apply_error)?;
        return Ok(false);
    }

    for checkpoint in &result.checkpoints {
        insert_resolved_step(&tx, checkpoint, &result.run_id, &result.dispatch_nonce)
            .await
            .map_err(ApplyError::Db)?;
    }

    let mut consumed: Vec<String> = result
        .checkpoints
        .iter()
        .filter_map(|s| s.consumed_signal_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    consumed.sort();
    for signal_id in consumed {
        tx.execute(
            "UPDATE zeroship.workflow_signals \
                SET consumed_by = $1 \
              WHERE id = $2 AND consumed_by IS NULL",
            &[&result.run_id, &signal_id],
        )
        .await
        .map_err(map_apply_error)?;
    }

    let next_ordinal = result
        .checkpoints
        .iter()
        .map(|s| s.ordinal.saturating_add(1))
        .max()
        .unwrap_or(0);
    let state = result.run_update.state();
    let wake_at = result.run_update.wake_at();
    let waiting_step_key = result.run_update.waiting_step_key(&result.checkpoints);
    let output = result.run_update.output();
    let error = result.run_update.error();
    tx.execute(
        "UPDATE zeroship.workflow_runs \
            SET state = $1, \
                output = $2, \
                error = $3, \
                wake_at = $4, \
                next_ordinal = GREATEST(next_ordinal, $5), \
                waiting_step_key = $6, \
                claimed_by = NULL, \
                lease_expires = NULL, \
                dispatch_nonce = NULL \
          WHERE id = $7 \
            AND claimed_by = $8 \
            AND dispatch_nonce = $9",
        &[
            &state,
            &output,
            &error,
            &wake_at,
            &next_ordinal,
            &waiting_step_key,
            &result.run_id,
            &owner_id,
            &result.dispatch_nonce,
        ],
    )
    .await
    .map_err(map_apply_error)?;

    tx.commit().await.map_err(map_apply_error)?;
    Ok(true)
}

async fn insert_resolved_step<C>(
    conn: &C,
    checkpoint: &StepCheckpoint,
    run_id: &str,
    batch_id: &str,
) -> Result<(), RegistryError>
where
    C: GenericClient + Sync,
{
    conn.execute(
        "INSERT INTO zeroship.workflow_steps \
            (run_id, ordinal, name, name_occurrence, kind, state, output, error, \
             output_kind, wake_at, signal_type, max_signal_age_ms, consumed_signal_id, \
             batch_id, batch_width, finished_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, \
                 'inline', $9, $10, $11, $12, $13, 1, now()) \
         ON CONFLICT (run_id, ordinal) DO UPDATE SET \
             state = EXCLUDED.state, \
             output = EXCLUDED.output, \
             error = EXCLUDED.error, \
             wake_at = EXCLUDED.wake_at, \
             signal_type = EXCLUDED.signal_type, \
             max_signal_age_ms = EXCLUDED.max_signal_age_ms, \
             consumed_signal_id = EXCLUDED.consumed_signal_id, \
             finished_at = EXCLUDED.finished_at \
           WHERE zeroship.workflow_steps.state = 'running' \
             AND EXCLUDED.state = 'completed' \
             AND zeroship.workflow_steps.name = EXCLUDED.name \
             AND zeroship.workflow_steps.kind = EXCLUDED.kind",
        &[
            &run_id,
            &checkpoint.ordinal,
            &checkpoint.name,
            &checkpoint.name_occurrence,
            &checkpoint.kind,
            &checkpoint.state,
            &checkpoint.output,
            &checkpoint.error,
            &checkpoint.wake_at,
            &checkpoint.signal_type,
            &checkpoint.max_signal_age_ms,
            &checkpoint.consumed_signal_id,
            &batch_id,
        ],
    )
    .await
    .map_err(RegistryError::from)?;
    Ok(())
}

async fn requeue_claim(
    registry: &Registry,
    owner_id: &str,
    run_id: &str,
    dispatch_nonce: &str,
) -> Result<(), RegistryError> {
    let conn = registry.conn().await?;
    conn.execute(
        "UPDATE zeroship.workflow_runs \
            SET state = 'queued', \
                wake_at = now(), \
                claimed_by = NULL, \
                lease_expires = NULL, \
                dispatch_nonce = NULL \
          WHERE id = $1 \
            AND claimed_by = $2 \
            AND dispatch_nonce = $3",
        &[&run_id, &owner_id, &dispatch_nonce],
    )
    .await
    .map_err(RegistryError::from)?;
    Ok(())
}

async fn park_backpressure_claim(
    registry: &Registry,
    owner_id: &str,
    run_id: &str,
    dispatch_nonce: &str,
) -> Result<(), RegistryError> {
    let wake_at = Utc::now() + chrono::Duration::milliseconds(BACKPRESSURE_PARK_MS);
    let conn = registry.conn().await?;
    conn.execute(
        "UPDATE zeroship.workflow_runs \
            SET state = 'queued', \
                wake_at = $1, \
                claimed_by = NULL, \
                lease_expires = NULL, \
                dispatch_nonce = NULL \
          WHERE id = $2 \
            AND claimed_by = $3 \
            AND dispatch_nonce = $4",
        &[&wake_at, &run_id, &owner_id, &dispatch_nonce],
    )
    .await
    .map_err(RegistryError::from)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntex::web::{self, test};

    #[test]
    fn default_tick_is_one_second() {
        assert_eq!(DEFAULT_TICK_SECS, 1);
    }

    #[test]
    fn waiting_step_key_parses_sleep_and_wait() {
        assert_eq!(
            parse_waiting_step_key("sleep:7:cooldown").expect("sleep key"),
            WaitingStep::Sleep {
                ordinal: 7,
                name: "cooldown".to_string()
            }
        );
        assert_eq!(
            parse_waiting_step_key("wait:8:approved:approved:60000").expect("wait key"),
            WaitingStep::WaitSignal {
                ordinal: 8,
                name: "approved".to_string(),
                signal_type: "approved".to_string(),
                max_signal_age_ms: Some(60_000)
            }
        );
    }

    #[test]
    fn step_result_deserializes_sleeping_wake_at_contract() {
        let result: StepResult = serde_json::from_value(serde_json::json!({
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
                "wakeAt": "2026-07-06T10:24:10Z",
                "signalType": null,
                "maxSignalAgeMs": null,
                "consumedSignalId": null
            }],
            "runUpdate": {
                "state": "sleeping",
                "wakeAt": "2026-07-06T10:24:10Z"
            }
        }))
        .expect("StepResult JSON");
        assert_eq!(
            result.run_update.wake_at().expect("run wake_at").to_rfc3339(),
            "2026-07-06T10:24:10+00:00"
        );
        assert_eq!(
            result.checkpoints[0]
                .wake_at
                .expect("checkpoint wake_at")
                .to_rfc3339(),
            "2026-07-06T10:24:10+00:00"
        );
    }

    fn test_step_request() -> StepRequest {
        StepRequest {
            run_id: "run_test".to_string(),
            app_id: Uuid::new_v4(),
            workflow_name: "Checkout".to_string(),
            deploy_id: "dep_test".to_string(),
            deploy_hash: "hash_test".to_string(),
            dispatch_nonce: "wfd_test".to_string(),
            input: Some(serde_json::json!({"orderId": "ord_1"})),
            started_at: Utc::now(),
            journal: Vec::new(),
        }
    }

    async fn capture_step_request(
        seen: web::types::State<Arc<std::sync::Mutex<Vec<Value>>>>,
        body: ntex::util::Bytes,
    ) -> web::HttpResponse {
        let request: Value = serde_json::from_slice(body.as_ref()).expect("StepRequest json");
        seen.lock().expect("seen lock").push(request.clone());
        web::HttpResponse::Ok().json(&serde_json::json!({
            "runId": request["runId"],
            "dispatchNonce": request["dispatchNonce"],
            "checkpoints": [{
                "ordinal": 0,
                "name": "done",
                "nameOccurrence": 0,
                "kind": "run",
                "state": "completed",
                "output": {"ok": true},
                "error": null,
                "wakeAt": null,
                "signalType": null,
                "maxSignalAgeMs": null,
                "consumedSignalId": null
            }],
            "runUpdate": {
                "state": "completed",
                "output": {"ok": true}
            }
        }))
    }

    #[ntex::test]
    async fn gateway_step_dispatcher_posts_and_parses_step_result() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let server_seen = Arc::clone(&seen);
        let gateway = test::server(move || {
            let seen = Arc::clone(&server_seen);
            async move {
                web::App::new().state(seen).service(
                    web::resource(GATEWAY_WORKFLOW_DISPATCH_PATH)
                        .route(web::post().to(capture_step_request)),
                )
            }
        })
        .await;

        let request = test_step_request();
        let dispatcher = GatewayStepDispatcher::new(gateway.url(""));
        let outcome = dispatcher.dispatch(request.clone()).await;
        let DispatchOutcome::Completed(result) = outcome else {
            panic!("expected completed dispatch outcome");
        };
        assert_eq!(result.run_id, request.run_id);
        assert_eq!(result.dispatch_nonce, request.dispatch_nonce);
        assert_eq!(result.checkpoints.len(), 1);
        assert_eq!(result.checkpoints[0].name, "done");

        let seen = seen.lock().expect("seen lock");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0]["runId"], request.run_id);
        assert_eq!(seen[0]["appId"], request.app_id.to_string());
        assert_eq!(seen[0]["deployHash"], request.deploy_hash);
    }

    #[ntex::test]
    async fn gateway_step_dispatcher_maps_402_to_backpressure() {
        let gateway = test::server(|| async {
            web::App::new().service(
                web::resource(GATEWAY_WORKFLOW_DISPATCH_PATH)
                    .route(web::post().to(|| async {
                        web::HttpResponse::PaymentRequired()
                            .json(&serde_json::json!({"code": "SPEND_LIMIT"}))
                    })),
            )
        })
        .await;

        let request = test_step_request();
        let dispatcher = GatewayStepDispatcher::new(gateway.url(""));
        let outcome = dispatcher.dispatch(request.clone()).await;
        let DispatchOutcome::Backpressure {
            run_id,
            dispatch_nonce,
            reason,
        } = outcome
        else {
            panic!("expected backpressure dispatch outcome");
        };
        assert_eq!(run_id, request.run_id);
        assert_eq!(dispatch_nonce, request.dispatch_nonce);
        assert!(reason.contains("HTTP 402"));
    }
}
