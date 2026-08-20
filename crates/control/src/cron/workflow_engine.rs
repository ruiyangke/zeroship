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
use compio_postgres::error::SqlState;
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

/// What a fleet sweep learned about ONE app's workflow journal.
///
/// The three arms are NOT interchangeable and callers must not collapse them.
/// `NotProvisioned` is knowledge - the app has never started a run, so its
/// journal holds nothing. `Unreachable` is the ABSENCE of knowledge: the tables
/// are in the catalog but this connection could not read them. A sweep that
/// deletes on the strength of "holds nothing" (blob GC, deploy retention) must
/// treat `Unreachable` as "may hold anything" and keep its hands off.
#[derive(Debug, Clone)]
pub(crate) enum AppJournal {
    /// The journal exists and is readable on this connection.
    Ready(WorkflowTables),
    /// This app has no journal tables at all.
    NotProvisioned,
    /// The journal exists but this connection could not reach it - no USAGE on
    /// `app_<uuid>`, a half-provisioned schema, or an app dropped mid-sweep.
    /// Carries the database's own message so the skip log names a cause.
    Unreachable(String),
}

/// True when a database error is scoped to ONE app's journal rather than to the
/// sweep as a whole.
///
/// The three codes are the ways ONE tenant's journal can fail while the
/// connection is healthy: no USAGE on `app_<uuid>`, the schema gone, a table
/// gone. Every other failure propagates. Two exclusions are deliberate:
///
/// - A lost connection carries NO SQLSTATE at all, so it can never match. A
///   sweep that skipped past one would walk the remaining apps against a dead
///   connection and then report a clean tick.
/// - `undefined_column` is NOT here. Every journal has the same shape, so a
///   missing column is our own SQL or a schema drift affecting the whole fleet,
///   not a property of one tenant, and it must fail rather than degrade into a
///   per-app warning repeated for every app.
pub(crate) fn is_journal_scoped_error(err: &compio_postgres::Error) -> bool {
    matches!(
        err.code(),
        Some(code)
            if *code == SqlState::INSUFFICIENT_PRIVILEGE
                || *code == SqlState::INVALID_SCHEMA_NAME
                || *code == SqlState::UNDEFINED_TABLE
    )
}

/// Convert a per-app failure into a logged skip, or propagate it.
///
/// `Ok(None)` means "skip this app, loudly": the caller `continue`s and the WARN
/// below names the app, the operation and the database's own message. It is a
/// WARN and not a silent `continue` on purpose - the defect this replaced was a
/// fleet-wide outage, and swapping one for a sweep that quietly covers fewer
/// tenants every tick would be the same defect with the alarm removed.
pub(crate) fn skip_journal_scoped<T>(
    app_id: &Uuid,
    operation: &str,
    result: Result<T, compio_postgres::Error>,
) -> Result<Option<T>, RegistryError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(err) if is_journal_scoped_error(&err) => {
            tracing::warn!(
                app_id = %app_id,
                operation,
                error = %err,
                "skipping app in workflow sweep: its journal is unreadable"
            );
            Ok(None)
        }
        Err(err) => Err(RegistryError::from(err)),
    }
}

/// Probe one app's journal without deciding what a failure means.
///
/// `to_regclass` RAISES rather than returning NULL when the caller holds no
/// USAGE on the schema, so "does it exist" and "can I read it" are answered by
/// the same statement and have to be separated here.
///
/// The `Unreachable` arm returns `Ok`, but the statement that produced it has
/// already ABORTED any open transaction - Postgres does not un-fail a
/// transaction because the client caught the error. So an in-transaction caller
/// can report the condition and must then roll back; it cannot carry on. That
/// is why fleet sweeps take their app list from [`journalled_apps`], which
/// answers the same question out of the catalog without raising.
pub(crate) async fn probe_journal<C>(conn: &C, app_id: &Uuid) -> Result<AppJournal, RegistryError>
where
    C: GenericClient + Sync,
{
    let tables = WorkflowTables::for_app_id(app_id);
    let result = conn
        .query("SELECT to_regclass($1) IS NOT NULL AS exists", &[&tables.runs])
        .await;
    match result {
        Ok(rows) => {
            if rows.first().is_some_and(|row| row.get("exists")) {
                Ok(AppJournal::Ready(tables))
            } else {
                Ok(AppJournal::NotProvisioned)
            }
        }
        Err(err) if is_journal_scoped_error(&err) => Ok(AppJournal::Unreachable(err.to_string())),
        Err(err) => Err(RegistryError::from(err)),
    }
}

/// Resolve one app's journal for a caller that is working on THAT app alone.
///
/// An unreachable journal is an error here, because the caller has no other
/// tenant to get on with: reporting `None` would tell it the app holds no runs,
/// which is exactly the confusion [`AppJournal`] exists to prevent. Fleet-wide
/// sweeps take their app list from [`journalled_fleet`] instead.
pub(crate) async fn existing_tables<C>(
    conn: &C,
    app_id: &Uuid,
) -> Result<Option<WorkflowTables>, RegistryError>
where
    C: GenericClient + Sync,
{
    match probe_journal(conn, app_id).await? {
        AppJournal::Ready(tables) => Ok(Some(tables)),
        AppJournal::NotProvisioned => Ok(None),
        AppJournal::Unreachable(message) => Err(RegistryError::Database(message)),
    }
}

/// One app that has a workflow journal, and whether this connection can read it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct JournalledApp {
    pub app_id: Uuid,
    /// False when this connection holds no USAGE on `app_<uuid>` or no SELECT on
    /// the journal's runs table.
    ///
    /// Answered by `has_*_privilege`, which counts a privilege the role could
    /// reach through role MEMBERSHIP - control reaches the journals by being a
    /// member of `zeroship_workflow_owner`. That is the right answer only
    /// because the connecting role INHERITs; a NOINHERIT role would be reported
    /// readable here and then be denied by the statement, because it would have
    /// to `SET ROLE` first. `zeroship_control` is created without NOINHERIT
    /// (db/migrations-ts/20260702000100_schema_roles_extensions.ts), so this
    /// holds; if that ever changes, this column starts lying.
    pub readable: bool,
}

/// Every app that HAS a workflow journal, with its readability, in id order.
///
/// Two decisions live in this one query.
///
/// WHY NOT `workflows_enabled`. Selecting every app and probing each with
/// `to_regclass` was O(apps) round trips per tick, and the obvious fix -
/// filtering on the rollout flag - is wrong: an app that ran workflows and was
/// then disabled still has runs to reap, blobs to collect and parked cancels to
/// honour, and the flag would strand every one of them. Journal EXISTENCE is the
/// real precondition, it is what each caller was re-deriving per app, and the
/// catalog answers it for the whole fleet in one round trip.
///
/// WHY `has_*_privilege` AND NOT A PROBE. `to_regclass` raises `permission
/// denied for schema` on a journal this connection cannot read, and inside a
/// sweep that runs in ONE transaction (deploy retention, blob GC) that error
/// poisons every later statement - so "log and skip" would not survive the skip.
/// `has_schema_privilege`/`has_table_privilege` return false instead of raising,
/// so an unreadable tenant is DATA here rather than a failure, and callers
/// decide what it means: an independent sweep skips it loudly, while a sweep
/// that deletes on the strength of "nothing references this" must treat it as
/// unknown and keep its hands off.
///
/// Catalog VISIBILITY is not privilege - `pg_class` lists relations in schemas
/// the caller cannot enter - which is why the readability column is needed at
/// all.
pub(crate) async fn journalled_apps<C>(conn: &C) -> Result<Vec<JournalledApp>, RegistryError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            "SELECT a.id AS app_id, \
                    (has_schema_privilege(n.oid, 'USAGE') \
                     AND has_table_privilege(c.oid, 'SELECT')) AS readable \
               FROM zeroship.apps a \
               JOIN pg_catalog.pg_namespace n \
                 ON n.nspname = 'app_' || a.id::text \
               JOIN pg_catalog.pg_class c \
                 ON c.relnamespace = n.oid \
                AND c.relname = '__zeroship_workflow_runs' \
              ORDER BY a.id",
            &[],
        )
        .await
        .map_err(RegistryError::from)?;
    Ok(rows
        .into_iter()
        .map(|row| JournalledApp {
            app_id: row.get("app_id"),
            readable: row.get("readable"),
        })
        .collect())
}

/// How much of the fleet one sweep tick actually covered.
///
/// The three fields ACCOUNT for the fleet: `apps_swept + apps_skipped +
/// apps_unvisited` is the number of apps that had a journal when the tick
/// started. That is the whole point of the type. A sweep that returns only a
/// work count cannot distinguish "nothing to do" from "one tenant excluded
/// because we could not read it", and the only signal for the second is a WARN
/// string in a log nobody greps - a count that means two different things is
/// the same defect species as a grep that returns empty for two different
/// reasons.
///
/// A caller therefore decides on [`Self::is_complete`], not on a log line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SweepCoverage {
    /// Apps the tick actually ran its per-app statement against.
    pub apps_swept: usize,
    /// Apps EXCLUDED: unreadable in the catalog probe, or readable there and
    /// then denied by the per-app statement. Both are the availability trade
    /// [`skip_journal_scoped`] makes, and both leave this tenant uncovered.
    pub apps_skipped: usize,
    /// Apps never reached because the tick hit its batch limit first. NOT an
    /// error - the next tick picks them up - but the tick's work counts still
    /// describe fewer tenants than exist, so it is not folded into `swept`.
    pub apps_unvisited: usize,
}

impl SweepCoverage {
    /// Apps that had a journal when the tick started.
    #[must_use]
    pub fn apps_total(self) -> usize {
        self.apps_swept
            .saturating_add(self.apps_skipped)
            .saturating_add(self.apps_unvisited)
    }

    /// True when every journalled app was swept. False means this tick's other
    /// counts are a partial view of the fleet.
    #[must_use]
    pub fn is_complete(self) -> bool {
        self.apps_skipped == 0 && self.apps_unvisited == 0
    }
}

/// The apps a fleet sweep may walk, and the ones it must exclude.
///
/// Returns both halves rather than the readable ids alone so that a caller
/// cannot consume the list without the exclusions being in front of it. An app
/// with a journal it cannot read is EXCLUDED and WARNED about, once per tick,
/// naming the app - and now also COUNTED, so the tick's return value carries
/// the exclusion instead of leaving it to log archaeology.
///
/// Before this existed the sweeps selected every app and propagated the per-app
/// probe with `?`, so one app whose schema control could not read aborted the
/// tick for every other tenant.
pub(crate) struct JournalledFleet {
    /// In id order.
    pub(crate) readable: Vec<Uuid>,
    /// In id order. Already WARNed about by the time this returns.
    pub(crate) unreadable: Vec<Uuid>,
}

pub(crate) async fn journalled_fleet<C>(conn: &C) -> Result<JournalledFleet, RegistryError>
where
    C: GenericClient + Sync,
{
    let mut fleet = JournalledFleet {
        readable: Vec::new(),
        unreadable: Vec::new(),
    };
    for app in journalled_apps(conn).await? {
        if app.readable {
            fleet.readable.push(app.app_id);
        } else {
            tracing::warn!(
                app_id = %app.app_id,
                "skipping app in workflow sweep: its journal schema is unreadable on this connection"
            );
            fleet.unreadable.push(app.app_id);
        }
    }
    Ok(fleet)
}

/// Locate the journal holding `run_id` by searching every app's journal.
///
/// This is a LOOKUP, not a sweep, and `Ok(None)` here means "no such run".
/// Callers act on that by dropping the work: `sync_scheduler_for_run_on` acks
/// the scheduler timer as TERMINAL, retiring a live run's timer for good, and
/// `cascade_cancel_children` reports that there are no children to cancel.
/// Manufacturing `None` from an app that was merely unreadable would turn a
/// permission gap into silent data loss.
///
/// So the readable journals are searched FIRST and an unreadable app only
/// matters once the search comes up empty - at which point "no such run" is not
/// a safe answer and this errors instead, leaving the caller to retry. That
/// ordering is the design: a healthy tenant's run is still found while another
/// app's privilege gap is open, so this is NOT the fleet-wide coupling the
/// sweeps were fixed to remove, but no `None` is ever produced from one.
///
/// The paragraph above described an INTENT the code did not implement until
/// this was fixed: the app list came from a helper that dropped unreadable apps
/// with a WARN, so an unreadable journal really did produce `Ok(None)` and the
/// run's timer really was acked terminal.
async fn find_run_tables<C>(conn: &C, run_id: &str) -> Result<Option<WorkflowTables>, RegistryError>
where
    C: GenericClient + Sync,
{
    let fleet = journalled_fleet(conn).await?;
    for app_id in fleet.readable {
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
    if let Some(app_id) = fleet.unreadable.first() {
        return Err(RegistryError::Database(format!(
            "cannot decide whether workflow run {run_id} exists: \
             app {app_id}'s journal is unreadable on this connection"
        )));
    }
    Ok(None)
}

/// Integration-test window onto [`find_run_tables`], which is private because
/// nothing outside this module should be locating a run by fleet scan.
///
/// Exposed so a test can assert the DIFFERENCE between "no such run" and "could
/// not tell", which is the whole point of that function and is otherwise only
/// observable through several layers of scheduler ack.
#[cfg(feature = "live-db-tests")]
#[allow(clippy::future_not_send)]
pub async fn __find_run_app_for_test(
    registry: &Registry,
    run_id: &str,
) -> Result<Option<Uuid>, RegistryError> {
    let conn = registry.conn().await?;
    Ok(find_run_tables(&conn, run_id).await?.map(|t| t.app_id))
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
            // `fired_timers`, not `claimed`: the count is per fired timer and one run can
            // contribute more than one, so labelling it a run count overstated it. See
            // `fire_once`.
            Ok(n) if n > 0 => {
                tracing::info!(fired_timers = n, "workflow_engine tick fired due timers");
            }
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
    if let Err(e) = store.ensure_ready().await {
        tracing::error!(error = %e, "workflow inflight reaper store not provisioned");
        return;
    }
    match reconcile_scheduler_from_journal_with_store(&store, &state.registry, false).await {
        Ok(n) if n > 0 => tracing::info!(registered = n, "workflow scheduler DR reconcile seeded timers"),
        Ok(_) => {}
        Err(e) => tracing::error!(error = %e, "workflow scheduler DR reconcile failed"),
    }
    loop {
        match reap_parked_cancel_requested_batch(&store, &state.registry, 64).await {
            // `apps_skipped` is logged even when it is the ONLY thing that
            // happened. A tick that reaped nothing because the one tenant with
            // parked cancels was unreadable used to be indistinguishable from
            // an idle tick here.
            Ok(reap) if reap.registered > 0 || reap.coverage.apps_skipped > 0 => tracing::info!(
                registered = reap.registered,
                apps_swept = reap.coverage.apps_swept,
                apps_skipped = reap.coverage.apps_skipped,
                apps_unvisited = reap.coverage.apps_unvisited,
                "workflow parked-cancel reaper tick"
            ),
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
        .ensure_ready()
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
    for app_id in journalled_fleet(&conn).await?.readable {
        let tables = WorkflowTables::for_app_id(&app_id);
        let sql = format!(
            "SELECT id, app_id, wake_at, cancel_requested \
               FROM {} \
              WHERE state IN ('queued','running','sleeping','waiting','compensating') \
                AND wake_at IS NOT NULL \
                AND ($1::bool = false OR wake_at <= now() OR cancel_requested) \
              ORDER BY wake_at, id",
            tables.runs
        );
        let Some(rows) =
            skip_journal_scoped(&app_id, "scheduler reconcile scan", conn.query(&sql, &[&due_only]).await)?
        else {
            continue;
        };
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

/// What one [`reap_parked_cancel_requested_batch`] tick did, and over how much
/// of the fleet.
///
/// `registered` alone cannot tell "no parked cancels anywhere" from "the one
/// tenant that had them was excluded because we could not read its journal".
/// `coverage` is what separates them, from the return value alone.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ParkedCancelReap {
    /// Timers registered with the scheduler store.
    pub registered: usize,
    pub coverage: SweepCoverage,
}

/// Pull parked cancel requests into the scheduler store as due-now timers.
///
/// Cascade cancellation updates the per-app journal first. The scheduler store
/// remains the timer authority, so a sleeping/waiting child with an old future
/// store row must be repaired before the normal due-timer scan can dispatch it.
///
/// An app whose journal this connection cannot read is SKIPPED, not fatal - one
/// tenant must not stop the fleet - and the skip is reported in the returned
/// [`SweepCoverage`] as well as WARNed.
#[allow(clippy::future_not_send)]
pub async fn reap_parked_cancel_requested_batch(
    scheduler_store: &WorkflowSchedulerStore,
    registry: &Registry,
    limit: i64,
) -> Result<ParkedCancelReap, RegistryError> {
    if limit <= 0 {
        return Ok(ParkedCancelReap::default());
    }

    let conn = registry.conn().await?;
    let fleet = journalled_fleet(&conn).await?;
    let mut reap = ParkedCancelReap {
        registered: 0,
        coverage: SweepCoverage {
            apps_swept: 0,
            apps_skipped: fleet.unreadable.len(),
            apps_unvisited: 0,
        },
    };
    // `apps` is walked by `next()` rather than by `for`, so that a `break` on
    // the batch limit leaves the untouched apps IN the iterator and `len()`
    // below reports them. A `for` loop would have already moved the app that
    // triggered the break out of it and undercounted by one.
    let mut apps = fleet.readable.into_iter();
    loop {
        if reap.registered >= usize::try_from(limit).unwrap_or(usize::MAX) {
            break;
        }
        let remaining = limit.saturating_sub(i64::try_from(reap.registered).unwrap_or(i64::MAX));
        if remaining <= 0 {
            break;
        }
        let Some(app_id) = apps.next() else {
            break;
        };
        let tables = WorkflowTables::for_app_id(&app_id);
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
        let Some(rows) =
            skip_journal_scoped(&app_id, "parked-cancel scan", conn.query(&sql, &[&remaining]).await)?
        else {
            // Readable in the catalog, denied by the statement: a different
            // exclusion from the one counted above, but the same consequence
            // for this tenant, so it lands in the same field.
            reap.coverage.apps_skipped = reap.coverage.apps_skipped.saturating_add(1);
            continue;
        };
        reap.coverage.apps_swept = reap.coverage.apps_swept.saturating_add(1);
        for row in rows {
            let run_id: String = row.get("id");
            let app_id: Uuid = row.get("app_id");
            let wake_at: DateTime<Utc> = row.get("wake_at");
            scheduler_store
                .ack_register_next(&run_id, app_id, wake_at)
                .await
                .map_err(scheduler_store_error_to_registry)?;
            reap.registered = reap.registered.saturating_add(1);
        }
    }
    reap.coverage.apps_unvisited = apps.len();
    Ok(reap)
}

/// Control-hosted timer-authority tick.
///
/// Timer state lives in the scheduler store; control claims due rows and owns
/// dispatch/ack handling until that loop moves into the standalone scheduler.
#[allow(clippy::future_not_send)]
pub async fn tick(state: &AppState) -> Result<usize, RegistryError> {
    let store = WorkflowSchedulerStore::new(state.registry.workflow_store_db_url().to_string());
    store
        .ensure_ready()
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
///
/// Returns the number of TIMERS FIRED, which is NOT the number of distinct runs
/// claimed and NOT the number of dispatches. The loop below increments once per fired
/// timer and keeps pulling until the per-app fair limit, so a single run can contribute
/// more than one - measured at 4 of 14 runs of one test, each dispatching exactly twice
/// in total, identical to the passing runs. The extra increment carries no extra
/// dispatch; the engine is right and the number is coarse.
///
/// So do not treat this as a run count. It is a liveness signal: greater than zero
/// means the tick did work.
///
/// `uncaught_step_failure_fails_after_one_extra_replay` asserted it as an exact run
/// count and failed 43 percent of isolated runs. Dropping that assertion removed only
/// part of it - 43 percent to 15 percent - because a SECOND, independent race remained:
/// a single tick can beat the replay's deadline in the scheduler store's own timer
/// wheel and correctly claim nothing. That needed a retry loop, not an assertion
/// change. Two races, not one, which is why several single-cause fixes each looked
/// refuted.
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
    // Coverage is DROPPED here on purpose: `fire_once` returns a fired-timer
    // count, and threading a second sweep's coverage through it would say
    // nothing about the timer loop below, which is what this function is. The
    // reaper's own caller (`run_inflight_reaper`) is where the coverage of this
    // sweep is reported. That does mean a `zeroship-control` running only the
    // engine cron still learns about a skipped tenant from the WARN alone.
    let _reap =
        reap_parked_cancel_requested_batch(scheduler_store, &state.registry, per_app_fair_limit)
            .await?;
    let mut scheduler_config = SchedulerConfig {
        max_due_per_tick: per_app_fair_limit_usize,
        max_loaded_timers: fair_limit,
        ..Default::default()
    };

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
    if let Some(next_wake_at) = registration.next_wake_at {
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
    let config = WorkflowEngineConfig {
        owner_id: owner_id.to_string(),
        ..Default::default()
    };
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
    let config = WorkflowEngineConfig {
        owner_id: owner_id.to_string(),
        ..Default::default()
    };
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

/// [`register_run_timer`] executed on the caller's open transaction.
///
/// The timer store is a separate SCHEMA in the same database as the workflow
/// journal (`Registry::workflow_store_db_url`), so both the run row and its
/// timer row can be written in one transaction. Callers that mutate a run and
/// then need it scheduled MUST use this rather than registering after commit:
/// a post-commit registration failure leaves a durable, queued run behind while
/// the caller reports an error, and the inflight reaper then adopts and runs it
/// — so an unkeyed client retry executes the workflow twice.
///
/// Reads go through the same `tx`, so the run's uncommitted state is what gets
/// synced.
#[allow(clippy::future_not_send)]
pub(crate) async fn register_run_timer_in_tx<C>(
    state: &AppState,
    tx: &C,
    run_id: &str,
) -> Result<(), RegistryError>
where
    C: GenericClient + Sync,
{
    let store = WorkflowSchedulerStore::new(state.registry.workflow_store_db_url().to_string());
    sync_scheduler_for_run_on(&store, tx, run_id, AckExec::CallerConnection).await
}

/// Where a scheduler-store write is executed.
///
/// The store's own methods open a fresh connection (and, where two statements
/// must agree, their own transaction). That is right for the cron paths, which
/// have no transaction to join. It is wrong for an API handler that has just
/// written the run row: there the ack has to land in the caller's transaction
/// or the two facts can disagree.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AckExec {
    /// The store opens its own connection and transaction.
    OwnConnection,
    /// The store's statements run on the connection passed to
    /// [`sync_scheduler_row`] — the caller's transaction.
    CallerConnection,
}

#[allow(clippy::future_not_send)]
async fn ack_register_next<C>(
    scheduler_store: &WorkflowSchedulerStore,
    exec: AckExec,
    conn: &C,
    run_id: &str,
    app_id: Uuid,
    wake_at: DateTime<Utc>,
) -> Result<(), RegistryError>
where
    C: GenericClient + Sync,
{
    match exec {
        AckExec::OwnConnection => scheduler_store.ack_register_next(run_id, app_id, wake_at).await,
        AckExec::CallerConnection => {
            scheduler_store
                .ack_register_next_on(conn, run_id, app_id, wake_at)
                .await
        }
    }
    .map(|_| ())
    .map_err(scheduler_store_error_to_registry)
}

#[allow(clippy::future_not_send)]
async fn ack_park<C>(
    scheduler_store: &WorkflowSchedulerStore,
    exec: AckExec,
    conn: &C,
    run_id: &str,
) -> Result<(), RegistryError>
where
    C: GenericClient + Sync,
{
    match exec {
        AckExec::OwnConnection => scheduler_store.ack_park(run_id).await,
        AckExec::CallerConnection => scheduler_store.ack_park_on(conn, run_id).await,
    }
    .map_err(scheduler_store_error_to_registry)
}

#[allow(clippy::future_not_send)]
async fn ack_terminal<C>(
    scheduler_store: &WorkflowSchedulerStore,
    exec: AckExec,
    conn: &C,
    run_id: &str,
) -> Result<(), RegistryError>
where
    C: GenericClient + Sync,
{
    match exec {
        AckExec::OwnConnection => scheduler_store.ack_terminal(run_id).await,
        AckExec::CallerConnection => scheduler_store.ack_terminal_on(conn, run_id).await,
    }
    .map_err(scheduler_store_error_to_registry)
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
        sync_scheduler_row(scheduler_store, &conn, &tables, &row, AckExec::OwnConnection).await?;
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
        sync_scheduler_row(scheduler_store, conn, tables, row, AckExec::OwnConnection).await?;
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
    sync_scheduler_for_run_on(scheduler_store, &conn, run_id, AckExec::OwnConnection).await
}

#[allow(clippy::future_not_send)]
async fn sync_scheduler_for_run_on<C>(
    scheduler_store: &WorkflowSchedulerStore,
    conn: &C,
    run_id: &str,
    exec: AckExec,
) -> Result<(), RegistryError>
where
    C: GenericClient + Sync,
{
    let Some(tables) = find_run_tables(conn, run_id).await? else {
        return ack_terminal(scheduler_store, exec, conn, run_id).await;
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
        return ack_terminal(scheduler_store, exec, conn, run_id).await;
    };
    sync_scheduler_row(scheduler_store, conn, &tables, row, exec).await
}

#[allow(clippy::future_not_send)]
async fn sync_scheduler_row<C>(
    scheduler_store: &WorkflowSchedulerStore,
    conn: &C,
    tables: &WorkflowTables,
    row: &compio_postgres::Row,
    exec: AckExec,
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
            ack_register_next(scheduler_store, exec, conn, &run_id, app_id, wake_at).await?;
        } else if state == "waiting"
            && waiting_run_has_live_resume_source(conn, tables, &run_id, waiting_step_key.as_deref()).await?
        {
            // Deliberate event-only park: no timer and no in-flight row. This
            // is safe only because the live resume source checked above
            // guarantees a signal/child completion will register a due wake.
            // `ack_park` clears only in-flight so a concurrent event wake
            // registration in the timer store is not clobbered.
            ack_park(scheduler_store, exec, conn, &run_id).await?;
        } else {
            ack_terminal(scheduler_store, exec, conn, &run_id).await?;
        }
    } else {
        ack_terminal(scheduler_store, exec, conn, &run_id).await?;
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
