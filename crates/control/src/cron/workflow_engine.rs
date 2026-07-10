//! Durable-workflow engine scheduler.
//!
//! This cron owns only the control-plane scheduling core: due-run claiming,
//! lease heartbeats, the dispatch seam, and the idempotent outcome apply txn.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use compio_postgres::GenericClient;
use serde_json::Value;
use uuid::Uuid;
use zeroship_core::typed_id;
use zeroship_plugin_workflow::advance::{
    WorkflowAdvanceNackKind, WorkflowAdvanceRegistration, WorkflowAdvanceResponse,
};
use zeroship_plugin_workflow::apply;
use zeroship_plugin_workflow::engine;
use zeroship_plugin_workflow::errors::WorkflowError;
use zeroship_plugin_workflow::store::pg::{PgStore, WorkflowTables};
use zeroship_plugin_workflow::store::{CompensationProgress, StepWriteOutcome};
use zeroship_workflow_scheduler::{
    self as workflow_scheduler, FiredTimer, LapsedInflightTimer, SchedulerConfig, TimerWheel,
    WakeHandle,
    WorkflowSchedulerStore, WorkflowSchedulerStoreError, WORKFLOW_ADVANCE_PATH,
};

use crate::registry::RegistryError;
use crate::workflow_limits;
use crate::workflow_rollout;
use crate::{AppState, Registry};

pub use engine::{
    child_dedup_key, child_signal_type, JournalStep, RunUpdate, StepCheckpoint, StepOutcome,
    StepRequest, StepResult, WorkflowEngineConfig, WorkflowOutputRef, DEFAULT_MAX_CHILD_DEPTH,
    DEFAULT_MAX_LIVE_DESCENDANTS, DEFAULT_MAX_START_MANY_BATCH,
};

/// Default tick cadence. Workflow wake latency is intentionally a scheduler
/// knob, not a correctness bound; DW-23 will measure and tune it.
pub const DEFAULT_TICK_SECS: u64 = 1;
const GATEWAY_DISPATCH_TIMEOUT: Duration = Duration::from_secs(35);
const BACKPRESSURE_PARK_MS: i64 = 1_000;
const BLOB_REF_JOURNAL_BYTES: i64 = 160;
static INFLIGHT_DISPATCHES: AtomicUsize = AtomicUsize::new(0);
static PROVISIONED_WORKFLOW_JOURNALS: OnceLock<Mutex<HashSet<Uuid>>> = OnceLock::new();

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
    async fn dispatch(&self, request: StepRequest) -> DispatchOutcome;
}

#[derive(Debug, Clone)]
pub enum DispatchOutcome {
    Completed(WorkflowAdvanceResponse),
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
            "SELECT id, app_id, wake_at \
               FROM {} \
              WHERE state IN ('queued','running','sleeping','waiting','compensating') \
                AND wake_at IS NOT NULL \
                AND ($1::bool = false OR wake_at <= now()) \
                AND (claimed_by IS NULL OR lease_expires IS NULL OR lease_expires <= now()) \
              ORDER BY wake_at, id",
            tables.runs
        );
        let rows = conn.query(&sql, &[&due_only]).await.map_err(RegistryError::from)?;
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

/// Retired control-side scan tick. The scheduler store is the timer authority.
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

    let reaped = reap_parked_cancel_requested_batch(&state.registry, &config).await?;
    for run_id in reaped {
        scheduler_store
            .ack_terminal(&run_id)
            .await
            .map_err(scheduler_store_error_to_registry)?;
    }

    let fair_limit = config
        .batch_apps
        .saturating_mul(config.per_app_fair_limit)
        .max(1);
    let per_app_fair_limit = config.per_app_fair_limit.max(1);
    let per_app_fair_limit_usize = usize::try_from(per_app_fair_limit).unwrap_or(usize::MAX);
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

        let mut batch_claimed = 0usize;
        for timer in fired {
            match claim_fired_timer(&state.registry, &config, &timer, true).await? {
                Some(claim) => {
                    claimed += 1;
                    batch_claimed += 1;
                    spawn_dispatch(
                        scheduler_store.clone(),
                        state.registry.clone(),
                        dispatcher.clone(),
                        config.clone(),
                        claim,
                    );
                }
                None => {
                    sync_scheduler_for_run(scheduler_store, &state.registry, &timer.run_id).await?;
                }
            }
        }
        if batch_claimed == 0 {
            break;
        }
    }
    Ok(claimed)
}

/// Re-dispatch scheduler rows whose dispatch/apply/register ack was lost.
///
/// The recurring scan reads only `workflow_scheduler.inflight`; each lapsed row
/// then goes through the normal per-run claim/dispatch/apply path.
#[allow(clippy::future_not_send)]
pub async fn reap_lapsed_inflight_once<D>(
    scheduler_store: &WorkflowSchedulerStore,
    state: &AppState,
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
        match claim_lapsed_inflight_timer(&state.registry, &config, &timer).await? {
            Some(claim) => {
                redispatched = redispatched.saturating_add(1);
                spawn_dispatch(
                    scheduler_store.clone(),
                    state.registry.clone(),
                    dispatcher.clone(),
                    config.clone(),
                    claim,
                );
            }
            None => {
                sync_scheduler_for_run(scheduler_store, &state.registry, &timer.run_id).await?;
            }
        }
    }
    Ok(redispatched)
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
    state: String,
    input: Option<Value>,
    started_at: DateTime<Utc>,
    waiting_step_key: Option<String>,
    cancel_requested: bool,
    require_due: bool,
}

async fn claim_fired_timer(
    registry: &Registry,
    config: &WorkflowEngineConfig,
    timer: &FiredTimer,
    require_due: bool,
) -> Result<Option<ClaimedRun>, RegistryError> {
    let mut conn = registry.conn().await?;
    let tx = conn.transaction().await.map_err(RegistryError::from)?;
    let Some(tables) = existing_tables(&tx, &timer.app_id).await? else {
        tx.commit().await.map_err(RegistryError::from)?;
        return Ok(None);
    };
    let sql = journal_sql(
        &tables,
        "SELECT r.id, r.app_id, r.workflow_name, r.deploy_id, d.deploy_hash, \
                    r.state, r.input, r.started_at, r.waiting_step_key, r.cancel_requested \
               FROM zeroship.workflow_runs r \
               JOIN zeroship.apps app ON app.id = r.app_id \
               JOIN zeroship.plans plan ON plan.id = app.plan_id \
               JOIN zeroship.app_deploys d ON d.id = r.deploy_id \
              WHERE r.id = $1 \
                AND r.app_id = $2 \
                AND ($3::bool = false OR r.wake_at <= now()) \
                AND r.state IN ('queued','running','sleeping','waiting','compensating') \
                AND (r.claimed_by IS NULL OR r.lease_expires IS NULL OR r.lease_expires <= now()) \
                AND app.workflows_enabled \
                AND plan.workflows_allowed \
                AND NOT plan.archived \
              FOR UPDATE SKIP LOCKED",
    );
    let rows = tx
        .query(&sql, &[&timer.run_id, &timer.app_id, &require_due])
        .await
        .map_err(RegistryError::from)?;

    let Some(row) = rows.first() else {
        tx.commit().await.map_err(RegistryError::from)?;
        return Ok(None);
    };
    let candidate = CandidateRun {
        run_id: row.get("id"),
        app_id: row.get("app_id"),
        workflow_name: row.get("workflow_name"),
        deploy_id: row.get("deploy_id"),
        deploy_hash: row.get("deploy_hash"),
        state: row.get("state"),
        input: row.get("input"),
        started_at: row.get("started_at"),
        waiting_step_key: row.get("waiting_step_key"),
        cancel_requested: row.get("cancel_requested"),
        require_due,
    };

    tx.batch_execute("SAVEPOINT workflow_claim_row")
        .await
        .map_err(RegistryError::from)?;
    let mut run_config = config.clone();
    run_config.journal_limits =
        workflow_limits::workflow_journal_limits_for_app(&tx, &candidate.app_id).await?;
    let claimed = match claim_one_locked(&tx, &tables, &run_config, candidate).await {
        Ok(run) => {
            tx.batch_execute("RELEASE SAVEPOINT workflow_claim_row")
                .await
                .map_err(RegistryError::from)?;
            run
        }
        Err(e) => {
            tracing::warn!(error = %e, run_id = %timer.run_id, "workflow_engine: quarantining poison fired timer");
            tx.batch_execute("ROLLBACK TO SAVEPOINT workflow_claim_row")
                .await
                .map_err(RegistryError::from)?;
            tx.batch_execute("RELEASE SAVEPOINT workflow_claim_row")
                .await
                .map_err(RegistryError::from)?;
            None
        }
    };

    tx.commit().await.map_err(RegistryError::from)?;
    Ok(claimed)
}

async fn claim_lapsed_inflight_timer(
    registry: &Registry,
    config: &WorkflowEngineConfig,
    timer: &LapsedInflightTimer,
) -> Result<Option<ClaimedRun>, RegistryError> {
    let fired = FiredTimer {
        run_id: timer.run_id.clone(),
        app_id: timer.app_id,
        wake_at: Utc::now(),
        dispatch_generation: timer.dispatch_generation,
        deadline: timer.deadline,
    };
    claim_fired_timer(registry, config, &fired, false).await
}

async fn reap_parked_cancel_requested_batch(
    registry: &Registry,
    config: &WorkflowEngineConfig,
) -> Result<Vec<String>, RegistryError> {
    let limit = i64::try_from(config.batch_apps.saturating_mul(config.per_app_fair_limit))
        .unwrap_or(i64::MAX);
    if limit <= 0 {
        return Ok(Vec::new());
    }

    let mut conn = registry.conn().await?;
    let tx = conn.transaction().await.map_err(RegistryError::from)?;
    let reaped = reap_parked_cancel_requested_runs(&tx, limit).await?;
    tx.commit().await.map_err(RegistryError::from)?;
    Ok(reaped)
}

async fn reap_parked_cancel_requested_runs<C>(
    tx: &C,
    limit: i64,
) -> Result<Vec<String>, RegistryError>
where
    C: GenericClient + Sync,
{
    let mut reaped = Vec::new();
    for app_id in workflow_app_ids(tx).await? {
        if i64::try_from(reaped.len()).unwrap_or(i64::MAX) >= limit {
            break;
        }
        let Some(tables) = existing_tables(tx, &app_id).await? else {
            continue;
        };
        let remaining = limit - i64::try_from(reaped.len()).unwrap_or(i64::MAX);
        let sql = format!(
            "SELECT r.id \
               FROM {runs} r \
              WHERE r.cancel_requested \
                AND r.state IN ('queued','sleeping','waiting') \
              ORDER BY r.wake_at NULLS FIRST, r.id \
              LIMIT $1 \
              FOR UPDATE SKIP LOCKED",
            runs = tables.runs
        );
        let rows = tx
            .query(&sql, &[&remaining])
            .await
            .map_err(RegistryError::from)?;
        for row in rows {
            let run_id: String = row.get("id");
            if cancel_requested_run_for_app(tx, &tables, &run_id).await? {
                reaped.push(run_id);
            }
        }
    }
    Ok(reaped)
}

pub(crate) async fn cascade_cancel_children_for_app<C>(
    conn: &C,
    tables: &WorkflowTables,
    parent_run_id: &str,
) -> Result<u64, RegistryError>
where
    C: GenericClient + Sync,
{
    let sql = journal_sql(
        tables,
        "UPDATE zeroship.workflow_runs \
            SET cancel_requested = true, wake_at = now() \
          WHERE parent_run_id = $1 \
            AND parent_cascade \
            AND state NOT IN ('completed','failed','cancelled','stalled')",
    );
    conn.execute(&sql, &[&parent_run_id])
        .await
        .map_err(RegistryError::from)
}

pub(crate) async fn cascade_cancel_children<C>(
    conn: &C,
    parent_run_id: &str,
) -> Result<u64, RegistryError>
where
    C: GenericClient + Sync,
{
    let Some(tables) = find_run_tables(conn, parent_run_id).await? else {
        return Ok(0);
    };
    cascade_cancel_children_for_app(conn, &tables, parent_run_id).await
}

async fn claim_one_locked<C>(
    tx: &C,
    tables: &WorkflowTables,
    config: &WorkflowEngineConfig,
    candidate: CandidateRun,
) -> Result<Option<ClaimedRun>, RegistryError>
where
    C: GenericClient + Sync,
{
    let inflight_sql = journal_sql(
        tables,
        "SELECT COUNT(*)::bigint AS n \
               FROM zeroship.workflow_runs \
              WHERE app_id = $1 \
                AND id <> $2 \
                AND state IN ('running','compensating') \
                AND claimed_by IS NOT NULL \
                AND lease_expires IS NOT NULL \
                AND lease_expires > now()",
    );
    let inflight = tx
        .query(&inflight_sql, &[&candidate.app_id, &candidate.run_id])
        .await
        .map_err(RegistryError::from)?;
    let inflight: i64 = inflight[0].get("n");
    if inflight >= config.max_inflight_per_app {
        return Ok(None);
    }

    if candidate.cancel_requested && candidate.state != "compensating" {
        cancel_requested_run_for_app(tx, tables, &candidate.run_id).await?;
        return Ok(None);
    }

    if candidate.state == "compensating"
        && !has_due_compensation(tx, tables, &candidate.run_id).await?
    {
        finalize_compensation_if_drained(tx, tables, &candidate.run_id).await?;
        return Ok(None);
    }

    let dispatch_nonce = typed_id::new_workflow_dispatch_id();
    let lease_expires = Utc::now() + chrono::Duration::milliseconds(config.claim_ttl_ms);
    if let Some(key) = candidate.waiting_step_key.as_deref() {
        if !resolve_due_waiting_step(tx, tables, config, &candidate.run_id, key, &dispatch_nonce).await? {
            return Ok(None);
        }
    }

    let update_sql = journal_sql(
        tables,
        "UPDATE zeroship.workflow_runs \
                SET claimed_by = $1, \
                    lease_expires = $2, \
                    dispatch_nonce = $3, \
                    claim_epoch = claim_epoch + 1, \
                    state = CASE WHEN state = 'compensating' THEN 'compensating' ELSE 'running' END, \
                    terminal_at = NULL, \
                    last_dispatch_at = now() \
              WHERE id = $4 \
                AND ($5::bool = false OR wake_at <= now()) \
                AND state IN ('queued','running','sleeping','waiting','compensating') \
                AND (claimed_by IS NULL OR lease_expires IS NULL OR lease_expires <= now()) \
              RETURNING id",
    );
    let rows = tx
        .query(
            &update_sql,
            &[
                &config.owner_id,
                &lease_expires,
                &dispatch_nonce,
                &candidate.run_id,
                &candidate.require_due,
            ],
        )
        .await
        .map_err(RegistryError::from)?;
    if rows.is_empty() {
        return Ok(None);
    }

    let journal = load_journal(tx, tables, &candidate.run_id).await?;
    Ok(Some(ClaimedRun {
        request: StepRequest {
            run_id: candidate.run_id,
            app_id: candidate.app_id,
            workflow_name: candidate.workflow_name,
            deploy_id: candidate.deploy_id,
            deploy_hash: candidate.deploy_hash,
            dispatch_nonce,
            phase: if candidate.state == "compensating" {
                "compensating".to_string()
            } else {
                "running".to_string()
            },
            input: candidate.input,
            started_at: candidate.started_at,
            journal,
            owner_id: config.owner_id.clone(),
            stuck_strike_limit: config.stuck_strike_limit,
            max_child_depth: config.max_child_depth,
            max_live_descendants: config.max_live_descendants,
            max_start_many_batch: config.max_start_many_batch,
            journal_limits: config.journal_limits,
        },
    }))
}

async fn load_journal<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
) -> Result<Vec<JournalStep>, RegistryError>
where
    C: GenericClient + Sync,
{
    let sql = journal_sql(
        tables,
        "SELECT ordinal, name, name_occurrence, kind, state, output, error, child_run_id, \
                    output_kind, output_hash, output_size, output_content_type, \
                    compensation_state \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 \
              ORDER BY ordinal",
    );
    let rows = conn
        .query(&sql, &[&run_id])
        .await
        .map_err(RegistryError::from)?;
    Ok(rows
        .into_iter()
        .map(|row| JournalStep {
            output_ref: workflow_output_ref_from_row(
                row.get("output_kind"),
                row.get("output_hash"),
                row.get("output_size"),
                row.get("output_content_type"),
            ),
            ordinal: row.get("ordinal"),
            name: row.get("name"),
            name_occurrence: row.get("name_occurrence"),
            kind: row.get("kind"),
            state: row.get("state"),
            output: row.get("output"),
            error: row.get("error"),
            child_run_id: row.get("child_run_id"),
            compensation_state: row.get("compensation_state"),
        })
        .collect())
}

fn workflow_output_ref_from_row(
    output_kind: String,
    output_hash: Option<String>,
    output_size: Option<i64>,
    output_content_type: Option<String>,
) -> Option<WorkflowOutputRef> {
    if output_kind != "blob" {
        return None;
    }
    Some(WorkflowOutputRef {
        hash: output_hash?,
        size: output_size?,
        content_type: output_content_type,
    })
}

fn workflow_output_ref_from_value(value: &Value) -> Option<WorkflowOutputRef> {
    let hash = value.get("hash")?.as_str()?.to_string();
    let size = value.get("size")?.as_i64()?;
    let content_type = value
        .get("contentType")
        .or_else(|| value.get("content_type"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(WorkflowOutputRef {
        hash,
        size,
        content_type,
    })
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

async fn resolve_due_waiting_step<C>(
    tx: &C,
    tables: &WorkflowTables,
    config: &WorkflowEngineConfig,
    run_id: &str,
    key: &str,
    dispatch_nonce: &str,
) -> Result<bool, RegistryError>
where
    C: GenericClient + Sync,
{
    match parse_waiting_step_key(key)? {
        WaitingStep::Sleep { ordinal, name } => {
            if insert_resolved_step(
                tx,
                tables,
                config,
                &StepCheckpoint {
                    ordinal,
                    name,
                    name_occurrence: 0,
                    kind: "sleep".to_string(),
                    state: "completed".to_string(),
                    output: None,
                    output_ref: None,
                    error: None,
                    wake_at: None,
                    signal_type: None,
                    max_signal_age_ms: None,
                    consumed_signal_id: None,
                    topic: None,
                    child_run_id: None,
                    child_workflow_name: None,
                    child_input: None,
                    child_options: None,
                    compensation_state: None,
                    compensation_max_attempts: 1,
                },
                run_id,
                dispatch_nonce,
                1,
            )
            .await? == StepWriteOutcome::CapExceeded
            {
                return Ok(false);
            }
            tx.execute(
                &journal_sql(tables, "UPDATE zeroship.workflow_runs \
                    SET waiting_step_key = NULL, wake_at = now() \
                  WHERE id = $1"),
                &[&run_id],
            )
            .await
            .map_err(RegistryError::from)?;
            Ok(true)
        }
        WaitingStep::WaitSignal {
            ordinal,
            name,
            signal_type,
            max_signal_age_ms,
        } => {
            let step_rows = tx
                .query(
                    &journal_sql(tables, "SELECT wake_at \
                       FROM zeroship.workflow_steps \
                      WHERE run_id = $1 \
                        AND ordinal = $2 \
                        AND name = $3 \
                        AND kind = 'wait_signal' \
                        AND state = 'running' \
                      FOR UPDATE"),
                    &[&run_id, &ordinal, &name],
                )
                .await
                .map_err(RegistryError::from)?;
            let Some(step_row) = step_rows.first() else {
                return Err(RegistryError::InvalidInput(format!(
                    "waiting_step_key {key} has no running workflow_steps row"
                )));
            };
            let deadline: Option<DateTime<Utc>> = step_row.get("wake_at");
            let now = Utc::now();
            let min_created_at =
                max_signal_age_ms.map(|age| now - chrono::Duration::milliseconds(age));
            if let Some(stale_cutoff) = min_created_at.as_ref() {
                tx.execute(
                    &journal_sql(tables, "UPDATE zeroship.workflow_signals \
                        SET consumed_by = $1 \
                      WHERE run_id = $1 \
                        AND type = $2 \
                        AND consumed_by IS NULL \
                        AND created_at < $3"),
                    &[&run_id, &signal_type, stale_cutoff],
                )
                .await
                .map_err(RegistryError::from)?;
            }
            let signal = tx
                .query(
                    &journal_sql(tables, "SELECT id, payload, created_at, origin, delivery, topic \
                       FROM zeroship.workflow_signals \
                      WHERE run_id = $1 \
                        AND type = $2 \
                        AND consumed_by IS NULL \
                        AND ($3::timestamptz IS NULL OR created_at >= $3) \
                      ORDER BY created_at, id \
                      LIMIT 1 \
                      FOR UPDATE SKIP LOCKED"),
                    &[&run_id, &signal_type, &min_created_at],
                )
                .await
                .map_err(RegistryError::from)?;

            let Some(row) = signal.first() else {
                if deadline.is_some_and(|deadline| deadline <= now) {
                    if insert_resolved_step(
                        tx,
                        tables,
                        config,
                        &StepCheckpoint {
                            ordinal,
                            name,
                            name_occurrence: 0,
                            kind: "wait_signal".to_string(),
                            state: "failed".to_string(),
                            output: None,
                            output_ref: None,
                            error: Some(serde_json::json!({
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
                            child_workflow_name: None,
                            child_input: None,
                            child_options: None,
                            compensation_state: None,
                            compensation_max_attempts: 1,
                        },
                        run_id,
                        dispatch_nonce,
                        1,
                    )
                    .await? == StepWriteOutcome::CapExceeded
                    {
                        return Ok(false);
                    }
                    tx.execute(
                        &journal_sql(tables, "UPDATE zeroship.workflow_runs \
                            SET waiting_step_key = NULL, wake_at = now() \
                          WHERE id = $1"),
                        &[&run_id],
                    )
                    .await
                    .map_err(RegistryError::from)?;
                    delete_workflow_subscription(tx, tables, run_id, ordinal).await?;
                    return Ok(true);
                }

                tx.execute(
                    &journal_sql(tables, "UPDATE zeroship.workflow_runs \
                        SET state = 'waiting', wake_at = $2 \
                      WHERE id = $1"),
                    &[&run_id, &deadline],
                )
                .await
                .map_err(RegistryError::from)?;
                return Ok(false);
            };

            let signal_id: String = row.get("id");
            let payload: Option<Value> = row.get("payload");
            let created_at: DateTime<Utc> = row.get("created_at");
            let origin: String = row.get("origin");
            let delivery: String = row.get("delivery");
            let topic: Option<String> = row.get("topic");
            if insert_resolved_step(
                tx,
                tables,
                config,
                &StepCheckpoint {
                    ordinal,
                    name,
                    name_occurrence: 0,
                    kind: "wait_signal".to_string(),
                    state: "completed".to_string(),
                    output: Some(serde_json::json!({
                        "id": signal_id.clone(),
                        "type": signal_type.clone(),
                        "payload": payload,
                        "createdAt": created_at.to_rfc3339(),
                        "receivedAt": created_at.to_rfc3339(),
                        "origin": origin,
                        "delivery": delivery,
                        "topic": topic.clone(),
                    })),
                    output_ref: None,
                    error: None,
                    wake_at: None,
                    signal_type: Some(signal_type),
                    max_signal_age_ms,
                    consumed_signal_id: Some(signal_id.clone()),
                    topic,
                    child_run_id: None,
                    child_workflow_name: None,
                    child_input: None,
                    child_options: None,
                    compensation_state: None,
                    compensation_max_attempts: 1,
                },
                run_id,
                dispatch_nonce,
                1,
            )
            .await? == StepWriteOutcome::CapExceeded
            {
                return Ok(false);
            }
            tx.execute(
                &journal_sql(tables, "UPDATE zeroship.workflow_signals \
                    SET consumed_by = $1 \
                  WHERE id = $2 AND consumed_by IS NULL"),
                &[&run_id, &signal_id],
            )
            .await
            .map_err(RegistryError::from)?;
            tx.execute(
                &journal_sql(tables, "UPDATE zeroship.workflow_runs \
                    SET waiting_step_key = NULL, wake_at = now() \
                  WHERE id = $1"),
                &[&run_id],
            )
            .await
            .map_err(RegistryError::from)?;
            delete_workflow_subscription(tx, tables, run_id, ordinal).await?;
            Ok(true)
        }
        WaitingStep::Child { ordinal, name } => {
            let step_rows = tx
                .query(
                    &journal_sql(tables, "SELECT wake_at, child_run_id, name_occurrence \
                       FROM zeroship.workflow_steps \
                      WHERE run_id = $1 \
                        AND ordinal = $2 \
                        AND name = $3 \
                        AND kind = 'child' \
                        AND state = 'running' \
                      FOR UPDATE"),
                    &[&run_id, &ordinal, &name],
                )
                .await
                .map_err(RegistryError::from)?;
            let Some(step_row) = step_rows.first() else {
                return Err(RegistryError::InvalidInput(format!(
                    "child waiting_step_key child:{ordinal}:{name} has no running workflow_steps row"
                )));
            };
            let deadline: Option<DateTime<Utc>> = step_row.get("wake_at");
            let child_run_id: Option<String> = step_row.get("child_run_id");
            let name_occurrence: i32 = step_row.get("name_occurrence");
            let mut signal_type = child_signal_type(ordinal);
            let now = Utc::now();
            let signal = tx
                .query(
                    &journal_sql(tables, "SELECT id, payload \
                       FROM zeroship.workflow_signals \
                      WHERE run_id = $1 \
                        AND type = $2 \
                        AND consumed_by IS NULL \
                      ORDER BY created_at, id \
                      LIMIT 1 \
                      FOR UPDATE SKIP LOCKED"),
                    &[&run_id, &signal_type],
                )
                .await
                .map_err(RegistryError::from)?;

            let mut resolved_ordinal = ordinal;
            let mut resolved_name = name;
            let mut resolved_name_occurrence = name_occurrence;
            let mut resolved_child_run_id = child_run_id;
            let current_signal = signal
                .first()
                .map(|row| (row.get::<_, String>("id"), row.get::<_, Option<Value>>("payload")));
            let resolved_signal = if let Some(signal) = current_signal {
                Some(signal)
            } else {
                let any_signal = tx
                    .query(
                        &journal_sql(tables, "SELECT sig.id, sig.payload, s.ordinal, s.name, s.name_occurrence, s.child_run_id \
                           FROM zeroship.workflow_steps s \
                           JOIN zeroship.workflow_signals sig \
                             ON sig.run_id = s.run_id \
                            AND sig.type = s.signal_type \
                            AND sig.consumed_by IS NULL \
                          WHERE s.run_id = $1 \
                            AND s.kind = 'child' \
                            AND s.state = 'running' \
                          ORDER BY sig.created_at, sig.id \
                          LIMIT 1 \
                          FOR UPDATE OF sig SKIP LOCKED"),
                        &[&run_id],
                    )
                    .await
                    .map_err(RegistryError::from)?;
                any_signal.first().map(|row| {
                    resolved_ordinal = row.get("ordinal");
                    resolved_name = row.get("name");
                    resolved_name_occurrence = row.get("name_occurrence");
                    resolved_child_run_id = row.get("child_run_id");
                    signal_type = child_signal_type(resolved_ordinal);
                    (row.get::<_, String>("id"), row.get::<_, Option<Value>>("payload"))
                })
            };

            let Some((signal_id, payload)) = resolved_signal else {
                if deadline.is_some_and(|deadline| deadline <= now) {
                    if let Some(child_run_id) = resolved_child_run_id.as_ref() {
                        tx.execute(
                            &journal_sql(tables, "UPDATE zeroship.workflow_runs \
                                SET cancel_requested = true, wake_at = now() \
                              WHERE id = $1 \
                                AND parent_cascade \
                                AND state NOT IN ('completed','failed','cancelled','stalled')"),
                            &[child_run_id],
                        )
                        .await
                        .map_err(RegistryError::from)?;
                    }
                    if insert_resolved_step(
                        tx,
                        tables,
                        config,
                            &StepCheckpoint {
                                ordinal: resolved_ordinal,
                                name: resolved_name,
                                name_occurrence: resolved_name_occurrence,
                                kind: "child".to_string(),
                                state: "failed".to_string(),
                            output: None,
                            output_ref: None,
                            error: Some(serde_json::json!({
                                "type": "ChildTimeoutError",
                                "message": format!("child workflow timed out for {signal_type}"),
                                "retryable": false,
                            })),
                            wake_at: None,
                            signal_type: Some(signal_type),
                            max_signal_age_ms: None,
                            consumed_signal_id: None,
                            topic: None,
                            child_run_id: resolved_child_run_id,
                            child_workflow_name: None,
                            child_input: None,
                            child_options: None,
                            compensation_state: None,
                            compensation_max_attempts: 1,
                        },
                        run_id,
                        dispatch_nonce,
                        1,
                    )
                    .await? == StepWriteOutcome::CapExceeded
                    {
                        return Ok(false);
                    }
                    tx.execute(
                        &journal_sql(tables, "UPDATE zeroship.workflow_runs \
                            SET waiting_step_key = NULL, wake_at = now() \
                          WHERE id = $1"),
                        &[&run_id],
                    )
                    .await
                    .map_err(RegistryError::from)?;
                    return Ok(true);
                }

                tx.execute(
                    &journal_sql(tables, "UPDATE zeroship.workflow_runs \
                        SET state = 'waiting', wake_at = $2 \
                      WHERE id = $1"),
                    &[&run_id, &deadline],
                )
                .await
                .map_err(RegistryError::from)?;
                return Ok(false);
            };

            let payload = payload.unwrap_or(Value::Null);
            let ok = payload
                .get("ok")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let output_ref = payload
                .get("outputRef")
                .or_else(|| payload.get("output_ref"))
                .and_then(workflow_output_ref_from_value);
            let checkpoint = StepCheckpoint {
                ordinal: resolved_ordinal,
                name: resolved_name,
                name_occurrence: resolved_name_occurrence,
                kind: "child".to_string(),
                state: if ok { "completed" } else { "failed" }.to_string(),
                output: if ok && output_ref.is_none() {
                    payload.get("output").cloned()
                } else {
                    None
                },
                output_ref,
                error: if ok {
                    None
                } else {
                    Some(payload.get("error").cloned().unwrap_or_else(|| {
                        serde_json::json!({
                            "type": "PermanentError",
                            "message": "child workflow failed",
                            "retryable": false,
                        })
                    }))
                },
                wake_at: None,
                signal_type: Some(signal_type),
                max_signal_age_ms: None,
                consumed_signal_id: Some(signal_id.clone()),
                topic: None,
                child_run_id: resolved_child_run_id,
                child_workflow_name: None,
                child_input: None,
                child_options: None,
                compensation_state: None,
                compensation_max_attempts: 1,
            };
            if insert_resolved_step(tx, tables, config, &checkpoint, run_id, dispatch_nonce, 1).await?
                == StepWriteOutcome::CapExceeded
            {
                return Ok(false);
            }
            tx.execute(
                &journal_sql(tables, "UPDATE zeroship.workflow_signals \
                    SET consumed_by = $1 \
                  WHERE id = $2 AND consumed_by IS NULL"),
                &[&run_id, &signal_id],
            )
            .await
            .map_err(RegistryError::from)?;
            tx.execute(
                &journal_sql(tables, "UPDATE zeroship.workflow_runs \
                    SET waiting_step_key = NULL, wake_at = now() \
                  WHERE id = $1"),
                &[&run_id],
            )
            .await
            .map_err(RegistryError::from)?;
            Ok(true)
        }
    }
}

fn spawn_dispatch<D>(
    scheduler_store: WorkflowSchedulerStore,
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
        claim.request.app_id,
        claim.request.run_id.clone(),
        claim.request.dispatch_nonce.clone(),
        config.claim_ttl_ms,
        Duration::from_millis(config.heartbeat_ms),
    );
    compio::runtime::spawn(async move {
        let mut inflight_guard = InflightDispatchGuard::new();
        let dispatch_run_id = claim.request.run_id.clone();
        let dispatch_nonce = claim.request.dispatch_nonce.clone();
        let outcome = dispatcher.dispatch(claim.request).await;
        heartbeat.store(false, Ordering::SeqCst);
        match outcome {
            DispatchOutcome::Completed(response) if response.is_ack() => {
                let run_id = response
                    .run_id
                    .clone()
                    .unwrap_or_else(|| dispatch_run_id.clone());
                inflight_guard.release();
                if let Err(e) = apply_workflow_advance_ack(&scheduler_store, &response).await {
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
                match response
                    .nack_kind
                    .unwrap_or(WorkflowAdvanceNackKind::ApplyFailed)
                {
                    WorkflowAdvanceNackKind::Deadlock => {
                        tracing::warn!(error = %reason, run_id = %run_id, "workflow_engine: worker apply deadlock, requeueing claim");
                        if let Err(e) =
                            requeue_claim(&registry, &config.owner_id, &run_id, &dispatch_nonce).await
                        {
                            tracing::error!(error = %e, run_id = %run_id, "workflow_engine: deadlock requeue failed");
                        } else {
                            inflight_guard.release();
                            if let Err(e) =
                                sync_scheduler_for_run(&scheduler_store, &registry, &run_id).await
                            {
                                tracing::error!(error = %e, run_id = %run_id, "workflow_engine: scheduler requeue ack failed");
                            }
                        }
                    }
                    WorkflowAdvanceNackKind::Backpressure => {
                        tracing::warn!(
                            run_id = %run_id,
                            reason = %reason,
                            "workflow_engine: worker backpressure, parking claim"
                        );
                        if let Err(e) =
                            park_backpressure_claim(&registry, &config.owner_id, &run_id, &dispatch_nonce).await
                        {
                            tracing::error!(error = %e, run_id = %run_id, "workflow_engine: backpressure park failed");
                        } else {
                            inflight_guard.release();
                            if let Err(e) = sync_scheduler_for_run(&scheduler_store, &registry, &run_id).await {
                                tracing::error!(error = %e, run_id = %run_id, "workflow_engine: scheduler backpressure ack failed");
                            }
                        }
                    }
                    WorkflowAdvanceNackKind::Invalid | WorkflowAdvanceNackKind::ApplyFailed => {
                        tracing::error!(reason = %reason, run_id = %run_id, "workflow_engine: worker workflow advance nack");
                        inflight_guard.release();
                        if let Err(e) =
                            sync_scheduler_for_run(&scheduler_store, &registry, &run_id).await
                        {
                            tracing::error!(error = %e, run_id = %run_id, "workflow_engine: scheduler worker nack sync failed");
                        }
                    }
                }
            }
            DispatchOutcome::Completed(response) => {
                tracing::error!(
                    ?response,
                    run_id = %dispatch_run_id,
                    "workflow_engine: invalid workflow advance response"
                );
                inflight_guard.release();
                if let Err(e) =
                    sync_scheduler_for_run(&scheduler_store, &registry, &dispatch_run_id).await
                {
                    tracing::error!(error = %e, run_id = %dispatch_run_id, "workflow_engine: scheduler invalid ack sync failed");
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
                } else {
                    inflight_guard.release();
                    if let Err(e) = sync_scheduler_for_run(&scheduler_store, &registry, &run_id).await {
                        tracing::error!(error = %e, run_id = %run_id, "workflow_engine: scheduler backpressure ack failed");
                    }
                }
            }
        }
    })
    .detach();
}

async fn apply_workflow_advance_ack(
    scheduler_store: &WorkflowSchedulerStore,
    response: &WorkflowAdvanceResponse,
) -> Result<(), RegistryError> {
    if response.registrations.is_empty() {
        return Err(RegistryError::InvalidInput(
            "workflow advance ack missing registrations".to_string(),
        ));
    }

    for registration in &response.registrations {
        apply_workflow_advance_registration(scheduler_store, registration).await?;
    }
    Ok(())
}

async fn apply_workflow_advance_registration(
    scheduler_store: &WorkflowSchedulerStore,
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
    Ok(())
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

fn spawn_heartbeat(
    registry: Registry,
    owner_id: String,
    app_id: Uuid,
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
                let tables = WorkflowTables::for_app_id(&app_id);
                let lease_expires = Utc::now() + chrono::Duration::milliseconds(claim_ttl_ms);
                let sql = journal_sql(
                    &tables,
                    "UPDATE zeroship.workflow_runs \
                        SET lease_expires = $1 \
                      WHERE id = $2 \
                        AND claimed_by = $3 \
                        AND dispatch_nonce = $4",
                );
                conn.execute(
                    &sql,
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

fn workflow_error_to_registry(error: WorkflowError) -> RegistryError {
    match error {
        WorkflowError::Invalid(msg) => RegistryError::InvalidInput(msg),
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
    RegistryError::Database(format!("workflow scheduler store: {error}"))
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
                 SELECT id, app_id, state, wake_at, claimed_by, dispatch_nonce, waiting_step_key, tree_depth \
                   FROM zeroship.workflow_runs \
                  WHERE id = (SELECT id FROM root) \
                 UNION ALL \
                 SELECT c.id, c.app_id, c.state, c.wake_at, c.claimed_by, c.dispatch_nonce, c.waiting_step_key, c.tree_depth \
                   FROM zeroship.workflow_runs c \
                   JOIN family f ON c.parent_run_id = f.id \
             ) \
             SELECT id, app_id, state, wake_at, claimed_by, dispatch_nonce, waiting_step_key \
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
        "SELECT id, app_id, state, wake_at, claimed_by, dispatch_nonce, waiting_step_key \
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
        "SELECT id, app_id, state, wake_at, claimed_by, dispatch_nonce, waiting_step_key \
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
    let waiting_step_key: Option<String> = row.get("waiting_step_key");
    if is_schedulable_state(&state) {
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
            // A no-wake parent join/subscription park can race a child/signal
            // wake that already registered the timer store. Preserve any row
            // that exists; the next terminal signal will register if none does.
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

async fn has_due_compensation<C>(
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
                AND ( \
                    compensation_state = 'pending' \
                    OR (compensation_state = 'running' \
                        AND (compensation_wake_at IS NULL OR compensation_wake_at <= now())) \
                ) \
              ORDER BY ordinal DESC \
              LIMIT 1",
    );
    let rows = conn
        .query(&sql, &[&run_id])
        .await
        .map_err(RegistryError::from)?;
    Ok(!rows.is_empty())
}


async fn finalize_compensation_if_drained<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
) -> Result<bool, RegistryError>
where
    C: GenericClient + Sync,
{
    let progress = compensation_progress(conn, tables, run_id).await?;
    if progress.remaining() > 0 {
        return Ok(false);
    }
    let select_sql = journal_sql(
        tables,
        "SELECT compensation_target, error \
               FROM zeroship.workflow_runs \
              WHERE id = $1 AND state = 'compensating' \
              FOR UPDATE",
    );
    let rows = conn
        .query(&select_sql, &[&run_id])
        .await
        .map_err(RegistryError::from)?;
    let Some(row) = rows.first() else {
        return Ok(false);
    };
    let target = row
        .get::<_, Option<String>>("compensation_target")
        .unwrap_or_else(|| "failed".to_string());
    let current_error: Option<Value> = row.get("error");
    let outcome = progress.terminal_outcome().to_string();
    let error = compensation_progress_error(current_error, progress, Some(outcome.as_str()));
    let update_sql = journal_sql(
        tables,
        "UPDATE zeroship.workflow_runs \
                SET state = $2, \
                    error = $3, \
                    wake_at = NULL, \
                    terminal_at = now(), \
                    waiting_step_key = NULL, \
                    compensation_outcome = $4, \
                    claimed_by = NULL, \
                    lease_expires = NULL, \
                    dispatch_nonce = NULL \
              WHERE id = $1 AND state = 'compensating'",
    );
    let changed = conn
        .execute(&update_sql, &[&run_id, &target, &error, &outcome])
        .await
        .map_err(RegistryError::from)?;
    if changed > 0 && matches!(target.as_str(), "failed" | "cancelled") {
        emit_child_terminal_hook_for_app(
            conn,
            tables,
            run_id,
            ChildTerminalPayload {
                state: &target,
                output: None,
                output_ref: None,
                error: Some(error.clone()),
            },
        )
        .await?;
        cascade_cancel_children_for_app(conn, tables, run_id).await?;
    }
    Ok(changed > 0)
}

async fn compensation_progress<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
) -> Result<CompensationProgress, RegistryError>
where
    C: GenericClient + Sync,
{
    let sql = journal_sql(
        tables,
        "SELECT \
                COUNT(*) FILTER (WHERE compensation_state IS NOT NULL)::bigint AS total, \
                COUNT(*) FILTER (WHERE compensation_state = 'completed')::bigint AS completed, \
                COUNT(*) FILTER (WHERE compensation_state = 'failed')::bigint AS failed, \
                COUNT(*) FILTER (WHERE compensation_state = 'pending')::bigint AS pending, \
                COUNT(*) FILTER (WHERE compensation_state = 'running')::bigint AS running \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1",
    );
    let row = conn
        .query_one(&sql, &[&run_id])
        .await
        .map_err(RegistryError::from)?;
    Ok(CompensationProgress {
        total: row.get("total"),
        completed: row.get("completed"),
        failed: row.get("failed"),
        pending: row.get("pending"),
        running: row.get("running"),
    })
}


fn compensation_progress_error(
    base: Option<Value>,
    progress: CompensationProgress,
    outcome: Option<&str>,
) -> Value {
    let mut error = match base {
        Some(Value::Object(map)) => Value::Object(map),
        Some(value) => serde_json::json!({
            "type": "Error",
            "message": "workflow failed during compensation",
            "cause": value,
        }),
        None => serde_json::json!({
            "type": "Error",
            "message": "workflow compensation is running",
        }),
    };
    let mut compensation = serde_json::json!({
        "total": progress.total,
        "completed": progress.completed,
        "failed": progress.failed,
    });
    if let Some(outcome) = outcome {
        compensation["outcome"] = Value::String(outcome.to_string());
    }
    if let Some(obj) = error.as_object_mut() {
        obj.insert("compensation".to_string(), compensation);
    }
    error
}


fn child_cancelled_error() -> Value {
    serde_json::json!({
        "type": "ChildCancelledError",
        "message": "child workflow was cancelled",
        "retryable": false,
    })
}

async fn cancel_requested_run_for_app<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
) -> Result<bool, RegistryError>
where
    C: GenericClient + Sync,
{
    let sql = journal_sql(
        tables,
        "UPDATE zeroship.workflow_runs \
                SET state = 'cancelled', \
                    cancel_requested = false, \
                    wake_at = NULL, \
                    terminal_at = now(), \
                    waiting_step_key = NULL, \
                    claimed_by = NULL, \
                    lease_expires = NULL, \
                    dispatch_nonce = NULL, \
                    claim_epoch = claim_epoch + 1 \
              WHERE id = $1 \
                AND cancel_requested \
                AND state NOT IN ('completed','failed','cancelled','stalled')",
    );
    let changed = conn
        .execute(&sql, &[&run_id])
        .await
        .map_err(RegistryError::from)?;
    if changed == 0 {
        return Ok(false);
    }

    emit_child_terminal_hook_for_app(
        conn,
        tables,
        run_id,
        ChildTerminalPayload {
            state: "cancelled",
            output: None,
            output_ref: None,
            error: Some(child_cancelled_error()),
        },
    )
    .await?;
    cascade_cancel_children_for_app(conn, tables, run_id).await?;
    Ok(true)
}


#[derive(Debug, Clone)]
struct ChildTerminalPayload<'a> {
    state: &'a str,
    output: Option<Value>,
    output_ref: Option<WorkflowOutputRef>,
    error: Option<Value>,
}

async fn emit_child_terminal_hook_for_app<C>(
    conn: &C,
    tables: &WorkflowTables,
    child_run_id: &str,
    terminal: ChildTerminalPayload<'_>,
) -> Result<(), RegistryError>
where
    C: GenericClient + Sync,
{
    let parent_sql = journal_sql(
        tables,
        "SELECT parent_run_id, parent_wait_step_key \
               FROM zeroship.workflow_runs \
              WHERE id = $1 \
                AND parent_run_id IS NOT NULL \
                AND parent_wait_step_key IS NOT NULL",
    );
    let rows = conn
        .query(&parent_sql, &[&child_run_id])
        .await
        .map_err(RegistryError::from)?;
    let Some(row) = rows.first() else {
        return Ok(());
    };
    let parent_run_id: String = row.get("parent_run_id");
    let parent_wait_step_key: String = row.get("parent_wait_step_key");
    let ok = terminal.state == "completed";
    let error = if ok {
        None
    } else if terminal.state == "cancelled" {
        Some(child_cancelled_error())
    } else {
        terminal.error
    };
    let output_ref = terminal.output_ref.map(|value| {
        serde_json::json!({
            "hash": value.hash,
            "size": value.size,
            "contentType": value.content_type,
        })
    });
    let payload = serde_json::json!({
        "ok": ok,
        "output": if ok { terminal.output } else { None },
        "outputRef": output_ref,
        "error": error,
        "state": terminal.state,
        "childRunId": child_run_id,
    });
    let signal_id = typed_id::new_workflow_signal_id();
    let insert_sql = journal_sql(
        tables,
        "INSERT INTO zeroship.workflow_signals \
            (id, run_id, type, payload, origin, delivery, idempotency_key, created_at) \
         VALUES ($1, $2, $3, $4, 'system', 'direct', $3, now()) \
         ON CONFLICT (run_id, type, idempotency_key) \
         WHERE idempotency_key IS NOT NULL AND delivery <> 'topic' \
         DO NOTHING",
    );
    conn.execute(
        &insert_sql,
        &[
            &signal_id,
            &parent_run_id,
            &parent_wait_step_key,
            &payload,
        ],
    )
    .await
    .map_err(RegistryError::from)?;
    let wake_sql = journal_sql(
        tables,
        "UPDATE zeroship.workflow_runs \
            SET wake_at = now() \
          WHERE id = $1 \
            AND state IN ('running','sleeping','waiting')",
    );
    conn.execute(
        &wake_sql,
        &[&parent_run_id],
    )
    .await
    .map_err(RegistryError::from)?;
    Ok(())
}

async fn delete_workflow_subscription<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
    ordinal: i32,
) -> Result<(), RegistryError>
where
    C: GenericClient + Sync,
{
    let sql = journal_sql(
        tables,
        "DELETE FROM zeroship.workflow_subscriptions \
          WHERE run_id = $1 AND ordinal = $2",
    );
    conn.execute(
        &sql,
        &[&run_id, &ordinal],
    )
    .await
    .map_err(RegistryError::from)?;
    Ok(())
}

async fn insert_resolved_step<C>(
    conn: &C,
    tables: &WorkflowTables,
    config: &WorkflowEngineConfig,
    checkpoint: &StepCheckpoint,
    run_id: &str,
    batch_id: &str,
    batch_width: i16,
) -> Result<StepWriteOutcome, RegistryError>
where
    C: GenericClient + Sync,
{
    let existing_sql = journal_sql(
        tables,
        "SELECT state, name, kind \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 AND ordinal = $2 \
              FOR UPDATE",
    );
    let existing = conn
        .query(&existing_sql, &[&run_id, &checkpoint.ordinal])
        .await
        .map_err(RegistryError::from)?;
    let resolves_running = if let Some(row) = existing.first() {
        let state: String = row.get("state");
        let name: String = row.get("name");
        let kind: String = row.get("kind");
        let resolves_running = state == "running"
            && matches!(checkpoint.state.as_str(), "completed" | "failed")
            && name == checkpoint.name
            && kind == checkpoint.kind;
        if !resolves_running {
            return Ok(StepWriteOutcome::Noop);
        }
        true
    } else {
        false
    };

    let output_ref = checkpoint.output_ref.as_ref();
    let output_kind = if output_ref.is_some() { "blob" } else { "inline" };
    let output_value = if output_ref.is_some() {
        None
    } else {
        checkpoint.output.clone()
    };
    let output_hash = output_ref.map(|value| value.hash.clone());
    let output_size = output_ref.map(|value| value.size);
    let output_content_type = output_ref
        .and_then(|value| value.content_type.clone())
        .or_else(|| output_ref.map(|_| "application/json".to_string()));
    let blob_bytes_delta = output_ref.map_or(0, |value| value.size.max(0));
    let compensation_state = if checkpoint.kind == "run" && checkpoint.state == "completed" {
        checkpoint.compensation_state.as_deref()
    } else {
        None
    };
    let compensation_max_attempts = checkpoint.compensation_max_attempts.max(1);

    let delta = checkpoint_journal_bytes(conn, checkpoint).await?;
    if delta > 0 {
        let accounting_sql = journal_sql(
            tables,
            "SELECT app_id, journal_bytes \
                   FROM zeroship.workflow_runs \
                  WHERE id = $1 \
                  FOR UPDATE",
        );
        let rows = conn
            .query(&accounting_sql, &[&run_id])
            .await
            .map_err(RegistryError::from)?;
        let Some(row) = rows.first() else {
            return Err(RegistryError::NotFound(format!(
                "workflow run {run_id} not found for journal accounting"
            )));
        };
        let current: i64 = row.get("journal_bytes");
        let limits = config.journal_limits;
        if engine::cap_exceeded(current, delta, limits.run_max_bytes) {
            mark_run_state_cap_exceeded(conn, tables, run_id, current, delta, limits.run_max_bytes)
                .await?;
            return Ok(StepWriteOutcome::CapExceeded);
        }
    }

    let changed = if resolves_running {
        let update_sql = journal_sql(
            tables,
            "UPDATE zeroship.workflow_steps \
                SET state = $4, \
                    output = $5, \
                    error = $6, \
                    wake_at = $7, \
                    signal_type = $8, \
                    max_signal_age_ms = $9, \
                    consumed_signal_id = $10, \
                    output_kind = $12, \
                    output_hash = $13, \
                    output_size = $14, \
                    output_content_type = $15, \
                    child_run_id = COALESCE(child_run_id, $16), \
                    compensation_state = $17, \
                    compensation_max_attempts = $18, \
                    finished_at = now() \
              WHERE run_id = $1 \
                AND ordinal = $2 \
                AND name = $3 \
                AND kind = $11 \
                AND state = 'running'",
        );
        conn.execute(
            &update_sql,
            &[
                &run_id,
                &checkpoint.ordinal,
                &checkpoint.name,
                &checkpoint.state,
                &output_value,
                &checkpoint.error,
                &checkpoint.wake_at,
                &checkpoint.signal_type,
                &checkpoint.max_signal_age_ms,
                &checkpoint.consumed_signal_id,
                &checkpoint.kind,
                &output_kind,
                &output_hash,
                &output_size,
                &output_content_type,
                &checkpoint.child_run_id,
                &compensation_state,
                &compensation_max_attempts,
            ],
        )
        .await
        .map_err(RegistryError::from)?
    } else {
        let insert_sql = journal_sql(
            tables,
            "INSERT INTO zeroship.workflow_steps \
            (run_id, ordinal, name, name_occurrence, kind, state, output, error, \
             output_kind, output_hash, output_size, output_content_type, \
             wake_at, signal_type, max_signal_age_ms, consumed_signal_id, \
             child_run_id, batch_id, batch_width, compensation_state, compensation_max_attempts, finished_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, \
                 $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, now()) \
         ON CONFLICT (run_id, ordinal) DO NOTHING",
        );
        conn.execute(
            &insert_sql,
            &[
                &run_id,
                &checkpoint.ordinal,
                &checkpoint.name,
                &checkpoint.name_occurrence,
                &checkpoint.kind,
                &checkpoint.state,
                &output_value,
                &checkpoint.error,
                &output_kind,
                &output_hash,
                &output_size,
                &output_content_type,
                &checkpoint.wake_at,
                &checkpoint.signal_type,
                &checkpoint.max_signal_age_ms,
                &checkpoint.consumed_signal_id,
                &checkpoint.child_run_id,
                &batch_id,
                &batch_width,
                &compensation_state,
                &compensation_max_attempts,
            ],
        )
        .await
        .map_err(RegistryError::from)?
    };
    if changed > 0 {
        if let Some(output_ref) = output_ref {
            upsert_workflow_blob_ref(conn, tables, output_ref).await?;
        }
    }
    if changed > 0 && (delta > 0 || blob_bytes_delta > 0) {
        let update_run_sql = journal_sql(
            tables,
            "UPDATE zeroship.workflow_runs \
                SET journal_bytes = journal_bytes + $2, \
                    blob_bytes = blob_bytes + $3 \
              WHERE id = $1",
        );
        conn.execute(
            &update_run_sql,
            &[&run_id, &delta, &blob_bytes_delta],
        )
        .await
        .map_err(RegistryError::from)?;
    }

    Ok(if changed > 0 {
        StepWriteOutcome::Wrote
    } else {
        StepWriteOutcome::Noop
    })
}

async fn checkpoint_journal_bytes<C>(
    conn: &C,
    checkpoint: &StepCheckpoint,
) -> Result<i64, RegistryError>
where
    C: GenericClient + Sync,
{
    if checkpoint.output_ref.is_some() {
        let rows = conn
            .query(
                "SELECT (COALESCE(pg_column_size($1::jsonb), 0))::bigint AS bytes",
                &[&checkpoint.error],
            )
            .await
            .map_err(RegistryError::from)?;
        let error_bytes: i64 = rows[0].get("bytes");
        return Ok(error_bytes + BLOB_REF_JOURNAL_BYTES);
    }
    let rows = conn
        .query(
            "SELECT (COALESCE(pg_column_size($1::jsonb), 0) \
                    + COALESCE(pg_column_size($2::jsonb), 0))::bigint AS bytes",
            &[&checkpoint.output, &checkpoint.error],
        )
        .await
        .map_err(RegistryError::from)?;
    Ok(rows[0].get("bytes"))
}


async fn upsert_workflow_blob_ref<C>(
    conn: &C,
    tables: &WorkflowTables,
    output_ref: &WorkflowOutputRef,
) -> Result<(), RegistryError>
where
    C: GenericClient + Sync,
{
    let content_type = output_ref
        .content_type
        .as_deref()
        .unwrap_or("application/json");
    let sql = journal_sql(
        tables,
        "INSERT INTO zeroship.workflow_blobs \
            (hash, size, content_type, refcount, last_referenced_at) \
         VALUES ($1, $2, $3, 1, now()) \
         ON CONFLICT (hash) DO UPDATE SET \
            size = EXCLUDED.size, \
            content_type = EXCLUDED.content_type, \
            refcount = zeroship.workflow_blobs.refcount + 1, \
            last_referenced_at = now()",
    );
    conn.execute(
        &sql,
        &[&output_ref.hash, &output_ref.size, &content_type],
    )
    .await
    .map_err(RegistryError::from)?;
    Ok(())
}

async fn mark_run_state_cap_exceeded<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
    current: i64,
    delta: i64,
    cap: i64,
) -> Result<(), RegistryError>
where
    C: GenericClient + Sync,
{
    let error = engine::state_cap_error(current, delta, cap);
    let sql = journal_sql(
        tables,
        "UPDATE zeroship.workflow_runs \
            SET state = 'failed', \
                output = NULL, \
                error = $2, \
                output_kind = 'inline', \
                output_hash = NULL, \
                output_size = NULL, \
                output_content_type = NULL, \
                wake_at = NULL, \
                terminal_at = now(), \
                waiting_step_key = NULL, \
                paused_from_status = NULL, \
                claimed_by = NULL, \
                lease_expires = NULL, \
                dispatch_nonce = NULL \
          WHERE id = $1",
    );
    conn.execute(
        &sql,
        &[&run_id, &error],
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
    let Some(tables) = find_run_tables(&conn, run_id).await? else {
        return Ok(());
    };
    let sql = journal_sql(
        &tables,
        "UPDATE zeroship.workflow_runs \
            SET state = CASE \
                    WHEN state = 'paused' THEN 'paused' \
                    WHEN state = 'compensating' THEN 'compensating' \
                    ELSE 'queued' \
                END, \
                wake_at = CASE WHEN state = 'paused' THEN wake_at ELSE now() END, \
                terminal_at = NULL, \
                claimed_by = NULL, \
                lease_expires = NULL, \
                dispatch_nonce = NULL \
          WHERE id = $1 \
            AND claimed_by = $2 \
            AND dispatch_nonce = $3 \
            AND state IN ('running','paused','compensating')",
    );
    conn.execute(
        &sql,
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
    let Some(tables) = find_run_tables(&conn, run_id).await? else {
        return Ok(());
    };
    let sql = journal_sql(
        &tables,
        "UPDATE zeroship.workflow_runs \
            SET state = CASE \
                    WHEN state = 'paused' THEN 'paused' \
                    WHEN state = 'compensating' THEN 'compensating' \
                    ELSE 'queued' \
                END, \
                wake_at = CASE WHEN state = 'paused' THEN wake_at ELSE $1 END, \
                terminal_at = NULL, \
                claimed_by = NULL, \
                lease_expires = NULL, \
                dispatch_nonce = NULL \
          WHERE id = $2 \
            AND claimed_by = $3 \
            AND dispatch_nonce = $4 \
            AND state IN ('running','paused','compensating')",
    );
    conn.execute(
        &sql,
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
            phase: "running".to_string(),
            input: Some(serde_json::json!({"orderId": "ord_1"})),
            started_at: Utc::now(),
            owner_id: "owner-test".to_string(),
            stuck_strike_limit: 3,
            max_child_depth: DEFAULT_MAX_CHILD_DEPTH,
            max_live_descendants: DEFAULT_MAX_LIVE_DESCENDANTS,
            max_start_many_batch: DEFAULT_MAX_START_MANY_BATCH,
            journal_limits: Default::default(),
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

        let request = test_step_request();
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
        assert_eq!(seen[0]["deployHash"], request.deploy_hash);
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
