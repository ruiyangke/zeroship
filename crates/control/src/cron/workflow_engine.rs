//! Durable-workflow engine scheduler.
//!
//! This cron owns only the control-plane scheduling core: due timer bookkeeping,
//! the dispatch seam, and idempotent scheduler-store ack registration.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use compio_postgres::GenericClient;
use uuid::Uuid;
use zeroship_plugin_workflow::advance::{
    WorkflowAdvanceNackKind, WorkflowAdvanceRegistration, WorkflowAdvanceResponse,
    WorkflowRunDispatchRequest,
};
use zeroship_plugin_workflow::apply;
use zeroship_plugin_workflow::engine;
use zeroship_plugin_workflow::errors::WorkflowError;
use zeroship_plugin_workflow::store::pg::{PgStore, WorkflowTables};
use zeroship_workflow_scheduler::{
    self as workflow_scheduler, SchedulerConfig, TimerWheel, WakeHandle,
    WorkflowSchedulerStore, WorkflowSchedulerStoreError, WORKFLOW_ADVANCE_PATH,
};

use crate::registry::RegistryError;
use crate::workflow_limits;
use crate::workflow_rollout;
use crate::{AppState, Registry};

pub use engine::{
    child_dedup_key, child_signal_type, RunUpdate, StepCheckpoint, StepOutcome, StepRequest, StepResult,
    WorkflowEngineConfig, WorkflowOutputRef, DEFAULT_MAX_CHILD_DEPTH,
    DEFAULT_MAX_LIVE_DESCENDANTS, DEFAULT_MAX_START_MANY_BATCH,
};

/// Default tick cadence. Workflow wake latency is intentionally a scheduler
/// knob, not a correctness bound; DW-23 will measure and tune it.
pub const DEFAULT_TICK_SECS: u64 = 1;
const GATEWAY_DISPATCH_TIMEOUT: Duration = Duration::from_secs(35);
static INFLIGHT_DISPATCHES: AtomicUsize = AtomicUsize::new(0);
static PROVISIONED_WORKFLOW_JOURNALS: OnceLock<Mutex<HashSet<Uuid>>> = OnceLock::new();

#[doc(hidden)]
pub fn reset_inflight_dispatches_for_test() {
    INFLIGHT_DISPATCHES.store(0, Ordering::SeqCst);
}

pub(crate) fn journal_sql(tables: &WorkflowTables, sql: &str) -> String {
    sql.replace("zeroship.workflow_runs", &tables.runs)
        .replace("zeroship.workflow_steps", &tables.steps)
        .replace("zeroship.workflow_signals", &tables.signals)
        .replace("zeroship.workflow_subscriptions", &tables.subscriptions)
        .replace("zeroship.workflow_blobs", &tables.blobs)
}

pub(crate) async fn provision_tables<C>(
    conn: &C,
    app_id: &Uuid,
) -> Result<WorkflowTables, RegistryError>
where
    C: GenericClient + Sync,
{
    let tables = WorkflowTables::for_app_id(app_id);
    let cache = PROVISIONED_WORKFLOW_JOURNALS.get_or_init(|| Mutex::new(HashSet::new()));
    if cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(app_id)
    {
        return Ok(tables);
    }

    PgStore::provision(conn, app_id)
        .await
        .map_err(workflow_error_to_registry)?;
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(*app_id);
    Ok(tables)
}

pub(crate) async fn existing_tables<C>(
    conn: &C,
    app_id: &Uuid,
) -> Result<Option<WorkflowTables>, RegistryError>
where
    C: GenericClient + Sync,
{
    let tables = WorkflowTables::for_app_id(app_id);
    let rows = conn
        .query("SELECT to_regclass($1) IS NOT NULL AS exists", &[&tables.runs])
        .await
        .map_err(RegistryError::from)?;
    if rows.first().is_some_and(|row| row.get("exists")) {
        Ok(Some(tables))
    } else {
        Ok(None)
    }
}

pub(crate) async fn workflow_app_ids<C>(conn: &C) -> Result<Vec<Uuid>, RegistryError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            "SELECT id \
               FROM zeroship.apps \
              ORDER BY id",
            &[],
        )
        .await
        .map_err(RegistryError::from)?;
    Ok(rows.into_iter().map(|row| row.get("id")).collect())
}

async fn find_run_tables<C>(conn: &C, run_id: &str) -> Result<Option<WorkflowTables>, RegistryError>
where
    C: GenericClient + Sync,
{
    for app_id in workflow_app_ids(conn).await? {
        let Some(tables) = existing_tables(conn, &app_id).await? else {
            continue;
        };
        let sql = format!("SELECT 1 FROM {} WHERE id = $1 LIMIT 1", tables.runs);
        let rows = conn
            .query(&sql, &[&run_id])
            .await
            .map_err(RegistryError::from)?;
        if !rows.is_empty() {
            return Ok(Some(tables));
        }
    }
    Ok(None)
}

#[async_trait(?Send)]
pub trait StepDispatcher: Send + Sync {
    async fn dispatch(&self, request: WorkflowRunDispatchRequest) -> DispatchOutcome;
}

#[derive(Debug, Clone)]
pub enum DispatchOutcome {
    Completed(WorkflowAdvanceResponse),
    Backpressure {
        run_id: String,
        reason: String,
    },
}

impl DispatchOutcome {
    fn backpressure(request: &WorkflowRunDispatchRequest, reason: impl Into<String>) -> Self {
        Self::Backpressure {
            run_id: request.run_id.clone(),
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Default)]
pub struct StubStepDispatcher;

#[async_trait(?Send)]
impl StepDispatcher for StubStepDispatcher {
    async fn dispatch(&self, request: WorkflowRunDispatchRequest) -> DispatchOutcome {
        DispatchOutcome::Completed(WorkflowAdvanceResponse::ack(
            request.run_id.clone(),
            vec![WorkflowAdvanceRegistration::preserve(
                request.run_id,
                request.app_id,
            )],
        ))
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
    async fn dispatch(&self, request: WorkflowRunDispatchRequest) -> DispatchOutcome {
        if self.gateway_url.is_empty() {
            return DispatchOutcome::backpressure(&request, "gateway URL is not configured");
        }
        let body = match serde_json::to_vec(&request) {
            Ok(body) => body,
            Err(e) => {
                return DispatchOutcome::backpressure(
                    &request,
                    format!("serialize WorkflowRunDispatchRequest: {e}"),
                );
            }
        };
        let url = format!("{}{}", self.gateway_url, WORKFLOW_ADVANCE_PATH);
        let client = cyper::Client::new();
        let builder = match client.post(&url) {
            Ok(builder) => builder,
            Err(e) => {
                return DispatchOutcome::backpressure(
                    &request,
                    format!("build gateway workflow advance request: {e}"),
                );
            }
        };
        let builder = match builder.header("content-type", "application/json") {
            Ok(builder) => builder,
            Err(e) => {
                return DispatchOutcome::backpressure(
                    &request,
                    format!("set gateway workflow advance content-type: {e}"),
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
                    format!("gateway workflow advance transport: {e}"),
                );
            }
            Err(_) => {
                return DispatchOutcome::backpressure(
                    &request,
                    "gateway workflow advance timeout",
                );
            }
        };

        let status = response.status().as_u16();
        let bytes = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(e) => {
                return DispatchOutcome::backpressure(
                    &request,
                    format!("read gateway workflow advance body: {e}"),
                );
            }
        };

        if status == 402 || status >= 500 {
            let body_snippet: String = String::from_utf8_lossy(&bytes).chars().take(200).collect();
            return DispatchOutcome::backpressure(
                &request,
                format!("gateway workflow advance HTTP {status}: {body_snippet}"),
            );
        }
        if !(200..300).contains(&status) {
            let body_snippet: String = String::from_utf8_lossy(&bytes).chars().take(200).collect();
            return DispatchOutcome::backpressure(
                &request,
                format!("gateway workflow advance rejected HTTP {status}: {body_snippet}"),
            );
        }

        match serde_json::from_slice::<WorkflowAdvanceResponse>(&bytes) {
            Ok(response) if response.is_ack() || response.is_nack() => {
                DispatchOutcome::Completed(response)
            }
            Ok(_) => DispatchOutcome::backpressure(
                &request,
                "parse gateway workflow advance ack: response is neither ack nor nack",
            ),
            Err(e) => DispatchOutcome::backpressure(
                &request,
                format!("parse gateway workflow advance ack: {e}"),
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

/// Low-frequency safety net for lost scheduler acks.
#[allow(clippy::future_not_send)]
pub async fn run_inflight_reaper(state: Arc<AppState>, tick_secs: u64) {
    tracing::info!(tick_secs, "control workflow inflight reaper starting");
    let store = WorkflowSchedulerStore::new(state.registry.workflow_store_db_url().to_string());
    if let Err(e) = store.provision().await {
        tracing::error!(error = %e, "workflow inflight reaper provision failed");
        return;
    }
    match reconcile_scheduler_from_journal_with_store(&store, &state.registry, false).await {
        Ok(n) if n > 0 => tracing::info!(registered = n, "workflow scheduler DR reconcile seeded timers"),
        Ok(_) => {}
        Err(e) => tracing::error!(error = %e, "workflow scheduler DR reconcile failed"),
    }
    loop {
        match reap_parked_cancel_requested_batch(&store, &state.registry, 64).await {
            Ok(n) if n > 0 => {
                tracing::info!(registered = n, "workflow parked-cancel reaper registered due timers")
            }
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "workflow parked-cancel reaper tick failed"),
        }
        match reconcile_scheduler_from_journal_with_store(&store, &state.registry, true).await {
            Ok(n) if n > 0 => {
                tracing::info!(registered = n, "workflow scheduler DR reconcile seeded due timers")
            }
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "workflow scheduler DR reconcile tick failed"),
        }
        match reap_lapsed_inflight_once(
            &store,
            &state,
            Arc::new(GatewayStepDispatcher::new(state.gateway_url.clone())),
            WorkflowEngineConfig::default(),
            64,
        )
        .await
        {
            Ok(n) if n > 0 => tracing::info!(redispatched = n, "workflow inflight reaper redispatched runs"),
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "workflow inflight reaper tick failed"),
        }
        compio::time::sleep(Duration::from_secs(tick_secs)).await;
    }
}

/// One-shot control-mediated scheduler seed from per-app workflow journals.
///
/// The scheduler store never reads the journal directly. Control owns the
/// platform connection, provisions every app-local journal, and uses the
/// scheduler's normal ack API to register current wake rows.
#[allow(clippy::future_not_send)]
pub async fn reconcile_scheduler_from_journal(state: &AppState) -> Result<usize, RegistryError> {
    let store = WorkflowSchedulerStore::new(state.registry.workflow_store_db_url().to_string());
    store
        .provision()
        .await
        .map_err(scheduler_store_error_to_registry)?;
    reconcile_scheduler_from_journal_with_store(&store, &state.registry, false).await
}

#[allow(clippy::future_not_send)]
async fn reconcile_scheduler_from_journal_with_store(
    scheduler_store: &WorkflowSchedulerStore,
    registry: &Registry,
    due_only: bool,
) -> Result<usize, RegistryError> {
    let conn = registry.conn().await?;
    let mut registered = 0usize;
    for app_id in workflow_app_ids(&conn).await? {
        let Some(tables) = existing_tables(&conn, &app_id).await? else {
            continue;
        };
        let sql = format!(
            "SELECT id, app_id, wake_at, cancel_requested \
               FROM {} \
              WHERE state IN ('queued','running','sleeping','waiting','compensating') \
                AND wake_at IS NOT NULL \
                AND ($1::bool = false OR wake_at <= now() OR cancel_requested) \
              ORDER BY wake_at, id",
            tables.runs
        );
        let rows = conn.query(&sql, &[&due_only]).await.map_err(RegistryError::from)?;
        for row in rows {
            let run_id: String = row.get("id");
            let app_id: Uuid = row.get("app_id");
            let cancel_requested: bool = row.get("cancel_requested");
            let wake_at: DateTime<Utc> = if cancel_requested {
                Utc::now()
            } else {
                row.get("wake_at")
            };
            scheduler_store
                .ack_register_next(&run_id, app_id, wake_at)
                .await
                .map_err(scheduler_store_error_to_registry)?;
            registered = registered.saturating_add(1);
        }
    }
    Ok(registered)
}

/// Pull parked cancel requests into the scheduler store as due-now timers.
///
/// Cascade cancellation updates the per-app journal first. The scheduler store
/// remains the timer authority, so a sleeping/waiting child with an old future
/// store row must be repaired before the normal due-timer scan can dispatch it.
#[allow(clippy::future_not_send)]
pub async fn reap_parked_cancel_requested_batch(
    scheduler_store: &WorkflowSchedulerStore,
    registry: &Registry,
    limit: i64,
) -> Result<usize, RegistryError> {
    if limit <= 0 {
        return Ok(0);
    }

    let conn = registry.conn().await?;
    let mut registered = 0usize;
    for app_id in workflow_app_ids(&conn).await? {
        if registered >= usize::try_from(limit).unwrap_or(usize::MAX) {
            break;
        }
        let Some(tables) = existing_tables(&conn, &app_id).await? else {
            continue;
        };
        let remaining = limit.saturating_sub(i64::try_from(registered).unwrap_or(i64::MAX));
        if remaining <= 0 {
            break;
        }
        let sql = format!(
            "WITH candidates AS ( \
                 SELECT id \
                   FROM {runs} \
                  WHERE cancel_requested \
                    AND state IN ('queued','sleeping','waiting','compensating') \
                  ORDER BY wake_at NULLS FIRST, id \
                  LIMIT $1 \
                  FOR UPDATE SKIP LOCKED \
             ) \
             UPDATE {runs} r \
                SET wake_at = now() \
               FROM candidates c \
              WHERE r.id = c.id \
              RETURNING r.id, r.app_id, r.wake_at",
            runs = tables.runs,
        );
        let rows = conn
            .query(&sql, &[&remaining])
            .await
            .map_err(RegistryError::from)?;
        for row in rows {
            let run_id: String = row.get("id");
            let app_id: Uuid = row.get("app_id");
            let wake_at: DateTime<Utc> = row.get("wake_at");
            scheduler_store
                .ack_register_next(&run_id, app_id, wake_at)
                .await
                .map_err(scheduler_store_error_to_registry)?;
            registered = registered.saturating_add(1);
        }
    }
    Ok(registered)
}

/// Control-hosted timer-authority tick.
///
/// Timer state lives in the scheduler store; control claims due rows and owns
/// dispatch/ack handling until that loop moves into the standalone scheduler.
#[allow(clippy::future_not_send)]
pub async fn tick(state: &AppState) -> Result<usize, RegistryError> {
    let store = WorkflowSchedulerStore::new(state.registry.workflow_store_db_url().to_string());
    store
        .provision()
        .await
        .map_err(scheduler_store_error_to_registry)?;
    fire_once(
        &store,
        state,
        Arc::new(GatewayStepDispatcher::new(state.gateway_url.clone())),
        WorkflowEngineConfig::default(),
    )
    .await
}

/// Fire due scheduler timers and dispatch claimed workflow runs.
#[allow(clippy::future_not_send)]
pub async fn fire_once<D>(
    scheduler_store: &WorkflowSchedulerStore,
    state: &AppState,
    dispatcher: Arc<D>,
    config: WorkflowEngineConfig,
) -> Result<usize, RegistryError>
where
    D: StepDispatcher + 'static,
{
    {
        let conn = state.registry.conn().await?;
        if workflow_rollout::dispatch_paused(&conn).await? {
            return Ok(0);
        }
    }

    let fair_limit = config
        .batch_apps
        .saturating_mul(config.per_app_fair_limit)
        .max(1);
    let per_app_fair_limit = config.per_app_fair_limit.max(1);
    let per_app_fair_limit_usize = usize::try_from(per_app_fair_limit).unwrap_or(usize::MAX);
    reap_parked_cancel_requested_batch(scheduler_store, &state.registry, per_app_fair_limit).await?;
    let mut scheduler_config = SchedulerConfig::default();
    scheduler_config.max_due_per_tick = per_app_fair_limit_usize;
    scheduler_config.max_loaded_timers = fair_limit;

    let mut wheel = TimerWheel::new(WakeHandle::new());
    let mut claimed = 0usize;
    while claimed < per_app_fair_limit_usize {
        let current = INFLIGHT_DISPATCHES.load(Ordering::SeqCst);
        let available_dispatch_slots = config.max_inflight_dispatch.saturating_sub(current);
        if available_dispatch_slots == 0 {
            break;
        }
        scheduler_config.max_due_per_tick =
            (per_app_fair_limit_usize - claimed).min(available_dispatch_slots);
        let fired = workflow_scheduler::fire_once(scheduler_store, &mut wheel, &scheduler_config)
            .await
            .map_err(scheduler_error_to_registry)?;
        if fired.is_empty() {
            break;
        }

        for timer in fired {
            claimed += 1;
            spawn_dispatch(
                scheduler_store.clone(),
                state.registry.clone(),
                dispatcher.clone(),
                WorkflowRunDispatchRequest {
                    run_id: timer.run_id,
                    app_id: timer.app_id,
                },
            );
        }
    }
    Ok(claimed)
}

/// Re-dispatch scheduler rows whose dispatch/apply/register ack was lost.
///
/// The recurring scan reads only `workflow_scheduler.inflight`; each lapsed row
/// is re-dispatched by run reference. The worker re-claims the tenant-journal row.
#[allow(clippy::future_not_send)]
pub async fn reap_lapsed_inflight_once<D>(
    scheduler_store: &WorkflowSchedulerStore,
    _state: &AppState,
    dispatcher: Arc<D>,
    config: WorkflowEngineConfig,
    limit: i64,
) -> Result<usize, RegistryError>
where
    D: StepDispatcher + 'static,
{
    if limit <= 0 {
        return Ok(0);
    }
    let now = Utc::now();
    let next_deadline = now + chrono::Duration::milliseconds(config.claim_ttl_ms);
    let lapsed = scheduler_store
        .claim_lapsed_inflight(now, limit, next_deadline)
        .await
        .map_err(scheduler_store_error_to_registry)?;
    let mut redispatched = 0usize;
    for timer in lapsed {
        redispatched = redispatched.saturating_add(1);
        spawn_dispatch(
            scheduler_store.clone(),
            _state.registry.clone(),
            dispatcher.clone(),
            WorkflowRunDispatchRequest {
                run_id: timer.run_id,
                app_id: timer.app_id,
            },
        );
    }
    Ok(redispatched)
}

pub(crate) async fn cascade_cancel_children_for_app<C>(
    conn: &C,
    tables: &WorkflowTables,
    parent_run_id: &str,
) -> Result<Vec<String>, RegistryError>
where
    C: GenericClient + Sync,
{
    let sql = cascade_cancel_children_sql(tables);
    let rows = conn
        .query(&sql, &[&parent_run_id])
        .await
        .map_err(|e| workflow_error_to_registry(WorkflowError::from(e)))?;
    Ok(rows.into_iter().map(|row| row.get("id")).collect())
}

fn cascade_cancel_children_sql(tables: &WorkflowTables) -> String {
    journal_sql(
        tables,
        "WITH locked_children AS MATERIALIZED ( \
             SELECT id \
               FROM zeroship.workflow_runs \
              WHERE parent_run_id = $1 \
                AND parent_cascade \
                AND state NOT IN ('completed','failed','cancelled','stalled') \
              ORDER BY id \
              FOR UPDATE \
         ) \
         UPDATE zeroship.workflow_runs AS r \
            SET cancel_requested = true, wake_at = now() \
           FROM locked_children \
          WHERE r.id = locked_children.id \
          RETURNING r.id",
    )
}

pub(crate) async fn cascade_cancel_children<C>(
    conn: &C,
    parent_run_id: &str,
) -> Result<Vec<String>, RegistryError>
where
    C: GenericClient + Sync,
{
    let Some(tables) = find_run_tables(conn, parent_run_id).await? else {
        return Ok(Vec::new());
    };
    cascade_cancel_children_for_app(conn, &tables, parent_run_id).await
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
    Child {
        ordinal: i32,
        name: String,
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
        ["child", ordinal, name] => {
            let ordinal = ordinal.parse::<i32>().map_err(|_| {
                RegistryError::InvalidInput(format!("invalid child waiting_step_key ordinal: {key}"))
            })?;
            Ok(WaitingStep::Child {
                ordinal,
                name: (*name).to_string(),
            })
        }
        _ => Err(RegistryError::InvalidInput(format!(
            "unrecognized waiting_step_key: {key}"
        ))),
    }
}

fn spawn_dispatch<D>(
    scheduler_store: WorkflowSchedulerStore,
    registry: Registry,
    dispatcher: Arc<D>,
    request: WorkflowRunDispatchRequest,
) where
    D: StepDispatcher + 'static,
{
    INFLIGHT_DISPATCHES.fetch_add(1, Ordering::SeqCst);
    compio::runtime::spawn(async move {
        let mut inflight_guard = InflightDispatchGuard::new();
        let dispatch_run_id = request.run_id.clone();
        let outcome = dispatcher.dispatch(request).await;
        match outcome {
            DispatchOutcome::Completed(response) if response.is_ack() => {
                let run_id = response
                    .run_id
                    .clone()
                    .unwrap_or_else(|| dispatch_run_id.clone());
                inflight_guard.release();
                if let Err(e) = apply_workflow_advance_ack(&scheduler_store, &registry, &response).await {
                    tracing::error!(error = %e, run_id = %run_id, "workflow_engine: scheduler worker ack registration failed");
                }
            }
            DispatchOutcome::Completed(response) if response.is_nack() => {
                let run_id = response
                    .run_id
                    .clone()
                    .unwrap_or_else(|| dispatch_run_id.clone());
                let reason = response
                    .reason
                    .clone()
                    .unwrap_or_else(|| "worker workflow advance nack".to_string());
                let kind = response
                    .nack_kind
                    .unwrap_or(WorkflowAdvanceNackKind::ApplyFailed);
                match kind {
                    WorkflowAdvanceNackKind::ClaimLost => tracing::debug!(
                        run_id = %run_id,
                        reason = %reason,
                        "workflow_engine: worker claim lost; leaving scheduler inflight for reaper"
                    ),
                    WorkflowAdvanceNackKind::Backpressure => tracing::warn!(
                        run_id = %run_id,
                        reason = %reason,
                        "workflow_engine: worker backpressure; leaving scheduler inflight for reaper"
                    ),
                    WorkflowAdvanceNackKind::Deadlock
                    | WorkflowAdvanceNackKind::Invalid
                    | WorkflowAdvanceNackKind::ApplyFailed => tracing::error!(
                        run_id = %run_id,
                        reason = %reason,
                        ?kind,
                        "workflow_engine: worker workflow advance nack; leaving scheduler inflight for reaper"
                    ),
                }
            }
            DispatchOutcome::Completed(response) => {
                tracing::error!(
                    ?response,
                    run_id = %dispatch_run_id,
                    "workflow_engine: invalid workflow advance response; leaving scheduler inflight for reaper"
                );
            }
            DispatchOutcome::Backpressure { run_id, reason } => {
                tracing::warn!(
                    run_id = %run_id,
                    reason = %reason,
                    "workflow_engine: gateway backpressure; leaving scheduler inflight for reaper"
                );
            }
        }
    })
    .detach();
}

async fn apply_workflow_advance_ack(
    scheduler_store: &WorkflowSchedulerStore,
    registry: &Registry,
    response: &WorkflowAdvanceResponse,
) -> Result<(), RegistryError> {
    if response.registrations.is_empty() {
        return Err(RegistryError::InvalidInput(
            "workflow advance ack missing registrations".to_string(),
        ));
    }

    for registration in &response.registrations {
        apply_workflow_advance_registration(scheduler_store, registry, registration).await?;
    }
    Ok(())
}

async fn apply_workflow_advance_registration(
    scheduler_store: &WorkflowSchedulerStore,
    registry: &Registry,
    registration: &WorkflowAdvanceRegistration,
) -> Result<(), RegistryError> {
    if let Some(next_wake_at) = registration.next_wake_at.clone() {
        scheduler_store
            .ack_register_next(&registration.run_id, registration.app_id, next_wake_at)
            .await
            .map_err(scheduler_store_error_to_registry)?;
    } else if registration.terminal {
        scheduler_store
            .ack_terminal(&registration.run_id)
            .await
            .map_err(scheduler_store_error_to_registry)?;
    }
    sync_scheduler_for_run(scheduler_store, registry, &registration.run_id).await
}

struct InflightDispatchGuard {
    released: bool,
}

impl InflightDispatchGuard {
    fn new() -> Self {
        Self { released: false }
    }

    fn release(&mut self) {
        if !self.released {
            INFLIGHT_DISPATCHES.fetch_sub(1, Ordering::SeqCst);
            self.released = true;
        }
    }
}

impl Drop for InflightDispatchGuard {
    fn drop(&mut self) {
        self.release();
    }
}

fn workflow_error_to_registry(error: WorkflowError) -> RegistryError {
    match error {
        WorkflowError::Invalid(msg) => RegistryError::InvalidInput(msg),
        WorkflowError::CompensableCarry(msg) => RegistryError::InvalidInput(format!("CompensableCarryError: {msg}")),
        WorkflowError::Deadlock(msg) => RegistryError::Database(format!("retryable deadlock: {msg}")),
        WorkflowError::Db(msg) => RegistryError::Database(msg),
    }
}

async fn apply_step_result_on_registry(
    registry: &Registry,
    config: &WorkflowEngineConfig,
    result: StepResult,
) -> Result<bool, WorkflowError> {
    let conn = registry
        .conn()
        .await
        .map_err(|e| WorkflowError::Db(e.to_string()))?;
    let tables = find_run_tables(&conn, &result.run_id)
        .await
        .map_err(|e| WorkflowError::Db(e.to_string()))?
        .ok_or_else(|| WorkflowError::Db(format!("workflow run {} not found", result.run_id)))?;
    let mut apply_config = config.clone();
    apply_config.journal_limits =
        workflow_limits::workflow_journal_limits_for_app(&conn, &tables.app_id)
            .await
            .map_err(|e| WorkflowError::Db(e.to_string()))?;
    let store = PgStore::new(registry.workflow_store_db_url().to_string(), tables.app_id);
    apply::apply_step_result_on_store(&store, &apply_config, result).await
}

/// Deterministic apply path that intentionally stops before scheduler sync.
///
/// This is used to exercise the crash window where the journal commit
/// succeeds but the scheduler ack is lost before the next timer is registered.
#[allow(clippy::future_not_send)]
pub async fn apply_step_result_without_scheduler_sync(
    state: &AppState,
    owner_id: &str,
    result: StepResult,
) -> Result<bool, RegistryError> {
    let mut config = WorkflowEngineConfig::default();
    config.owner_id = owner_id.to_string();
    apply_step_result_without_scheduler_sync_with_config(state, config, result).await
}

/// Deterministic apply path with config overrides that intentionally stops
/// before scheduler sync.
#[allow(clippy::future_not_send)]
pub async fn apply_step_result_without_scheduler_sync_with_config(
    state: &AppState,
    config: WorkflowEngineConfig,
    result: StepResult,
) -> Result<bool, RegistryError> {
    apply_step_result_on_registry(&state.registry, &config, result)
        .await
        .map_err(workflow_error_to_registry)
}

/// Public deterministic apply path for tests and future control handlers.
#[allow(clippy::future_not_send)]
pub async fn apply_step_result(
    state: &AppState,
    owner_id: &str,
    result: StepResult,
) -> Result<bool, RegistryError> {
    let run_id = result.run_id.clone();
    let mut config = WorkflowEngineConfig::default();
    config.owner_id = owner_id.to_string();
    let applied = apply_step_result_on_registry(&state.registry, &config, result)
        .await
        .map_err(workflow_error_to_registry)?;
    if applied {
        let store = WorkflowSchedulerStore::new(state.registry.workflow_store_db_url().to_string());
        sync_scheduler_after_apply(&store, &state.registry, &run_id).await?;
    }
    Ok(applied)
}

/// Public deterministic apply path with scheduler config overrides for tests.
#[allow(clippy::future_not_send)]
pub async fn apply_step_result_with_config(
    state: &AppState,
    config: WorkflowEngineConfig,
    result: StepResult,
) -> Result<bool, RegistryError> {
    let run_id = result.run_id.clone();
    let applied = apply_step_result_on_registry(&state.registry, &config, result)
        .await
        .map_err(workflow_error_to_registry)?;
    if applied {
        let store = WorkflowSchedulerStore::new(state.registry.workflow_store_db_url().to_string());
        sync_scheduler_after_apply(&store, &state.registry, &run_id).await?;
    }
    Ok(applied)
}

/// Register the run's current durable wake with the scheduler store.
#[allow(clippy::future_not_send)]
pub async fn register_run_timer(state: &AppState, run_id: &str) -> Result<(), RegistryError> {
    let store = WorkflowSchedulerStore::new(state.registry.workflow_store_db_url().to_string());
    sync_scheduler_for_run(&store, &state.registry, run_id).await
}

fn scheduler_error_to_registry(error: workflow_scheduler::SchedulerError) -> RegistryError {
    RegistryError::Database(format!("workflow scheduler: {error}"))
}

fn scheduler_store_error_to_registry(error: WorkflowSchedulerStoreError) -> RegistryError {
    RegistryError::Database(format!("workflow scheduler store: {error:?}"))
}

#[allow(clippy::future_not_send)]
async fn sync_scheduler_after_apply(
    scheduler_store: &WorkflowSchedulerStore,
    registry: &Registry,
    run_id: &str,
) -> Result<(), RegistryError> {
    let conn = registry.conn().await?;
    let Some(tables) = find_run_tables(&conn, run_id).await? else {
        scheduler_store
            .ack_terminal(run_id)
            .await
            .map_err(scheduler_store_error_to_registry)?;
        return Ok(());
    };
    let sql = journal_sql(
        &tables,
        "WITH RECURSIVE ancestors AS ( \
                 SELECT id, parent_run_id \
                   FROM zeroship.workflow_runs \
                  WHERE id = $1 \
                 UNION ALL \
                 SELECT p.id, p.parent_run_id \
                   FROM zeroship.workflow_runs p \
                   JOIN ancestors a ON a.parent_run_id = p.id \
             ), root AS ( \
                 SELECT id \
                   FROM ancestors \
                  WHERE parent_run_id IS NULL \
                  LIMIT 1 \
             ), family AS ( \
                 SELECT id, app_id, state, wake_at, cancel_requested, claimed_by, dispatch_nonce, waiting_step_key, tree_depth \
                   FROM zeroship.workflow_runs \
                  WHERE id = (SELECT id FROM root) \
                 UNION ALL \
                 SELECT c.id, c.app_id, c.state, c.wake_at, c.cancel_requested, c.claimed_by, c.dispatch_nonce, c.waiting_step_key, c.tree_depth \
                   FROM zeroship.workflow_runs c \
                   JOIN family f ON c.parent_run_id = f.id \
             ) \
             SELECT id, app_id, state, wake_at, cancel_requested, claimed_by, dispatch_nonce, waiting_step_key \
               FROM family \
              ORDER BY tree_depth, id",
    );
    let rows = conn
        .query(&sql, &[&run_id])
        .await
        .map_err(RegistryError::from)?;
    if rows.is_empty() {
        scheduler_store
            .ack_terminal(run_id)
            .await
            .map_err(scheduler_store_error_to_registry)?;
        return Ok(());
    }
    for row in rows {
        sync_scheduler_row(scheduler_store, &conn, &tables, &row).await?;
    }
    sync_parent_after_child_apply(scheduler_store, &conn, &tables, run_id).await?;

    Ok(())
}

#[allow(clippy::future_not_send)]
async fn sync_parent_after_child_apply<C>(
    scheduler_store: &WorkflowSchedulerStore,
    conn: &C,
    tables: &WorkflowTables,
    child_run_id: &str,
) -> Result<(), RegistryError>
where
    C: GenericClient + Sync,
{
    let parent_sql = journal_sql(
        tables,
        "SELECT parent_run_id \
               FROM zeroship.workflow_runs \
              WHERE id = $1 \
                AND parent_run_id IS NOT NULL",
    );
    let rows = conn
        .query(&parent_sql, &[&child_run_id])
        .await
        .map_err(RegistryError::from)?;
    let Some(row) = rows.first() else {
        return Ok(());
    };
    let parent_run_id: String = row.get("parent_run_id");
    let parent_sql = journal_sql(
        tables,
        "SELECT id, app_id, state, wake_at, cancel_requested, claimed_by, dispatch_nonce, waiting_step_key \
               FROM zeroship.workflow_runs \
              WHERE id = $1",
    );
    let parent_rows = conn
        .query(&parent_sql, &[&parent_run_id])
        .await
        .map_err(RegistryError::from)?;
    if let Some(row) = parent_rows.first() {
        sync_scheduler_row(scheduler_store, conn, tables, row).await?;
    }
    Ok(())
}

#[allow(clippy::future_not_send)]
async fn sync_scheduler_for_run(
    scheduler_store: &WorkflowSchedulerStore,
    registry: &Registry,
    run_id: &str,
) -> Result<(), RegistryError> {
    let conn = registry.conn().await?;
    let Some(tables) = find_run_tables(&conn, run_id).await? else {
        scheduler_store
            .ack_terminal(run_id)
            .await
            .map_err(scheduler_store_error_to_registry)?;
        return Ok(());
    };
    let sql = journal_sql(
        &tables,
        "SELECT id, app_id, state, wake_at, cancel_requested, claimed_by, dispatch_nonce, waiting_step_key \
               FROM zeroship.workflow_runs \
              WHERE id = $1",
    );
    let rows = conn
        .query(&sql, &[&run_id])
        .await
        .map_err(RegistryError::from)?;
    let Some(row) = rows.first() else {
        scheduler_store
            .ack_terminal(run_id)
            .await
            .map_err(scheduler_store_error_to_registry)?;
        return Ok(());
    };
    sync_scheduler_row(scheduler_store, &conn, &tables, row).await
}

#[allow(clippy::future_not_send)]
async fn sync_scheduler_row<C>(
    scheduler_store: &WorkflowSchedulerStore,
    conn: &C,
    tables: &WorkflowTables,
    row: &compio_postgres::Row,
) -> Result<(), RegistryError>
where
    C: GenericClient + Sync,
{
    let run_id: String = row.get("id");
    let app_id: Uuid = row.get("app_id");
    let state: String = row.get("state");
    let mut wake_at: Option<DateTime<Utc>> = row.get("wake_at");
    let cancel_requested: bool = row.get("cancel_requested");
    let waiting_step_key: Option<String> = row.get("waiting_step_key");
    if is_schedulable_state(&state) {
        if cancel_requested {
            wake_at = Some(Utc::now());
        }
        if state == "waiting" && wake_at.is_none() {
            wake_at = rearm_waiting_run_if_pending_signal(conn, tables, &run_id, waiting_step_key.as_deref()).await?;
        }
        if let Some(wake_at) = wake_at {
            // The journal claim gates execution; the scheduler store still
            // needs a row for every durable wake so claimed parent joins cannot
            // disappear between child-terminal applies and the parent's park.
            scheduler_store
                .ack_register_next(&run_id, app_id, wake_at)
                .await
                .map_err(scheduler_store_error_to_registry)?;
        } else if state == "waiting"
            && waiting_run_has_live_resume_source(conn, tables, &run_id, waiting_step_key.as_deref()).await?
        {
            // Deliberate event-only park: no timer and no in-flight row. This
            // is safe only because the live resume source checked above
            // guarantees a signal/child completion will register a due wake.
            // `ack_park` clears only in-flight so a concurrent event wake
            // registration in the timer store is not clobbered.
            scheduler_store
                .ack_park(&run_id)
                .await
                .map_err(scheduler_store_error_to_registry)?;
        } else {
            scheduler_store
                .ack_terminal(&run_id)
                .await
                .map_err(scheduler_store_error_to_registry)?;
        }
    } else {
        scheduler_store
            .ack_terminal(&run_id)
            .await
            .map_err(scheduler_store_error_to_registry)?;
    }
    Ok(())
}

fn is_schedulable_state(state: &str) -> bool {
    matches!(
        state,
        "queued" | "running" | "sleeping" | "waiting" | "compensating"
    )
}

async fn waiting_run_has_live_resume_source<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
    waiting_step_key: Option<&str>,
) -> Result<bool, RegistryError>
where
    C: GenericClient + Sync,
{
    match waiting_step_key {
        Some(key) => match parse_waiting_step_key(key)? {
            WaitingStep::Sleep { .. } => Ok(false),
            WaitingStep::Child { .. } => {
                waiting_run_has_running_step(conn, tables, run_id, "child").await
            }
            WaitingStep::WaitSignal { ordinal, name, .. } => {
                let sql = journal_sql(
                    tables,
                    "SELECT 1 \
                       FROM zeroship.workflow_steps \
                      WHERE run_id = $1 \
                        AND ordinal = $2 \
                        AND name = $3 \
                        AND kind = 'wait_signal' \
                        AND state = 'running' \
                      LIMIT 1",
                );
                let rows = conn
                    .query(&sql, &[&run_id, &ordinal, &name])
                    .await
                    .map_err(RegistryError::from)?;
                if !rows.is_empty() {
                    return Ok(true);
                }
                waiting_run_has_subscription(conn, tables, run_id, Some(ordinal)).await
            }
        },
        None => waiting_run_has_running_resume_step_or_subscription(conn, tables, run_id).await,
    }
}

async fn waiting_run_has_running_resume_step_or_subscription<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
) -> Result<bool, RegistryError>
where
    C: GenericClient + Sync,
{
    let sql = journal_sql(
        tables,
        "SELECT 1 \
           FROM zeroship.workflow_steps \
          WHERE run_id = $1 \
            AND kind IN ('child', 'wait_signal') \
            AND state = 'running' \
          LIMIT 1",
    );
    let rows = conn
        .query(&sql, &[&run_id])
        .await
        .map_err(RegistryError::from)?;
    if !rows.is_empty() {
        return Ok(true);
    }
    waiting_run_has_subscription(conn, tables, run_id, None).await
}

async fn waiting_run_has_running_step<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
    kind: &str,
) -> Result<bool, RegistryError>
where
    C: GenericClient + Sync,
{
    let sql = journal_sql(
        tables,
        "SELECT 1 \
           FROM zeroship.workflow_steps \
          WHERE run_id = $1 \
            AND kind = $2 \
            AND state = 'running' \
          LIMIT 1",
    );
    let rows = conn
        .query(&sql, &[&run_id, &kind])
        .await
        .map_err(RegistryError::from)?;
    Ok(!rows.is_empty())
}

async fn waiting_run_has_subscription<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
    ordinal: Option<i32>,
) -> Result<bool, RegistryError>
where
    C: GenericClient + Sync,
{
    let sql = journal_sql(
        tables,
        "SELECT 1 \
           FROM zeroship.workflow_subscriptions \
          WHERE run_id = $1 \
            AND ($2::integer IS NULL OR ordinal = $2) \
          LIMIT 1",
    );
    let rows = conn
        .query(&sql, &[&run_id, &ordinal])
        .await
        .map_err(RegistryError::from)?;
    Ok(!rows.is_empty())
}

async fn rearm_waiting_run_if_pending_signal<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
    waiting_step_key: Option<&str>,
) -> Result<Option<DateTime<Utc>>, RegistryError>
where
    C: GenericClient + Sync,
{
    let Some(key) = waiting_step_key else {
        return Ok(None);
    };
    let pending = match parse_waiting_step_key(key)? {
        WaitingStep::Sleep { .. } => false,
        WaitingStep::WaitSignal { signal_type, .. } => {
            let sql = journal_sql(
                tables,
                "SELECT id \
                       FROM zeroship.workflow_signals \
                      WHERE run_id = $1 \
                        AND type = $2 \
                        AND consumed_by IS NULL \
                      LIMIT 1",
            );
            let rows = conn
                .query(&sql, &[&run_id, &signal_type])
                .await
                .map_err(RegistryError::from)?;
            !rows.is_empty()
        }
        WaitingStep::Child { .. } => {
            let sql = journal_sql(
                tables,
                "SELECT sig.id \
                       FROM zeroship.workflow_steps s \
                       JOIN zeroship.workflow_signals sig \
                         ON sig.run_id = s.run_id \
                        AND sig.type = s.signal_type \
                        AND sig.consumed_by IS NULL \
                      WHERE s.run_id = $1 \
                        AND s.kind = 'child' \
                        AND s.state = 'running' \
                      LIMIT 1",
            );
            let rows = conn
                .query(&sql, &[&run_id])
                .await
                .map_err(RegistryError::from)?;
            !rows.is_empty()
        }
    };
    if !pending {
        return Ok(None);
    }

    let wake_at = Utc::now();
    let sql = journal_sql(
        tables,
        "UPDATE zeroship.workflow_runs \
                SET wake_at = $2 \
              WHERE id = $1 \
                AND state = 'waiting' \
                AND wake_at IS NULL",
    );
    let changed = conn
        .execute(&sql, &[&run_id, &wake_at])
        .await
        .map_err(RegistryError::from)?;
    if changed > 0 {
        Ok(Some(wake_at))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntex::web::{self, test};
    use serde_json::Value;

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
    fn cascade_cancel_children_sql_orders_child_locks_before_update() {
        let tables = WorkflowTables::for_app_id(&Uuid::nil());
        let sql = cascade_cancel_children_sql(&tables);
        let normalized = sql.split_whitespace().collect::<Vec<_>>().join(" ");

        // Durable workflow scheduler design section 5.2 requires every run-family
        // co-lock to acquire row locks in globally ascending run_id order.
        let lock_pos = normalized
            .find("ORDER BY id FOR UPDATE")
            .expect("cascade SQL must lock children in run_id order");
        let update_pos = normalized
            .find("UPDATE")
            .expect("cascade SQL must update locked children");
        assert!(
            lock_pos < update_pos,
            "child rows must be locked in order before mutation: {normalized}"
        );
        assert!(
            normalized.contains("WITH locked_children AS MATERIALIZED"),
            "ordered lock pass must be materialized before update: {normalized}"
        );
        assert!(
            normalized.contains("parent_run_id = $1")
                && normalized.contains("state NOT IN ('completed','failed','cancelled','stalled')")
                && normalized.contains("SET cancel_requested = true"),
            "cascade predicate and mutation semantics changed unexpectedly: {normalized}"
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

    fn test_dispatch_request() -> WorkflowRunDispatchRequest {
        WorkflowRunDispatchRequest {
            run_id: "run_test".to_string(),
            app_id: Uuid::new_v4(),
        }
    }

    async fn capture_step_request(
        seen: web::types::State<Arc<std::sync::Mutex<Vec<Value>>>>,
        body: ntex::util::Bytes,
    ) -> web::HttpResponse {
        let request: Value =
            serde_json::from_slice(body.as_ref()).expect("workflow dispatch request json");
        seen.lock().expect("seen lock").push(request.clone());
        web::HttpResponse::Ok().json(&serde_json::json!({
            "ack": true,
            "runId": request["runId"],
            "registrations": [{
                "runId": request["runId"],
                "appId": request["appId"],
                "terminal": true
            }]
        }))
    }

    #[ntex::test]
    async fn gateway_step_dispatcher_posts_and_parses_advance_ack() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let server_seen = Arc::clone(&seen);
        let gateway = test::server(move || {
            let seen = Arc::clone(&server_seen);
            async move {
                web::App::new().state(seen).service(
                    web::resource(WORKFLOW_ADVANCE_PATH)
                        .route(web::post().to(capture_step_request)),
                )
            }
        })
        .await;

        let request = test_dispatch_request();
        let dispatcher = GatewayStepDispatcher::new(gateway.url(""));
        let outcome = dispatcher.dispatch(request.clone()).await;
        let DispatchOutcome::Completed(result) = outcome else {
            panic!("expected completed dispatch outcome");
        };
        assert!(result.is_ack());
        assert_eq!(result.run_id.as_deref(), Some(request.run_id.as_str()));
        assert_eq!(result.registrations.len(), 1);
        assert_eq!(result.registrations[0].run_id, request.run_id);
        assert!(result.registrations[0].terminal);

        let seen = seen.lock().expect("seen lock");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0]["runId"], request.run_id);
        assert_eq!(seen[0]["appId"], request.app_id.to_string());
        assert_eq!(seen[0].as_object().expect("dispatch body object").len(), 2);
    }

    #[ntex::test]
    async fn gateway_step_dispatcher_maps_402_to_backpressure() {
        let gateway = test::server(|| async {
            web::App::new().service(
                web::resource(WORKFLOW_ADVANCE_PATH)
                    .route(web::post().to(|| async {
                        web::HttpResponse::PaymentRequired()
                            .json(&serde_json::json!({"code": "SPEND_LIMIT"}))
                    })),
            )
        })
        .await;

        let request = test_dispatch_request();
        let dispatcher = GatewayStepDispatcher::new(gateway.url(""));
        let outcome = dispatcher.dispatch(request.clone()).await;
        let DispatchOutcome::Backpressure { run_id, reason } = outcome
        else {
            panic!("expected backpressure dispatch outcome");
        };
        assert_eq!(run_id, request.run_id);
        assert!(reason.contains("HTTP 402"));
    }
}
