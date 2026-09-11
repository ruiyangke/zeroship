//! Local dev-tier workflow mini-engine.
//!
//! This module is intentionally constructed only by `zeroship_workflow_v8::WorkflowBinding::dev_sqlite`
//! from the CLI serve path. It mirrors the production journal/fold shape for
//! the local inner loop without a control plane, gateway, or shared database.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{de, Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};
use zeroship_core::typed_id;

use crate::backend::WorkflowBackend;
use crate::errors::WorkflowServiceError;
use crate::operations::{
    DeliveredSignal, RestartOptions, RestartedRun, RunOperation, RunStatus, SignalOptions,
    StartOptions, StartedRun, TransitionedRun,
};
use crate::validation::{
    signal_type as validate_signal_type, workflow_name as validate_workflow_name,
};

const DEV_DEPLOY_HASH: &str = "dev-local";
const DEV_OWNER_ID: &str = "dev-workflow-engine";
const DEV_TICK_MS: u64 = 100;
const STUCK_STRIKE_LIMIT: i16 = 3;

/// Executes a serialized workflow dispatch envelope and returns its result.
/// The host owns the execution environment; the journal engine owns persistence.
#[async_trait(?Send)]
pub trait WorkflowExecutor: Send + Sync {
    async fn dispatch(&self, envelope: &str) -> Result<String, WorkflowServiceError>;
}

pub struct DevWorkflowEngine {
    path: PathBuf,
    conn: Mutex<Connection>,
    executor: Arc<dyn WorkflowExecutor>,
    scheduler_started: AtomicBool,
}

impl std::fmt::Debug for DevWorkflowEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevWorkflowEngine")
            .field("path", &self.path)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct DevWorkflowBackend {
    engine: Arc<DevWorkflowEngine>,
    app_id: String,
}

#[derive(Debug)]
struct ClaimedRun {
    request: StepRequest,
}

#[derive(Debug, Clone)]
struct CandidateRun {
    run_id: String,
    app_id: String,
    workflow_name: String,
    deploy_id: String,
    deploy_hash: String,
    // No `state` field. It existed only to feed a `phase: "compensating"` branch
    // in `claim_one_due` that its own SELECT made unreachable; see the comment
    // there. Reintroducing it means dev has grown a compensating phase.
    input: Option<Value>,
    started_at: i64,
    waiting_step_key: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct StepRequest {
    run_id: String,
    app_id: String,
    workflow_name: String,
    deploy_id: String,
    deploy_hash: String,
    dispatch_nonce: String,
    nonce: String,
    phase: String,
    input: Option<Value>,
    trigger: Value,
    started_at: String,
    journal: Vec<JournalStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JournalStep {
    ordinal: i32,
    name: String,
    #[serde(default, rename = "nameOccurrence")]
    name_occurrence: i32,
    kind: String,
    state: String,
    output: Option<Value>,
    error: Option<Value>,
    #[serde(default, rename = "childRunId")]
    child_run_id: Option<String>,
    #[serde(default, rename = "compensationState")]
    compensation_state: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StepCheckpoint {
    ordinal: i32,
    name: String,
    name_occurrence: i32,
    kind: String,
    state: String,
    output: Option<Value>,
    error: Option<Value>,
    wake_at: Option<DateTime<Utc>>,
    signal_type: Option<String>,
    max_signal_age_ms: Option<i64>,
    consumed_signal_id: Option<String>,
    topic: Option<String>,
    child_run_id: Option<String>,
    #[serde(default, rename = "compensationState")]
    compensation_state: Option<String>,
    #[serde(
        default = "default_compensation_max_attempts",
        rename = "compensationMaxAttempts"
    )]
    compensation_max_attempts: i32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind")]
enum StepOutcome {
    StepCompleted {
        ordinal: i32,
        name: String,
        #[serde(default, rename = "nameOccurrence")]
        name_occurrence: i32,
        #[serde(default = "default_step_kind", rename = "stepKind")]
        step_kind: String,
        #[serde(default)]
        compensable: bool,
        #[serde(
            default = "default_compensation_max_attempts",
            rename = "compensationMaxAttempts"
        )]
        compensation_max_attempts: i32,
        #[serde(default)]
        output: Option<Value>,
    },
    StepFailed {
        ordinal: i32,
        name: String,
        #[serde(default, rename = "nameOccurrence")]
        name_occurrence: i32,
        #[serde(default)]
        error: Value,
    },
    RunCompleted {
        #[serde(default)]
        output: Option<Value>,
    },
    RunFailed {
        #[serde(default)]
        ordinal: Option<i32>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default, rename = "nameOccurrence")]
        name_occurrence: i32,
        error: Value,
    },
    Sleep {
        ordinal: i32,
        name: String,
        #[serde(default, rename = "nameOccurrence")]
        name_occurrence: i32,
        #[serde(rename = "wakeAt", deserialize_with = "deserialize_wake_at")]
        wake_at: DateTime<Utc>,
    },
    Wait {
        ordinal: i32,
        name: String,
        #[serde(default, rename = "nameOccurrence")]
        name_occurrence: i32,
        #[serde(
            default,
            rename = "wakeAt",
            deserialize_with = "deserialize_optional_wake_at"
        )]
        wake_at: Option<DateTime<Utc>>,
        #[serde(default, deserialize_with = "deserialize_optional_wake_duration")]
        timeout: Option<DateTime<Utc>>,
        #[serde(default, rename = "signalType")]
        signal_type: Option<String>,
        #[serde(default, rename = "maxSignalAgeMs")]
        max_signal_age_ms: Option<i64>,
        #[serde(default, rename = "consumedSignalId")]
        consumed_signal_id: Option<String>,
        #[serde(default)]
        topic: Option<String>,
    },
    Child {
        ordinal: i32,
        name: String,
        #[serde(default, rename = "nameOccurrence")]
        name_occurrence: i32,
    },
}

#[derive(Debug, Clone)]
enum RunUpdate {
    Queued,
    Sleeping {
        wake_at: Option<DateTime<Utc>>,
    },
    Waiting {
        wake_at: Option<DateTime<Utc>>,
    },
    Completed {
        output: Option<Value>,
    },
    Failed {
        error: Value,
    },
    Stalled {
        error: Value,
    },
    #[allow(
        dead_code,
        reason = "cancel transitions are applied directly in the dev instance API"
    )]
    Cancelled,
}

#[derive(Debug, Clone)]
struct StepResult {
    run_id: String,
    dispatch_nonce: String,
    checkpoints: Vec<StepCheckpoint>,
    run_update: RunUpdate,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StepResultWire {
    run_id: String,
    dispatch_nonce: String,
    #[serde(default)]
    outcomes: Vec<StepOutcome>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepWriteOutcome {
    Wrote,
    Noop,
}

impl<'de> Deserialize<'de> for StepResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = StepResultWire::deserialize(deserializer)?;
        let (checkpoints, run_update) =
            fold_dev_outcomes(&wire.outcomes).map_err(serde::de::Error::custom)?;
        Ok(Self {
            run_id: wire.run_id,
            dispatch_nonce: wire.dispatch_nonce,
            checkpoints,
            run_update,
        })
    }
}

impl RunUpdate {
    fn state(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Sleeping { .. } => "sleeping",
            Self::Waiting { .. } => "waiting",
            Self::Completed { .. } => "completed",
            Self::Failed { .. } => "failed",
            Self::Stalled { .. } => "stalled",
            Self::Cancelled => "cancelled",
        }
    }

    fn wake_at_ms(&self) -> Option<i64> {
        match self {
            Self::Queued => Some(now_ms()),
            Self::Sleeping { wake_at } | Self::Waiting { wake_at } => wake_at.map(datetime_to_ms),
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
            Self::Failed { error } | Self::Stalled { error } => Some(error.clone()),
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

impl DevWorkflowEngine {
    pub fn open(
        db_path: impl AsRef<Path>,
        executor: Arc<dyn WorkflowExecutor>,
    ) -> Result<Arc<Self>, String> {
        let path = db_path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create workflow dev db dir '{}': {e}", parent.display()))?;
        }
        let conn = Connection::open(&path)
            .map_err(|e| format!("open workflow dev sqlite '{}': {e}", path.display()))?;
        bootstrap_schema(&conn)
            .map_err(|e| format!("bootstrap workflow dev sqlite '{}': {e}", path.display()))?;
        Ok(Arc::new(Self {
            path,
            conn: Mutex::new(conn),
            executor,
            scheduler_started: AtomicBool::new(false),
        }))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn backend_for_app(self: &Arc<Self>, app_id: &str) -> DevWorkflowBackend {
        DevWorkflowBackend {
            engine: Arc::clone(self),
            app_id: app_id.to_string(),
        }
    }

    pub fn ensure_scheduler(self: &Arc<Self>) {
        if self.scheduler_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let engine = Arc::clone(self);
        compio::runtime::spawn(async move {
            loop {
                if let Err(e) = engine.tick_due().await {
                    tracing::warn!(error = ?e, "workflow dev engine tick failed");
                }
                compio::time::sleep(Duration::from_millis(DEV_TICK_MS)).await;
            }
        })
        .detach();
    }

    fn start_run(
        &self,
        app_id: &str,
        workflow_name: &str,
        options: StartOptions,
    ) -> Result<StartedRun, WorkflowServiceError> {
        validate_workflow_name(workflow_name)?;
        crate::validation::start(&options)?;
        let mut conn = self.lock_conn()?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let deploy_id = ensure_dev_deploy(&tx, app_id)?;
        let now = now_ms();
        if let Some(key) = &options.key {
            tx.execute(
                "UPDATE workflow_runs SET dedup_key = NULL \
                 WHERE app_id = ?1 AND workflow_name = ?2 AND dedup_key = ?3 \
                 AND state IN ('completed','failed','cancelled')",
                params![app_id, workflow_name, key],
            )
            .map_err(db_error)?;
            if let Some(existing) = existing_keyed_run(&tx, app_id, workflow_name, key)? {
                match options.on_conflict {
                    crate::operations::ConflictPolicy::Join => {
                        let state: String = tx
                            .query_row(
                                "SELECT state FROM workflow_runs WHERE id = ?1 AND app_id = ?2",
                                params![existing, app_id],
                                |row| row.get(0),
                            )
                            .map_err(db_error)?;
                        tx.commit().map_err(db_error)?;
                        drop(conn);
                        return Ok(StartedRun {
                            id: existing,
                            state: state.parse().map_err(WorkflowServiceError::Internal)?,
                        });
                    }
                    crate::operations::ConflictPolicy::Reject => {
                        return Err(WorkflowServiceError::Conflict(
                            "workflow run already exists for key".into(),
                        ))
                    }
                    crate::operations::ConflictPolicy::Replace => {
                        tx.execute(
                            "UPDATE workflow_runs SET state = 'cancelled', dedup_key = NULL, wake_at = NULL, \
                             terminal_at = ?3, output = NULL, error = NULL, output_kind = 'inline', \
                             output_hash = NULL, output_size = NULL, output_content_type = NULL, paused_from_status = NULL, \
                             claimed_by = NULL, lease_expires = NULL, dispatch_nonce = NULL \
                             WHERE id = ?1 AND app_id = ?2",
                            params![existing, app_id, now],
                        ).map_err(db_error)?;
                    }
                }
            }
        }
        let run_id = typed_id::new_workflow_run_id();
        tx.execute(
            "INSERT INTO workflow_runs \
             (id, workflow_name, app_id, deploy_id, state, input, journal_bytes, dedup_key, wake_at, started_at, created_at) \
             VALUES (?1, ?2, ?3, ?4, 'queued', ?5, ?6, ?7, ?8, ?8, ?8)",
            params![run_id, workflow_name, app_id, deploy_id, json_to_string(&options.input)?,
                json_size(&options.input), options.key, now],
        ).map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        drop(conn);
        Ok(StartedRun {
            id: run_id,
            state: crate::operations::RunState::Queued,
        })
    }

    fn status(&self, app_id: &str, run_id: &str) -> Result<RunStatus, WorkflowServiceError> {
        let conn = self.lock_conn()?;
        let row = conn
            .query_row(
                "SELECT state, output, error, output_kind, output_hash, output_size, output_content_type \
                   FROM workflow_runs \
                  WHERE id = ?1 AND app_id = ?2",
                params![run_id, app_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                    ))
                },
            )
            .optional()
            .map_err(db_error)?;
        let Some((
            state,
            output,
            error,
            output_kind,
            output_hash,
            output_size,
            output_content_type,
        )) = row
        else {
            return Err(WorkflowServiceError::NotFound(
                "workflow run not found".to_string(),
            ));
        };
        Ok(RunStatus {
            state: state.parse().map_err(WorkflowServiceError::Internal)?,
            output: Some(status_output(
                output_kind,
                output_hash,
                output_size,
                output_content_type,
                output,
            )?),
            error: parse_json_opt(error)?,
        })
    }

    fn signal(
        &self,
        app_id: &str,
        run_id: &str,
        options: SignalOptions,
    ) -> Result<DeliveredSignal, WorkflowServiceError> {
        let signal_type = &options.signal_type;
        validate_signal_type(signal_type)?;
        let payload = options.payload;
        let payload_json = json_to_string(&payload)?;
        let signal_id = typed_id::new_workflow_signal_id();
        let now = now_ms();
        {
            let mut connection = self.lock_conn()?;
            let tx = connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(db_error)?;
            let conn = &tx;
            let run = conn
                .query_row(
                    "SELECT state, waiting_step_key FROM workflow_runs WHERE id = ?1 AND app_id = ?2",
                    params![run_id, app_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
                )
                .optional()
                .map_err(db_error)?;
            let Some((state, waiting_step_key)) = run else {
                return Err(WorkflowServiceError::NotFound(
                    "workflow run not found".to_string(),
                ));
            };
            conn.execute(
                "INSERT INTO workflow_signals \
                 (id, run_id, type, payload, created_at, origin, delivery) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 'app', 'direct')",
                params![signal_id, run_id, signal_type, payload_json, now],
            )
            .map_err(db_error)?;
            if state == "waiting"
                && waiting_key_matches_signal(waiting_step_key.as_deref(), signal_type)
            {
                conn.execute(
                    "UPDATE workflow_runs SET wake_at = ?1 WHERE id = ?2 AND app_id = ?3",
                    params![now, run_id, app_id],
                )
                .map_err(db_error)?;
            }
            tx.commit().map_err(db_error)?;
        }
        Ok(DeliveredSignal { id: signal_id })
    }

    fn transition(
        &self,
        app_id: &str,
        run_id: &str,
        op: RunOperation,
    ) -> Result<TransitionedRun, WorkflowServiceError> {
        let now = now_ms();
        let mut connection = self.lock_conn()?;
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let conn = &tx;
        let current = conn
            .query_row(
                "SELECT state FROM workflow_runs WHERE id = ?1 AND app_id = ?2",
                params![run_id, app_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(db_error)?
            .ok_or_else(|| WorkflowServiceError::NotFound("workflow run not found".to_string()))?;
        let next = match op {
            RunOperation::Pause => {
                if is_terminal(&current) {
                    return Err(WorkflowServiceError::Conflict(format!(
                        "cannot pause workflow run in state {current}"
                    )));
                }
                conn.execute(
                    "UPDATE workflow_runs \
                        SET state = 'paused', paused_from_status = CASE WHEN state = 'paused' THEN paused_from_status ELSE state END, \
                            wake_at = wake_at, terminal_at = NULL, claimed_by = NULL, lease_expires = NULL, dispatch_nonce = NULL \
                      WHERE id = ?1 AND app_id = ?2",
                    params![run_id, app_id],
                )
                .map_err(db_error)?;
                "paused".to_string()
            }
            RunOperation::Resume => {
                if current != "paused" {
                    return Err(WorkflowServiceError::Conflict(format!(
                        "cannot resume workflow run in state {current}"
                    )));
                }
                let restored = restored_state(conn, run_id)?;
                let wake_at = if restored == "queued" {
                    Some(now)
                } else {
                    None
                };
                conn.execute(
                    "UPDATE workflow_runs \
                        SET state = ?3, wake_at = COALESCE(?4, wake_at), terminal_at = NULL, paused_from_status = NULL \
                      WHERE id = ?1 AND app_id = ?2",
                    params![run_id, app_id, restored, wake_at],
                )
                .map_err(db_error)?;
                restored
            }
            RunOperation::Cancel => {
                if is_terminal(&current) {
                    return Err(WorkflowServiceError::Conflict(format!(
                        "cannot cancel workflow run in state {current}"
                    )));
                }
                conn.execute(
                    "UPDATE workflow_runs \
                        SET state = 'cancelled', wake_at = NULL, terminal_at = ?3, output = NULL, error = NULL, \
                            output_kind = 'inline', output_hash = NULL, output_size = NULL, output_content_type = NULL, \
                            paused_from_status = NULL, claimed_by = NULL, lease_expires = NULL, dispatch_nonce = NULL \
                      WHERE id = ?1 AND app_id = ?2",
                    params![run_id, app_id, now],
                )
                .map_err(db_error)?;
                "cancelled".to_string()
            }
        };
        tx.commit().map_err(db_error)?;
        drop(connection);
        Ok(TransitionedRun {
            state: next.parse().map_err(WorkflowServiceError::Internal)?,
        })
    }

    fn restart(
        &self,
        app_id: &str,
        run_id: &str,
        options: RestartOptions,
    ) -> Result<RestartedRun, WorkflowServiceError> {
        let policy = options.deploy_policy()?;
        let mut conn = self.lock_conn()?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let now: i64 = tx
            .query_row(
                "SELECT CAST(unixepoch('subsec') * 1000 AS INTEGER)",
                [],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        let (current_deploy, live_lease, active_compensation) = tx
            .query_row(
                "SELECT deploy_id, COALESCE(lease_expires > ?3, 0), \
             COALESCE(state = 'compensating' OR paused_from_status = 'compensating', 0) \
             FROM workflow_runs WHERE id = ?1 AND app_id = ?2",
                params![run_id, app_id, now],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, bool>(1)?,
                        row.get::<_, bool>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(db_error)?
            .ok_or_else(|| WorkflowServiceError::NotFound("workflow run not found".into()))?;
        let active_descendants = tx.query_row(
            "WITH RECURSIVE descendants AS ( \
                SELECT id, state FROM workflow_runs WHERE parent_run_id = ?1 AND app_id = ?2 \
                UNION \
                SELECT child.id, child.state FROM workflow_runs child \
                JOIN descendants parent ON child.parent_run_id = parent.id WHERE child.app_id = ?2 \
             ) SELECT EXISTS (SELECT 1 FROM descendants WHERE state NOT IN ('completed','failed','cancelled'))",
            params![run_id, app_id], |row| row.get::<_, bool>(0),
        ).map_err(db_error)?;
        crate::lifecycle::RestartSafety {
            live_lease,
            active_descendants,
            active_compensation,
            compensated_prefix: false,
        }
        .check()?;
        let target_ordinal = restart_target_ordinal(&tx, run_id, options.from.as_ref())?;
        let deploy = if policy == crate::operations::RestartDeploy::Latest {
            tx.query_row(
                "SELECT id FROM app_deploys WHERE app_id = ?1 AND activated_at IS NOT NULL \
                 ORDER BY activated_at DESC, id DESC LIMIT 1",
                params![app_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(db_error)?
            .ok_or_else(|| WorkflowServiceError::NotFound("workflow deploy not found".into()))?
        } else {
            current_deploy.clone()
        };
        tx.execute(
            "UPDATE workflow_signals SET consumed_by = NULL \
             WHERE run_id = ?1 AND consumed_by = ?1 AND delivery <> 'topic' \
             AND id IN (SELECT consumed_signal_id FROM workflow_steps WHERE run_id = ?1 AND ordinal >= ?2)",
            params![run_id, target_ordinal],
        ).map_err(db_error)?;
        tx.execute(
            "DELETE FROM workflow_signals WHERE run_id = ?1 AND delivery = 'topic' \
             AND id IN (SELECT consumed_signal_id FROM workflow_steps WHERE run_id = ?1 AND ordinal >= ?2)",
            params![run_id, target_ordinal],
        ).map_err(db_error)?;
        tx.execute(
            "DELETE FROM workflow_steps WHERE run_id = ?1 AND ordinal >= ?2",
            params![run_id, target_ordinal],
        )
        .map_err(db_error)?;
        let restarted_from = options.from.as_ref().map(|_| target_ordinal);
        tx.execute(
            "UPDATE workflow_runs \
             SET state = 'queued', wake_at = ?3, terminal_at = NULL, output = NULL, error = NULL, \
                 output_kind = 'inline', output_hash = NULL, output_size = NULL, output_content_type = NULL, \
                 next_ordinal = ?5, stuck_strikes = 0, waiting_step_key = NULL, paused_from_status = NULL, \
                 claimed_by = NULL, lease_expires = NULL, dispatch_nonce = NULL, restart_count = restart_count + 1, \
                 restarted_at = ?3, restarted_from_ordinal = ?6, restarted_by = ?4, deploy_id = ?7, \
                 signal_epoch = signal_epoch + ?8, cancel_requested = 0, compensation_target = NULL, compensation_outcome = NULL \
             WHERE id = ?1 AND app_id = ?2",
            params![run_id, app_id, now, format!("app:{app_id}"), target_ordinal,
                restarted_from, deploy, i32::from(deploy != current_deploy)],
        ).map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        drop(conn);
        Ok(RestartedRun {
            run_id: run_id.to_owned(),
            state: crate::operations::RunState::Queued,
            restarted_from_ordinal: restarted_from,
            pinned_to: deploy,
        })
    }

    async fn tick_due(&self) -> Result<usize, WorkflowServiceError> {
        let mut claimed = 0usize;
        for _ in 0..32 {
            let Some(claim) = self.claim_one_due()? else {
                break;
            };
            let json = self.dispatch(claim.request.clone()).await?;
            let result: StepResult = serde_json::from_str(&json).map_err(|e| {
                WorkflowServiceError::Internal(format!(
                    "parse workflow StepResult: {e}; body={json}"
                ))
            })?;
            self.apply_step_result(result)?;
            claimed += 1;
        }
        Ok(claimed)
    }

    fn claim_one_due(&self) -> Result<Option<ClaimedRun>, WorkflowServiceError> {
        let now = now_ms();
        let mut connection = self.lock_conn()?;
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let conn = &tx;
        rearm_waiting_runs_with_pending_signals(conn, now)?;
        let candidate = conn
            .query_row(
                "SELECT r.id, r.app_id, r.workflow_name, r.deploy_id, d.deploy_hash, \
                        r.input, r.started_at, r.waiting_step_key \
                   FROM workflow_runs r \
                   JOIN app_deploys d ON d.id = r.deploy_id AND d.app_id = r.app_id \
                  WHERE r.state IN ('queued','running','sleeping','waiting') \
                    AND r.wake_at IS NOT NULL \
                    AND r.wake_at <= ?1 \
                    AND r.claimed_by IS NULL \
                  ORDER BY r.wake_at, r.id \
                  LIMIT 1",
                params![now],
                |row| {
                    Ok(CandidateRun {
                        run_id: row.get(0)?,
                        app_id: row.get(1)?,
                        workflow_name: row.get(2)?,
                        deploy_id: row.get(3)?,
                        deploy_hash: row.get(4)?,
                        input: parse_json_opt(row.get::<_, Option<String>>(5)?).map_err(|e| {
                            rusqlite::Error::ToSqlConversionFailure(Box::new(SimpleError(format!(
                                "{e:?}"
                            ))))
                        })?,
                        started_at: row.get(6)?,
                        waiting_step_key: row.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(db_error)?;
        let Some(candidate) = candidate else {
            tx.commit().map_err(db_error)?;
            return Ok(None);
        };
        let dispatch_nonce = typed_id::new_workflow_dispatch_id();
        if let Some(key) = candidate.waiting_step_key.as_deref() {
            if !resolve_due_waiting_step(conn, &candidate.run_id, key, &dispatch_nonce, now)? {
                tx.commit().map_err(db_error)?;
                return Ok(None);
            }
        }
        let changed = conn
            .execute(
                "UPDATE workflow_runs \
                    SET claimed_by = ?1, lease_expires = ?2, dispatch_nonce = ?3, \
                        state = 'running', terminal_at = NULL, last_dispatch_at = ?4 \
                  WHERE id = ?5 AND claimed_by IS NULL AND state IN ('queued','running','sleeping','waiting')",
                params![
                    DEV_OWNER_ID,
                    now + 120_000,
                    dispatch_nonce,
                    now,
                    candidate.run_id
                ],
            )
            .map_err(db_error)?;
        if changed == 0 {
            tx.commit().map_err(db_error)?;
            return Ok(None);
        }
        let journal = load_journal(conn, &candidate.run_id)?;
        let claim = ClaimedRun {
            request: StepRequest {
                run_id: candidate.run_id.clone(),
                app_id: candidate.app_id.clone(),
                workflow_name: candidate.workflow_name.clone(),
                deploy_id: candidate.deploy_id,
                deploy_hash: candidate.deploy_hash,
                dispatch_nonce: dispatch_nonce.clone(),
                nonce: dispatch_nonce,
                // Always forward. There is no `compensating` arm because the
                // dev engine has no compensating phase to be in: the SELECT
                // above admits only ('queued','running','sleeping','waiting'),
                // and no dev code path writes state='compensating'. A branch on
                // `candidate.state == "compensating"` used to sit here, and it
                // was dead by construction 60 lines below its own filter -- it
                // read as compensation support to anyone scanning this file,
                // which is precisely what kept the gap invisible. Dev instead
                // reports the gap at failure time; see
                // `annotate_dev_compensation_unsupported`.
                phase: "running".to_string(),
                input: candidate.input.clone(),
                trigger: json!({
                    "input": candidate.input,
                    "runId": candidate.run_id,
                    "workflowName": candidate.workflow_name,
                    "startedAt": ms_to_datetime(candidate.started_at).to_rfc3339(),
                }),
                started_at: ms_to_datetime(candidate.started_at).to_rfc3339(),
                journal,
            },
        };
        tx.commit().map_err(db_error)?;
        Ok(Some(claim))
    }

    async fn dispatch(&self, request: StepRequest) -> Result<String, WorkflowServiceError> {
        let envelope = serde_json::to_string(&request).map_err(|e| {
            WorkflowServiceError::Internal(format!("serialize workflow StepRequest: {e}"))
        })?;
        self.executor.dispatch(&envelope).await
    }

    fn apply_step_result(&self, mut result: StepResult) -> Result<bool, WorkflowServiceError> {
        result.checkpoints.sort_by_key(|s| s.ordinal);
        let now = now_ms();
        let mut connection = self.lock_conn()?;
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let conn = &tx;
        let row = conn
            .query_row(
                "SELECT claimed_by, state, dispatch_nonce, stuck_strikes \
                   FROM workflow_runs WHERE id = ?1",
                params![result.run_id],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, i16>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(db_error)?;
        let Some((claimed_by, state, dispatch_nonce, stuck_strikes)) = row else {
            return Ok(false);
        };
        if claimed_by.as_deref() != Some(DEV_OWNER_ID)
            || dispatch_nonce.as_deref() != Some(result.dispatch_nonce.as_str())
            || state != "running"
        {
            return Ok(false);
        }

        reject_child_checkpoints_for_dev(&mut result);

        let batch_width = i16::try_from(result.checkpoints.len()).unwrap_or(i16::MAX);
        let mut wrote_checkpoints = 0usize;
        for checkpoint in &result.checkpoints {
            match insert_resolved_step(
                conn,
                checkpoint,
                &result.run_id,
                &result.dispatch_nonce,
                batch_width,
            )? {
                StepWriteOutcome::Wrote => {
                    wrote_checkpoints += 1;
                    if let Some(signal_id) = checkpoint.consumed_signal_id.as_ref() {
                        consume_signal(conn, &result.run_id, signal_id)?;
                    }
                }
                StepWriteOutcome::Noop => {}
            }
        }

        let next_ordinal = result
            .checkpoints
            .iter()
            .map(|s| s.ordinal.saturating_add(1))
            .max()
            .unwrap_or(0);
        let made_progress =
            wrote_checkpoints > 0 || !matches!(result.run_update, RunUpdate::Queued);
        let zero_progress = !made_progress;
        let next_stuck_strikes = if zero_progress {
            stuck_strikes.saturating_add(1)
        } else {
            0
        };
        if zero_progress && next_stuck_strikes >= STUCK_STRIKE_LIMIT {
            result.run_update = RunUpdate::Stalled {
                error: stalled_error(next_stuck_strikes),
            };
        }

        // Say out loud that no compensator ran. The dev engine RECORDS which
        // completed steps declared one (`insert_resolved_step` writes
        // `compensation_state = 'pending'`) and never RUNS one, so a failed saga
        // ends here looking exactly like a saga that had nothing to roll back.
        // Deployed, the same failure enters the compensating phase and undoes
        // those steps (crates/zeroship-workflow/src/apply.rs:233-243). Runs AFTER
        // the stalled override above so a StalledError is never annotated --
        // deployed does not compensate that error either.
        let unsupported = match &result.run_update {
            RunUpdate::Failed { error }
                if crate::apply::should_enter_compensation_for_error(error) =>
            {
                let pending = pending_compensator_steps(conn, &result.run_id)?;
                (!pending.is_empty()).then(|| (error.clone(), pending))
            }
            _ => None,
        };
        if let Some((error, pending)) = unsupported {
            result.run_update = RunUpdate::Failed {
                error: annotate_dev_compensation_unsupported(error, &pending),
            };
        }

        let state = result.run_update.state();
        let wake_at = result.run_update.wake_at_ms();
        let waiting_step_key = result.run_update.waiting_step_key(&result.checkpoints);
        let output = result
            .run_update
            .output()
            .map(|v| json_to_string(&v))
            .transpose()?;
        let error = result
            .run_update
            .error()
            .map(|v| json_to_string(&v))
            .transpose()?;
        let terminal_at = if is_terminal(state) { Some(now) } else { None };
        conn.execute(
            "UPDATE workflow_runs \
                SET state = ?1, output = ?2, error = ?3, wake_at = ?4, terminal_at = ?5, \
                    next_ordinal = MAX(next_ordinal, ?6), waiting_step_key = ?7, paused_from_status = NULL, \
                    stuck_strikes = ?8, output_kind = 'inline', output_hash = NULL, output_size = NULL, \
                    output_content_type = NULL, claimed_by = NULL, lease_expires = NULL, dispatch_nonce = NULL \
              WHERE id = ?9 AND claimed_by = ?10 AND dispatch_nonce = ?11 AND state = 'running'",
            params![
                state,
                output,
                error,
                wake_at,
                terminal_at,
                next_ordinal,
                waiting_step_key,
                next_stuck_strikes,
                result.run_id,
                DEV_OWNER_ID,
                result.dispatch_nonce
            ],
        )
        .map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        Ok(true)
    }

    fn lock_conn(&self) -> Result<MutexGuard<'_, Connection>, WorkflowServiceError> {
        self.conn.lock().map_err(|_| {
            WorkflowServiceError::Unavailable("workflow dev sqlite lock poisoned".to_string())
        })
    }
}

#[async_trait(?Send)]
impl WorkflowBackend for DevWorkflowBackend {
    async fn start(
        &self,
        workflow_name: String,
        body: StartOptions,
    ) -> Result<StartedRun, WorkflowServiceError> {
        self.engine.start_run(&self.app_id, &workflow_name, body)
    }

    async fn status(&self, run_id: String) -> Result<RunStatus, WorkflowServiceError> {
        self.engine.status(&self.app_id, &run_id)
    }

    async fn signal(
        &self,
        run_id: String,
        body: SignalOptions,
    ) -> Result<DeliveredSignal, WorkflowServiceError> {
        self.engine.signal(&self.app_id, &run_id, body)
    }

    async fn transition(
        &self,
        run_id: String,
        op: RunOperation,
    ) -> Result<TransitionedRun, WorkflowServiceError> {
        self.engine.transition(&self.app_id, &run_id, op)
    }

    async fn restart(
        &self,
        run_id: String,
        body: RestartOptions,
    ) -> Result<RestartedRun, WorkflowServiceError> {
        self.engine.restart(&self.app_id, &run_id, body)
    }

    async fn read_step_output(
        &self,
        run_id: String,
        name: String,
        occurrence: u32,
    ) -> Result<Vec<u8>, WorkflowServiceError> {
        let conn = self.engine.lock_conn()?;
        let output: Option<Option<String>> = conn
            .query_row(
                "SELECT s.output FROM workflow_steps s JOIN workflow_runs r ON r.id = s.run_id \
                 WHERE r.app_id = ?1 AND r.id = ?2 AND s.name = ?3 \
                 AND s.name_occurrence = ?4 AND s.state = 'completed'",
                params![self.app_id, run_id, name, occurrence],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?;
        match output {
            Some(output) => Ok(output.unwrap_or_else(|| "null".into()).into_bytes()),
            None => Err(WorkflowServiceError::NotFound(
                "workflow step output not found".into(),
            )),
        }
    }
}

fn restart_target_ordinal(
    conn: &Connection,
    run_id: &str,
    target: Option<&crate::operations::RestartTarget>,
) -> Result<u32, WorkflowServiceError> {
    let target_ordinal: u32 = if let Some(target) = target {
        conn.query_row(
                "SELECT ordinal FROM workflow_steps WHERE run_id = ?1 AND name = ?2 AND name_occurrence = ?3",
                params![run_id, target.name, target.occurrence.unwrap_or(0)], |row| row.get(0),
            ).optional().map_err(db_error)?.ok_or_else(|| {
                WorkflowServiceError::NotFound("workflow restart target not found".into())
            })?
    } else {
        0
    };
    let compensated_prefix = conn
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM workflow_steps \
             WHERE run_id = ?1 AND ordinal < ?2 AND compensation_finished_at IS NOT NULL)",
            params![run_id, target_ordinal],
            |row| row.get::<_, bool>(0),
        )
        .map_err(db_error)?;
    crate::lifecycle::RestartSafety {
        compensated_prefix,
        ..Default::default()
    }
    .check()?;

    Ok(target_ordinal)
}

fn bootstrap_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS app_deploys (
            id TEXT PRIMARY KEY,
            app_id TEXT NOT NULL,
            deploy_hash TEXT NOT NULL,
            manifest_json TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            activated_at INTEGER,
            UNIQUE(app_id, deploy_hash)
        );

        CREATE TABLE IF NOT EXISTS workflow_runs (
            id TEXT PRIMARY KEY,
            workflow_name TEXT NOT NULL,
            app_id TEXT NOT NULL,
            deploy_id TEXT NOT NULL,
            state TEXT NOT NULL CHECK (state IN ('queued','running','sleeping','waiting','paused','stalled','compensating','completed','failed','cancelled')),
            input TEXT,
            output TEXT,
            error TEXT,
            output_kind TEXT NOT NULL DEFAULT 'inline',
            output_hash TEXT,
            output_size INTEGER,
            output_content_type TEXT,
            input_hash TEXT,
            input_size INTEGER,
            input_content_type TEXT,
            journal_bytes INTEGER NOT NULL DEFAULT 0,
            blob_bytes INTEGER NOT NULL DEFAULT 0,
            wake_at INTEGER,
            claimed_by TEXT,
            lease_expires INTEGER,
            dispatch_nonce TEXT,
            last_dispatch_at INTEGER,
            concurrency INTEGER NOT NULL DEFAULT 1,
            next_ordinal INTEGER NOT NULL DEFAULT 0,
            stuck_strikes INTEGER NOT NULL DEFAULT 0,
            waiting_step_key TEXT,
            paused_from_status TEXT,
            signal_epoch INTEGER NOT NULL DEFAULT 0,
            parent_run_id TEXT,
            parent_wait_step_key TEXT,
            parent_cascade INTEGER NOT NULL DEFAULT 0,
            tree_depth INTEGER NOT NULL DEFAULT 0,
            cancel_requested INTEGER NOT NULL DEFAULT 0,
            compensation_target TEXT CHECK (compensation_target IS NULL OR compensation_target IN ('failed','cancelled')),
            compensation_outcome TEXT CHECK (compensation_outcome IS NULL OR compensation_outcome IN ('completed','partial')),
            restart_count INTEGER NOT NULL DEFAULT 0,
            restarted_at INTEGER,
            restarted_from_ordinal INTEGER,
            restarted_by TEXT,
            dedup_key TEXT,
            started_at INTEGER NOT NULL,
            terminal_at INTEGER,
            created_at INTEGER NOT NULL,
            UNIQUE(app_id, workflow_name, dedup_key)
        );
        CREATE INDEX IF NOT EXISTS workflow_runs_due_idx ON workflow_runs(wake_at, id);

        CREATE TABLE IF NOT EXISTS workflow_steps (
            run_id TEXT NOT NULL,
            ordinal INTEGER NOT NULL,
            name TEXT NOT NULL,
            name_occurrence INTEGER NOT NULL DEFAULT 0,
            kind TEXT NOT NULL CHECK (kind IN ('run','sideEffect','sleep','wait_signal','child')),
            state TEXT NOT NULL CHECK (state IN ('running','completed','failed')),
            attempt INTEGER NOT NULL DEFAULT 0,
            max_attempts INTEGER NOT NULL DEFAULT 1,
            output TEXT,
            error TEXT,
            output_kind TEXT NOT NULL DEFAULT 'inline',
            output_hash TEXT,
            output_size INTEGER,
            output_content_type TEXT,
            wake_at INTEGER,
            signal_type TEXT,
            max_signal_age_ms INTEGER,
            consumed_signal_id TEXT,
            child_run_id TEXT,
            batch_id TEXT NOT NULL,
            batch_width INTEGER NOT NULL DEFAULT 1,
            started_at INTEGER NOT NULL,
            finished_at INTEGER,
            compensation_state TEXT CHECK (compensation_state IS NULL OR compensation_state IN ('pending','running','completed','failed')),
            compensation_attempt INTEGER NOT NULL DEFAULT 0,
            compensation_max_attempts INTEGER NOT NULL DEFAULT 1,
            compensation_wake_at INTEGER,
            compensation_error TEXT,
            compensation_batch_id TEXT,
            compensation_finished_at INTEGER,
            PRIMARY KEY (run_id, ordinal),
            UNIQUE(run_id, name, name_occurrence)
        );
        CREATE INDEX IF NOT EXISTS workflow_steps_running_wake_idx ON workflow_steps(run_id, wake_at);

        CREATE TABLE IF NOT EXISTS workflow_signals (
            id TEXT PRIMARY KEY,
            run_id TEXT NOT NULL,
            type TEXT NOT NULL,
            payload TEXT,
            created_at INTEGER NOT NULL,
            consumed_by TEXT,
            origin TEXT NOT NULL DEFAULT 'app' CHECK (origin IN ('app','ingress','system')),
            delivery TEXT NOT NULL DEFAULT 'direct' CHECK (delivery IN ('direct','topic')),
            topic TEXT,
            broadcast_id TEXT,
            idempotency_key TEXT,
            provider TEXT
        );
        CREATE INDEX IF NOT EXISTS workflow_signals_pending_idx ON workflow_signals(run_id, type, consumed_by, created_at, id);
        "#,
    )
}

fn ensure_dev_deploy(conn: &Connection, app_id: &str) -> Result<String, WorkflowServiceError> {
    let candidate = typed_id::generate("dep");
    let now = now_ms();
    conn.execute(
        "INSERT INTO app_deploys (id, app_id, deploy_hash, manifest_json, created_at, activated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?5) ON CONFLICT(app_id, deploy_hash) DO NOTHING",
        params![candidate, app_id, DEV_DEPLOY_HASH, json!({"version":1,"workflows":[]}).to_string(), now],
    ).map_err(db_error)?;
    conn.query_row(
        "SELECT id FROM app_deploys WHERE app_id = ?1 AND deploy_hash = ?2",
        params![app_id, DEV_DEPLOY_HASH],
        |row| row.get(0),
    )
    .map_err(db_error)
}

fn existing_keyed_run(
    conn: &Connection,
    app_id: &str,
    workflow_name: &str,
    key: &str,
) -> Result<Option<String>, WorkflowServiceError> {
    conn.query_row(
        "SELECT id FROM workflow_runs WHERE app_id = ?1 AND workflow_name = ?2 AND dedup_key = ?3 LIMIT 1",
        params![app_id, workflow_name, key],
        |row| row.get(0),
    )
    .optional()
    .map_err(db_error)
}

fn load_journal(conn: &Connection, run_id: &str) -> Result<Vec<JournalStep>, WorkflowServiceError> {
    let mut stmt = conn
        .prepare(
            "SELECT ordinal, name, name_occurrence, kind, state, output, error, child_run_id, compensation_state \
               FROM workflow_steps \
              WHERE run_id = ?1 \
              ORDER BY ordinal",
        )
        .map_err(db_error)?;
    let rows = stmt
        .query_map(params![run_id], |row| {
            Ok(JournalStep {
                ordinal: row.get(0)?,
                name: row.get(1)?,
                name_occurrence: row.get(2)?,
                kind: row.get(3)?,
                state: row.get(4)?,
                output: parse_json_opt(row.get::<_, Option<String>>(5)?).map_err(|e| {
                    rusqlite::Error::ToSqlConversionFailure(Box::new(SimpleError(format!("{e:?}"))))
                })?,
                error: parse_json_opt(row.get::<_, Option<String>>(6)?).map_err(|e| {
                    rusqlite::Error::ToSqlConversionFailure(Box::new(SimpleError(format!("{e:?}"))))
                })?,
                child_run_id: row.get(7)?,
                compensation_state: row.get(8)?,
            })
        })
        .map_err(db_error)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(db_error)?);
    }
    Ok(out)
}

fn rearm_waiting_runs_with_pending_signals(
    conn: &Connection,
    now: i64,
) -> Result<(), WorkflowServiceError> {
    conn.execute(
        "UPDATE workflow_runs \
            SET wake_at = ?1 \
          WHERE state = 'waiting' \
            AND (wake_at IS NULL OR wake_at > ?1) \
            AND EXISTS ( \
                SELECT 1 FROM workflow_steps s \
                JOIN workflow_signals sig ON sig.run_id = workflow_runs.id \
                 AND sig.consumed_by IS NULL \
                 AND sig.type = s.signal_type \
               WHERE s.run_id = workflow_runs.id \
                 AND s.state = 'running' \
                 AND s.kind = 'wait_signal' \
            )",
        params![now],
    )
    .map_err(db_error)?;
    Ok(())
}

fn parse_waiting_step_key(key: &str) -> Result<WaitingStep, WorkflowServiceError> {
    let parts: Vec<&str> = key.split(':').collect();
    match parts.as_slice() {
        ["sleep", ordinal, name] => Ok(WaitingStep::Sleep {
            ordinal: ordinal.parse::<i32>().map_err(|_| {
                WorkflowServiceError::Internal(format!(
                    "invalid sleep waiting_step_key ordinal: {key}"
                ))
            })?,
            name: (*name).to_string(),
        }),
        ["wait", ordinal, name, signal_type] => Ok(WaitingStep::WaitSignal {
            ordinal: ordinal.parse::<i32>().map_err(|_| {
                WorkflowServiceError::Internal(format!(
                    "invalid wait waiting_step_key ordinal: {key}"
                ))
            })?,
            name: (*name).to_string(),
            signal_type: (*signal_type).to_string(),
            max_signal_age_ms: None,
        }),
        ["wait", ordinal, name, signal_type, max_age] => Ok(WaitingStep::WaitSignal {
            ordinal: ordinal.parse::<i32>().map_err(|_| {
                WorkflowServiceError::Internal(format!(
                    "invalid wait waiting_step_key ordinal: {key}"
                ))
            })?,
            name: (*name).to_string(),
            signal_type: (*signal_type).to_string(),
            max_signal_age_ms: Some(max_age.parse::<i64>().map_err(|_| {
                WorkflowServiceError::Internal(format!(
                    "invalid wait waiting_step_key max age: {key}"
                ))
            })?),
        }),
        _ => Err(WorkflowServiceError::Internal(format!(
            "unrecognized waiting_step_key: {key}"
        ))),
    }
}

fn resolve_due_waiting_step(
    conn: &Connection,
    run_id: &str,
    key: &str,
    dispatch_nonce: &str,
    now: i64,
) -> Result<bool, WorkflowServiceError> {
    match parse_waiting_step_key(key)? {
        WaitingStep::Sleep { ordinal, name } => {
            insert_resolved_step(
                conn,
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
                    topic: None,
                    child_run_id: None,
                    compensation_state: None,
                    compensation_max_attempts: 1,
                },
                run_id,
                dispatch_nonce,
                1,
            )?;
            conn.execute(
                "UPDATE workflow_runs SET waiting_step_key = NULL, wake_at = ?1 WHERE id = ?2",
                params![now, run_id],
            )
            .map_err(db_error)?;
            Ok(true)
        }
        WaitingStep::WaitSignal {
            ordinal,
            name,
            signal_type,
            max_signal_age_ms,
        } => {
            let step_deadline = conn
                .query_row(
                    "SELECT wake_at FROM workflow_steps \
                      WHERE run_id = ?1 AND ordinal = ?2 AND name = ?3 AND kind = 'wait_signal' AND state = 'running'",
                    params![run_id, ordinal, name],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .optional()
                .map_err(db_error)?
                .flatten();
            let min_created_at = max_signal_age_ms.map(|age| now.saturating_sub(age));
            let signal = conn
                .query_row(
                    "SELECT id, payload, created_at, origin, delivery, topic \
                       FROM workflow_signals \
                      WHERE run_id = ?1 \
                        AND type = ?2 \
                        AND consumed_by IS NULL \
                        AND (?3 IS NULL OR created_at >= ?3) \
                      ORDER BY created_at, id \
                      LIMIT 1",
                    params![run_id, signal_type, min_created_at],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, Option<String>>(5)?,
                        ))
                    },
                )
                .optional()
                .map_err(db_error)?;

            let Some((signal_id, payload, created_at, origin, delivery, topic)) = signal else {
                if step_deadline.is_some_and(|deadline| deadline <= now) {
                    insert_resolved_step(
                        conn,
                        &StepCheckpoint {
                            ordinal,
                            name,
                            name_occurrence: 0,
                            kind: "wait_signal".to_string(),
                            state: "failed".to_string(),
                            output: None,
                            error: Some(json!({
                                "type": "WorkflowTimeoutError",
                                "message": format!("workflow signal wait timed out for {signal_type}"),
                                "retryable": false,
                            })),
                            wake_at: None,
                            signal_type: Some(signal_type),
                            max_signal_age_ms,
                            consumed_signal_id: None,
                            topic: None,
                            child_run_id: None,
                            compensation_state: None,
                            compensation_max_attempts: 1,
                        },
                        run_id,
                        dispatch_nonce,
                        1,
                    )?;
                    conn.execute(
                        "UPDATE workflow_runs SET waiting_step_key = NULL, wake_at = ?1 WHERE id = ?2",
                        params![now, run_id],
                    )
                    .map_err(db_error)?;
                    return Ok(true);
                }
                conn.execute(
                    "UPDATE workflow_runs SET state = 'waiting', wake_at = ?2 WHERE id = ?1",
                    params![run_id, step_deadline],
                )
                .map_err(db_error)?;
                return Ok(false);
            };

            let payload_value = parse_json_opt(payload)?.unwrap_or(Value::Null);
            let received = ms_to_datetime(created_at).to_rfc3339();
            insert_resolved_step(
                conn,
                &StepCheckpoint {
                    ordinal,
                    name,
                    name_occurrence: 0,
                    kind: "wait_signal".to_string(),
                    state: "completed".to_string(),
                    output: Some(json!({
                        "id": signal_id,
                        "type": signal_type,
                        "payload": payload_value,
                        "createdAt": received,
                        "receivedAt": received,
                        "origin": origin,
                        "delivery": delivery,
                        "topic": topic,
                    })),
                    error: None,
                    wake_at: None,
                    signal_type: Some(signal_type),
                    max_signal_age_ms,
                    consumed_signal_id: Some(signal_id.clone()),
                    topic,
                    child_run_id: None,
                    compensation_state: None,
                    compensation_max_attempts: 1,
                },
                run_id,
                dispatch_nonce,
                1,
            )?;
            consume_signal(conn, run_id, &signal_id)?;
            conn.execute(
                "UPDATE workflow_runs SET waiting_step_key = NULL, wake_at = ?1 WHERE id = ?2",
                params![now, run_id],
            )
            .map_err(db_error)?;
            Ok(true)
        }
    }
}

fn insert_resolved_step(
    conn: &Connection,
    checkpoint: &StepCheckpoint,
    run_id: &str,
    batch_id: &str,
    batch_width: i16,
) -> Result<StepWriteOutcome, WorkflowServiceError> {
    let existing = conn
        .query_row(
            "SELECT state, name, kind FROM workflow_steps WHERE run_id = ?1 AND ordinal = ?2",
            params![run_id, checkpoint.ordinal],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(db_error)?;
    let resolves_running = if let Some((state, name, kind)) = existing {
        let resolves = state == "running"
            && matches!(checkpoint.state.as_str(), "completed" | "failed")
            && name == checkpoint.name
            && kind == checkpoint.kind;
        if !resolves {
            return Ok(StepWriteOutcome::Noop);
        }
        true
    } else {
        false
    };
    let now = now_ms();
    let output = checkpoint.output.as_ref().map(json_to_string).transpose()?;
    let error = checkpoint.error.as_ref().map(json_to_string).transpose()?;
    let wake_at = checkpoint.wake_at.map(datetime_to_ms);
    let compensation_state = (checkpoint.kind == "run" && checkpoint.state == "completed")
        .then(|| checkpoint.compensation_state.clone())
        .flatten();
    let changed = if resolves_running {
        conn.execute(
            "UPDATE workflow_steps \
                SET state = ?4, output = ?5, error = ?6, wake_at = ?7, signal_type = ?8, \
                    max_signal_age_ms = ?9, consumed_signal_id = ?10, child_run_id = COALESCE(child_run_id, ?11), \
                    compensation_state = ?12, compensation_max_attempts = ?13, finished_at = ?14 \
              WHERE run_id = ?1 AND ordinal = ?2 AND name = ?3 AND kind = ?15 AND state = 'running'",
            params![
                run_id,
                checkpoint.ordinal,
                checkpoint.name,
                checkpoint.state,
                output,
                error,
                wake_at,
                checkpoint.signal_type,
                checkpoint.max_signal_age_ms,
                checkpoint.consumed_signal_id,
                checkpoint.child_run_id,
                compensation_state,
                checkpoint.compensation_max_attempts.max(1),
                now,
                checkpoint.kind
            ],
        )
        .map_err(db_error)?
    } else {
        conn.execute(
            "INSERT OR IGNORE INTO workflow_steps \
            (run_id, ordinal, name, name_occurrence, kind, state, output, error, output_kind, \
             wake_at, signal_type, max_signal_age_ms, consumed_signal_id, child_run_id, batch_id, \
             batch_width, started_at, finished_at, compensation_state, compensation_max_attempts) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'inline', ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            params![
                run_id,
                checkpoint.ordinal,
                checkpoint.name,
                checkpoint.name_occurrence,
                checkpoint.kind,
                checkpoint.state,
                output,
                error,
                wake_at,
                checkpoint.signal_type,
                checkpoint.max_signal_age_ms,
                checkpoint.consumed_signal_id,
                checkpoint.child_run_id,
                batch_id,
                batch_width,
                now,
                if checkpoint.state == "running" { None } else { Some(now) },
                compensation_state,
                checkpoint.compensation_max_attempts.max(1)
            ],
        )
        .map_err(db_error)?
    };
    Ok(if changed > 0 {
        StepWriteOutcome::Wrote
    } else {
        StepWriteOutcome::Noop
    })
}

fn consume_signal(
    conn: &Connection,
    run_id: &str,
    signal_id: &str,
) -> Result<(), WorkflowServiceError> {
    let changed = conn
        .execute(
            "UPDATE workflow_signals SET consumed_by = ?1 \
         WHERE id = ?2 AND run_id = ?1 AND (consumed_by IS NULL OR consumed_by = ?1)",
            params![run_id, signal_id],
        )
        .map_err(db_error)?;
    if changed == 0 {
        return Err(WorkflowServiceError::InvalidRequest(
            "workflow signal is not available to this run".into(),
        ));
    }
    Ok(())
}

fn fold_dev_outcomes(outcomes: &[StepOutcome]) -> Result<(Vec<StepCheckpoint>, RunUpdate), String> {
    let shared_outcomes = outcomes.iter().map(shared_step_outcome).collect::<Vec<_>>();
    let (checkpoints, run_update) = crate::engine::fold_outcomes(&shared_outcomes)?;
    Ok((
        checkpoints.into_iter().map(dev_step_checkpoint).collect(),
        dev_run_update(run_update),
    ))
}

fn shared_step_outcome(outcome: &StepOutcome) -> crate::engine::StepOutcome {
    match outcome {
        StepOutcome::StepCompleted {
            ordinal,
            name,
            name_occurrence,
            step_kind,
            compensable,
            compensation_max_attempts,
            output,
        } => crate::engine::StepOutcome::StepCompleted {
            ordinal: *ordinal,
            name: name.clone(),
            name_occurrence: *name_occurrence,
            step_kind: step_kind.clone(),
            compensable: *compensable,
            compensation_max_attempts: *compensation_max_attempts,
            output: output.clone(),
            output_ref: None,
        },
        StepOutcome::StepFailed {
            ordinal,
            name,
            name_occurrence,
            error,
        } => crate::engine::StepOutcome::StepFailed {
            ordinal: *ordinal,
            name: name.clone(),
            name_occurrence: *name_occurrence,
            error: error.clone(),
        },
        StepOutcome::RunCompleted { output } => crate::engine::StepOutcome::RunCompleted {
            output: output.clone(),
            output_ref: None,
        },
        StepOutcome::RunFailed {
            ordinal,
            name,
            name_occurrence,
            error,
        } => crate::engine::StepOutcome::RunFailed {
            ordinal: *ordinal,
            name: name.clone(),
            name_occurrence: *name_occurrence,
            error: error.clone(),
        },
        StepOutcome::Sleep {
            ordinal,
            name,
            name_occurrence,
            wake_at,
        } => crate::engine::StepOutcome::Sleep {
            ordinal: *ordinal,
            name: name.clone(),
            name_occurrence: *name_occurrence,
            wake_at: *wake_at,
        },
        StepOutcome::Wait {
            ordinal,
            name,
            name_occurrence,
            wake_at,
            timeout,
            signal_type,
            max_signal_age_ms,
            consumed_signal_id,
            topic,
        } => crate::engine::StepOutcome::Wait {
            ordinal: *ordinal,
            name: name.clone(),
            name_occurrence: *name_occurrence,
            wake_at: *wake_at,
            timeout: *timeout,
            signal_type: signal_type.clone(),
            max_signal_age_ms: *max_signal_age_ms,
            consumed_signal_id: consumed_signal_id.clone(),
            topic: topic.clone(),
        },
        StepOutcome::Child {
            ordinal,
            name,
            name_occurrence,
        } => crate::engine::StepOutcome::Child {
            ordinal: *ordinal,
            name: name.clone(),
            name_occurrence: *name_occurrence,
            child_workflow_name: name.clone(),
            input: Value::Null,
            options: crate::engine::ChildWorkflowOptions::default(),
        },
    }
}

fn dev_step_checkpoint(checkpoint: crate::engine::StepCheckpoint) -> StepCheckpoint {
    StepCheckpoint {
        ordinal: checkpoint.ordinal,
        name: checkpoint.name,
        name_occurrence: checkpoint.name_occurrence,
        kind: checkpoint.kind,
        state: checkpoint.state,
        output: checkpoint.output,
        error: checkpoint.error,
        wake_at: checkpoint.wake_at,
        signal_type: checkpoint.signal_type,
        max_signal_age_ms: checkpoint.max_signal_age_ms,
        consumed_signal_id: checkpoint.consumed_signal_id,
        topic: checkpoint.topic,
        child_run_id: checkpoint.child_run_id,
        compensation_state: checkpoint.compensation_state,
        compensation_max_attempts: checkpoint.compensation_max_attempts,
    }
}

fn dev_run_update(update: crate::engine::RunUpdate) -> RunUpdate {
    match update {
        crate::engine::RunUpdate::Queued => RunUpdate::Queued,
        crate::engine::RunUpdate::Sleeping { wake_at } => RunUpdate::Sleeping { wake_at },
        crate::engine::RunUpdate::Waiting { wake_at } => RunUpdate::Waiting { wake_at },
        crate::engine::RunUpdate::Completed { output, .. } => RunUpdate::Completed { output },
        crate::engine::RunUpdate::ContinuedAsNew { .. } => RunUpdate::Completed { output: None },
        crate::engine::RunUpdate::Failed { error } => RunUpdate::Failed { error },
        crate::engine::RunUpdate::Stalled { error } => RunUpdate::Stalled { error },
        crate::engine::RunUpdate::Cancelled => RunUpdate::Cancelled,
    }
}

fn dev_child_unsupported_error() -> Value {
    json!({
        "type": "WorkflowUnsupportedError",
        "message": "child workflows are not supported by the local dev engine in this slice",
        "retryable": false,
    })
}

fn reject_child_checkpoints_for_dev(result: &mut StepResult) {
    let mut rejected = false;
    for checkpoint in &mut result.checkpoints {
        if checkpoint.kind == "child" && checkpoint.state == "running" {
            checkpoint.state = "failed".to_string();
            checkpoint.output = None;
            checkpoint.error = Some(dev_child_unsupported_error());
            checkpoint.wake_at = None;
            rejected = true;
        }
    }
    if rejected {
        result.run_update = RunUpdate::Queued;
    }
}

/// Completed `step.run` steps that declared a compensator and have not been
/// rolled back, newest first -- the order the deployed engine would walk them in
/// ("reverse journal order", docs/reference/workflows.md).
///
/// Reads the journal rather than the current batch: the compensable step usually
/// completed in an EARLIER dispatch than the one that fails, and a batch-only
/// check would report the gap for single-batch sagas only.
fn pending_compensator_steps(
    conn: &Connection,
    run_id: &str,
) -> Result<Vec<String>, WorkflowServiceError> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM workflow_steps \
              WHERE run_id = ?1 AND kind = 'run' AND state = 'completed' \
                AND compensation_state = 'pending' \
              ORDER BY ordinal DESC",
        )
        .map_err(db_error)?;
    let names = stmt
        .query_map(params![run_id], |row| row.get::<_, String>(0))
        .map_err(db_error)?
        .collect::<rusqlite::Result<Vec<String>>>()
        .map_err(db_error)?;
    Ok(names)
}

/// The dev tier's honest report for a rollback it will not perform.
///
/// Named `WorkflowUnsupportedError` deliberately: that is the same greppable
/// name `dev_child_unsupported_error` uses for the other dev-tier gap, so one
/// search finds every deployed feature the local engine declines. Unlike that
/// one it does not refuse the step -- refusing would take away the creator's
/// ability to iterate on a saga locally at all -- it annotates the failure the
/// creator already got.
///
/// It rides in the error's `compensation` slot rather than replacing the error,
/// for two reasons. The creator's own failure is still why the run failed and
/// must stay at the top level. And the deployed engine fills that same slot with
/// its rollback summary (`compensation_progress_error`,
/// crates/zeroship-workflow/src/apply.rs:447-457), so an app that reads
/// `error.compensation` gets an answer from BOTH backends -- and the answers
/// differ in `outcome`, which is the fact worth surfacing.
fn annotate_dev_compensation_unsupported(error: Value, pending: &[String]) -> Value {
    let mut error = match error {
        Value::Object(map) => Value::Object(map),
        other => json!({
            "type": "Error",
            "message": "workflow failed",
            "cause": other,
        }),
    };
    let names = pending.join(", ");
    let report = json!({
        "supported": false,
        "outcome": "not-attempted",
        "type": "WorkflowUnsupportedError",
        "message": format!(
            "the local dev workflow engine does not run compensators: {count} completed \
             step(s) declaring `compensate` were NOT rolled back ({names}). Deployed, these \
             compensators run in reverse journal order after a terminal failure. Deploy the \
             app to exercise rollback.",
            count = pending.len(),
        ),
        "pending": pending.len(),
        "steps": pending,
    });
    if let Some(obj) = error.as_object_mut() {
        obj.insert("compensation".to_string(), report);
    }
    error
}

fn default_step_kind() -> String {
    "run".to_string()
}

fn default_compensation_max_attempts() -> i32 {
    1
}

fn parse_workflow_duration_ms(raw: &str) -> Option<i64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    parse_iso_duration_ms(trimmed).or_else(|| parse_suffix_duration_ms(trimmed))
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
    raw.parse::<i64>().ok().filter(|v| *v > 0)
}

fn wake_at_from_str(raw: &str) -> Result<DateTime<Utc>, String> {
    if let Ok(ts) = DateTime::parse_from_rfc3339(raw) {
        return Ok(ts.with_timezone(&Utc));
    }
    parse_workflow_duration_ms(raw)
        .map(|ms| Utc::now() + chrono::Duration::milliseconds(ms))
        .ok_or_else(|| format!("invalid workflow wake/duration value {raw:?}"))
}

fn deserialize_wake_at<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    wake_at_from_str(&raw).map_err(de::Error::custom)
}

fn deserialize_optional_wake_at<'de, D>(deserializer: D) -> Result<Option<DateTime<Utc>>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    raw.as_deref()
        .map(wake_at_from_str)
        .transpose()
        .map_err(de::Error::custom)
}

fn deserialize_optional_wake_duration<'de, D>(
    deserializer: D,
) -> Result<Option<DateTime<Utc>>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_optional_wake_at(deserializer)
}

fn waiting_key_matches_signal(waiting_step_key: Option<&str>, signal_type: &str) -> bool {
    let Some(key) = waiting_step_key else {
        return false;
    };
    let parts: Vec<&str> = key.split(':').collect();
    matches!(parts.as_slice(), ["wait", _, _, ty] | ["wait", _, _, ty, _] if *ty == signal_type)
}

fn restored_state(conn: &Connection, run_id: &str) -> Result<String, WorkflowServiceError> {
    conn.query_row(
        "SELECT paused_from_status, waiting_step_key, wake_at FROM workflow_runs WHERE id = ?1",
        params![run_id],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        },
    )
    .map(|(paused, waiting_key, wake_at)| {
        paused.unwrap_or_else(|| {
            if waiting_key
                .as_deref()
                .is_some_and(|key| key.starts_with("wait:"))
            {
                "waiting".to_string()
            } else if waiting_key
                .as_deref()
                .is_some_and(|key| key.starts_with("sleep:"))
            {
                "sleeping".to_string()
            } else if wake_at.is_none_or(|wake| wake <= now_ms()) {
                "queued".to_string()
            } else {
                "sleeping".to_string()
            }
        })
    })
    .map_err(db_error)
}

fn status_output(
    output_kind: String,
    output_hash: Option<String>,
    output_size: Option<i64>,
    output_content_type: Option<String>,
    output: Option<String>,
) -> Result<Value, WorkflowServiceError> {
    if output_kind == "blob" {
        if let (Some(hash), Some(size)) = (output_hash, output_size) {
            return Ok(json!({
                "kind": "ref",
                "ref": format!("wfblob:sha256:{hash}"),
                "hash": hash,
                "size": size,
                "contentType": output_content_type.unwrap_or_else(|| "application/octet-stream".to_string()),
            }));
        }
    }
    Ok(parse_json_opt(output)?.unwrap_or(Value::Null))
}

fn is_terminal(state: &str) -> bool {
    matches!(state, "completed" | "failed" | "cancelled" | "stalled")
}

fn stalled_error(strikes: i16) -> Value {
    json!({
        "type": "StalledError",
        "message": "workflow made no durable progress before the liveness strike limit",
        "stuck_strikes": strikes,
        "stuck_strike_limit": STUCK_STRIKE_LIMIT,
    })
}

fn json_to_string(value: &Value) -> Result<String, WorkflowServiceError> {
    serde_json::to_string(value)
        .map_err(|e| WorkflowServiceError::Internal(format!("serialize workflow JSON: {e}")))
}

fn json_size(value: &Value) -> i64 {
    serde_json::to_vec(value).map_or(0, |bytes| bytes.len() as i64)
}

fn parse_json_opt(raw: Option<String>) -> Result<Option<Value>, WorkflowServiceError> {
    raw.map(|s| {
        serde_json::from_str(&s).map_err(|e| {
            WorkflowServiceError::Internal(format!("parse workflow JSON: {e}; body={s}"))
        })
    })
    .transpose()
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn datetime_to_ms(value: DateTime<Utc>) -> i64 {
    value.timestamp_millis()
}

fn ms_to_datetime(value: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(value)
        .single()
        .unwrap_or_else(Utc::now)
}

fn db_error(e: rusqlite::Error) -> WorkflowServiceError {
    WorkflowServiceError::Internal(format!("workflow dev sqlite: {e}"))
}

#[derive(Debug)]
struct SimpleError(String);

impl std::fmt::Display for SimpleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SimpleError {}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoDispatch;

    #[async_trait(?Send)]
    impl WorkflowExecutor for NoDispatch {
        async fn dispatch(&self, _envelope: &str) -> Result<String, WorkflowServiceError> {
            panic!("journal unit tests must not dispatch a workflow");
        }
    }

    const TEST_APP: &str = "app_devtest";

    /// A dev engine over a scratch sqlite file. No modules and no plugins: every
    /// test here drives `apply_step_result` directly with a synthesised
    /// `StepResult`, which is exactly what `tick_due` would hand it after a V8
    /// dispatch. Nothing in this module boots V8.
    fn engine(dir: &tempfile::TempDir) -> Arc<DevWorkflowEngine> {
        DevWorkflowEngine::open(dir.path().join("workflows.sqlite"), Arc::new(NoDispatch))
            .expect("open dev engine")
    }

    /// Insert a run and put it in the exact state `claim_one_due` leaves behind,
    /// so `apply_step_result`'s claim guard (dev.rs, `claimed_by`/`dispatch_nonce`
    /// /`state = 'running'`) admits the result.
    fn claimed_run(engine: &DevWorkflowEngine, run_id: &str, nonce: &str) {
        let now = now_ms();
        let conn = engine.lock_conn().expect("lock");
        let deploy_id = ensure_dev_deploy(&conn, TEST_APP).expect("deploy");
        conn.execute(
            "INSERT INTO workflow_runs \
             (id, workflow_name, app_id, deploy_id, state, input, journal_bytes, wake_at, \
              started_at, created_at, claimed_by, lease_expires, dispatch_nonce) \
             VALUES (?1, 'CompensateCase', ?2, ?3, 'running', '{}', 2, ?4, ?4, ?4, ?5, ?6, ?7)",
            params![
                run_id,
                TEST_APP,
                deploy_id,
                now,
                DEV_OWNER_ID,
                now + 120_000,
                nonce
            ],
        )
        .expect("insert run");
    }

    fn run_row(engine: &DevWorkflowEngine, run_id: &str) -> (String, Option<Value>) {
        let conn = engine.lock_conn().expect("lock");
        conn.query_row(
            "SELECT state, error FROM workflow_runs WHERE id = ?1",
            params![run_id],
            |row| {
                let state: String = row.get(0)?;
                let error: Option<String> = row.get(1)?;
                Ok((state, error))
            },
        )
        .map(|(state, error)| {
            (
                state,
                error.map(|e| serde_json::from_str::<Value>(&e).expect("error json")),
            )
        })
        .expect("run row")
    }

    fn compensable_completed(ordinal: i32, name: &str) -> StepCheckpoint {
        StepCheckpoint {
            ordinal,
            name: name.to_string(),
            name_occurrence: 0,
            kind: "run".to_string(),
            state: "completed".to_string(),
            output: Some(json!({ "reserved": "probe" })),
            error: None,
            wake_at: None,
            signal_type: None,
            max_signal_age_ms: None,
            consumed_signal_id: None,
            topic: None,
            child_run_id: None,
            // What `crates/zeroship-workflow/src/engine.rs:847-848` writes for a
            // `step.run` that declared `config.compensate`.
            compensation_state: Some("pending".to_string()),
            compensation_max_attempts: 1,
        }
    }

    fn failed_step(ordinal: i32, name: &str) -> StepCheckpoint {
        StepCheckpoint {
            ordinal,
            name: name.to_string(),
            name_occurrence: 0,
            kind: "run".to_string(),
            state: "failed".to_string(),
            output: None,
            error: Some(json!({
                "type": "PermanentError",
                "message": "probe-intentional-failure",
            })),
            wake_at: None,
            signal_type: None,
            max_signal_age_ms: None,
            consumed_signal_id: None,
            topic: None,
            child_run_id: None,
            compensation_state: None,
            compensation_max_attempts: 1,
        }
    }

    fn permanent_failure() -> Value {
        json!({
            "type": "PermanentError",
            "message": "probe-intentional-failure",
            "retryable": false,
        })
    }

    /// The defect this module exists for. A creator writes a saga, a later step
    /// fails, and the dev tier ends the run `failed` having run no compensator --
    /// which is indistinguishable, from the app's side, from a saga with nothing
    /// to roll back. The failure must SAY the rollback was not attempted.
    #[test]
    fn dev_failure_reports_that_compensators_were_not_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let engine = engine(&dir);
        claimed_run(&engine, "run_compensable", "disp_1");

        engine
            .apply_step_result(StepResult {
                run_id: "run_compensable".to_string(),
                dispatch_nonce: "disp_1".to_string(),
                checkpoints: vec![compensable_completed(0, "reserve"), failed_step(1, "boom")],
                run_update: RunUpdate::Failed {
                    error: permanent_failure(),
                },
            })
            .expect("apply");

        let (state, error) = run_row(&engine, "run_compensable");
        assert_eq!(
            state, "failed",
            "the run must still fail; rollback is not a rescue"
        );
        let error = error.expect("failed run must carry an error");

        // The original business failure is preserved -- the dev tier annotates
        // the failure, it does not replace it. Deployed does the same
        // (crates/zeroship-workflow/src/apply.rs:435-457 keeps `base` and inserts
        // its rollback summary under the same `compensation` key).
        assert_eq!(error["type"], "PermanentError");
        assert_eq!(error["message"], "probe-intentional-failure");

        let comp = &error["compensation"];
        assert!(
            !comp.is_null(),
            "dev failure carried no compensation report at all: {error}"
        );
        assert_eq!(
            comp["supported"],
            json!(false),
            "must say dev cannot compensate"
        );
        assert_eq!(comp["outcome"], json!("not-attempted"));
        assert_eq!(
            comp["type"], "WorkflowUnsupportedError",
            "must be the same named, greppable error the dev tier uses for its other \
             unsupported feature (dev_child_unsupported_error)"
        );
        assert_eq!(comp["pending"], json!(1));
        assert_eq!(comp["steps"], json!(["reserve"]));
        let message = comp["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("dev"),
            "the message must name the dev tier as the reason: {message}"
        );
    }

    /// The compensable step usually completes in an EARLIER dispatch than the one
    /// that fails (a sleep, a signal wait, or just a second `step.run` batch). The
    /// report must be driven by the journal, not by what happens to be in the
    /// current batch -- otherwise the honest error appears only for single-batch
    /// sagas, which is the shape least likely to be written by hand.
    #[test]
    fn pending_compensator_from_an_earlier_batch_is_still_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let engine = engine(&dir);
        claimed_run(&engine, "run_two_batch", "disp_a");

        engine
            .apply_step_result(StepResult {
                run_id: "run_two_batch".to_string(),
                dispatch_nonce: "disp_a".to_string(),
                checkpoints: vec![compensable_completed(0, "reserve")],
                run_update: RunUpdate::Queued,
            })
            .expect("apply batch 1");

        // Re-claim for the second dispatch, as `claim_one_due` would.
        {
            let conn = engine.lock_conn().expect("lock");
            conn.execute(
                "UPDATE workflow_runs SET state = 'running', claimed_by = ?2, dispatch_nonce = ?3 \
                  WHERE id = ?1",
                params!["run_two_batch", DEV_OWNER_ID, "disp_b"],
            )
            .expect("reclaim");
        }

        engine
            .apply_step_result(StepResult {
                run_id: "run_two_batch".to_string(),
                dispatch_nonce: "disp_b".to_string(),
                checkpoints: vec![failed_step(1, "boom")],
                run_update: RunUpdate::Failed {
                    error: permanent_failure(),
                },
            })
            .expect("apply batch 2");

        let (state, error) = run_row(&engine, "run_two_batch");
        assert_eq!(state, "failed");
        let comp = &error.expect("error")["compensation"];
        assert_eq!(comp["supported"], json!(false));
        assert_eq!(comp["steps"], json!(["reserve"]));
    }

    /// A run with no compensable step must not grow a compensation report. If it
    /// did, every ordinary dev failure would claim a rollback gap it does not
    /// have, and the report would stop meaning anything.
    #[test]
    fn plain_failure_gets_no_compensation_report() {
        let dir = tempfile::tempdir().expect("tempdir");
        let engine = engine(&dir);
        claimed_run(&engine, "run_plain", "disp_p");

        engine
            .apply_step_result(StepResult {
                run_id: "run_plain".to_string(),
                dispatch_nonce: "disp_p".to_string(),
                checkpoints: vec![failed_step(0, "boom")],
                run_update: RunUpdate::Failed {
                    error: permanent_failure(),
                },
            })
            .expect("apply");

        let (state, error) = run_row(&engine, "run_plain");
        assert_eq!(state, "failed");
        assert!(
            error.expect("error").get("compensation").is_none(),
            "a run with no compensator must not report one"
        );
    }

    /// Deployed skips compensation for `NondeterministicError` and `StalledError`
    /// (`should_enter_compensation_for_error`, crates/zeroship-workflow/src/apply.rs
    /// :461-466). Dev must skip the REPORT on the same errors, or it would claim
    /// deployed would have rolled back when deployed would not.
    #[test]
    fn nondeterministic_failure_gets_no_compensation_report() {
        let dir = tempfile::tempdir().expect("tempdir");
        let engine = engine(&dir);
        claimed_run(&engine, "run_nondet", "disp_n");

        engine
            .apply_step_result(StepResult {
                run_id: "run_nondet".to_string(),
                dispatch_nonce: "disp_n".to_string(),
                checkpoints: vec![compensable_completed(0, "reserve")],
                run_update: RunUpdate::Failed {
                    error: json!({
                        "type": "NondeterministicError",
                        "message": "journal diverged from replay",
                    }),
                },
            })
            .expect("apply");

        let (state, error) = run_row(&engine, "run_nondet");
        assert_eq!(state, "failed");
        assert!(
            error.expect("error").get("compensation").is_none(),
            "a NondeterministicError is not compensated deployed either"
        );
    }

    /// What these tests do NOT catch: they do not prove the dev engine's V8
    /// dispatch ever emits `compensable: true` for a real `step.run`, and they do
    /// not compare against a live deployed run. Those two are the job of
    /// `examples/workflow-probe/tests/workflows.test.ts`, which drives both backends.
    ///
    /// This one is the tripwire under the report itself: the report is honest
    /// only because dev genuinely never compensates. A run parked in
    /// `compensating` is invisible to `claim_one_due`, so it can never be
    /// dispatched and its compensators can never run. That is also why the
    /// `phase: "compensating"` branch that used to sit in `claim_one_due` was
    /// dead. If dev ever grows a compensating phase this fails, and
    /// `dev_compensation_unsupported` must go with it.
    #[test]
    fn dev_never_claims_a_compensating_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let engine = engine(&dir);
        claimed_run(&engine, "run_comp_state", "disp_c");
        {
            let conn = engine.lock_conn().expect("lock");
            conn.execute(
                "UPDATE workflow_runs \
                    SET state = 'compensating', claimed_by = NULL, dispatch_nonce = NULL, wake_at = ?2 \
                  WHERE id = ?1",
                params!["run_comp_state", now_ms() - 1_000],
            )
            .expect("park compensating");
        }

        assert!(
            engine.claim_one_due().expect("claim").is_none(),
            "dev claimed a compensating run -- it now has a compensating phase, so the \
             not-attempted report is no longer true"
        );
    }

    #[test]
    fn checkpoint_batch_rolls_back_when_the_run_transition_fails() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(&dir);
        claimed_run(&engine, "run_atomic", "dispatch_atomic");
        engine
            .lock_conn()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_apply BEFORE UPDATE OF next_ordinal ON workflow_runs \
             BEGIN SELECT RAISE(ABORT, 'injected apply failure'); END;",
            )
            .unwrap();
        let result = StepResult {
            run_id: "run_atomic".into(),
            dispatch_nonce: "dispatch_atomic".into(),
            checkpoints: vec![compensable_completed(0, "reserve")],
            run_update: RunUpdate::Completed {
                output: Some(json!(true)),
            },
        };
        assert!(matches!(
            engine.apply_step_result(result.clone()),
            Err(WorkflowServiceError::Internal(_))
        ));
        {
            let conn = engine.lock_conn().unwrap();
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM workflow_steps WHERE run_id = 'run_atomic'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 0);
            conn.execute_batch("DROP TRIGGER reject_apply").unwrap();
        }
        assert_eq!(run_row(&engine, "run_atomic").0, "running");
        assert!(engine.apply_step_result(result).unwrap());
        assert_eq!(run_row(&engine, "run_atomic").0, "completed");
    }

    #[test]
    fn an_execution_cannot_consume_another_apps_signal() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(&dir);
        claimed_run(&engine, "run_a", "dispatch_a");
        claimed_run(&engine, "run_b", "dispatch_b");
        {
            let conn = engine.lock_conn().unwrap();
            conn.execute(
                "UPDATE workflow_runs SET app_id = 'app_other' WHERE id = 'run_b'",
                [],
            )
            .unwrap();
            conn.execute("INSERT INTO workflow_signals (id, run_id, type, created_at) VALUES ('signal_b', 'run_b', 'approved', 0)", []).unwrap();
        }
        let mut checkpoint = compensable_completed(0, "forged");
        checkpoint.consumed_signal_id = Some("signal_b".into());
        assert!(matches!(
            engine.apply_step_result(StepResult {
                run_id: "run_a".into(),
                dispatch_nonce: "dispatch_a".into(),
                checkpoints: vec![checkpoint],
                run_update: RunUpdate::Queued,
            }),
            Err(WorkflowServiceError::InvalidRequest(_))
        ));
        let conn = engine.lock_conn().unwrap();
        let consumed: Option<String> = conn
            .query_row(
                "SELECT consumed_by FROM workflow_signals WHERE id = 'signal_b'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(consumed, None);
        let checkpoints: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM workflow_steps WHERE run_id = 'run_a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(checkpoints, 0);
    }
}
