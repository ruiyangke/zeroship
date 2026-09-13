//! Durable-workflow schedule reconciliation and sweep.
//!
//! App archive preserves schedule rows and their due frontier. Both the batch
//! claim and the claimed-row reload require an active app, so an archive that
//! lands between those two transactions still prevents a new run. Unarchive
//! resumes from the retained frontier under the schedule's catch-up policy.

use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration as ChronoDuration, TimeZone, Utc};
use compio_postgres::GenericClient;
use serde::Deserialize;
use serde_json::Value;
use zeroship_core::app_id::AppId;
use zeroship_core::typed_id;
use zeroship_workflow::{calendar::Calendar, store::pg::WorkflowTables};

use crate::registry::RegistryError;
use crate::workflow_instance_api;
use crate::{AppState, Registry};

pub const DEFAULT_TICK_SECS: u64 = 1;
pub const SCHEDULE_MIN_INTERVAL_MS: i64 = 1_000;
pub const SCHEDULE_MAX_PER_APP: usize = 64;
pub const SCHEDULE_BACKFILL_HARD_MAX: i32 = 32;

static OWNER_ID: OnceLock<String> = OnceLock::new();

fn default_owner_id() -> String {
    OWNER_ID
        .get_or_init(|| format!("control-wfs-{}", std::process::id()))
        .clone()
}

#[derive(Debug, Clone)]
pub struct ScheduleSweepConfig {
    pub batch_size: i64,
    pub claim_ttl_ms: i64,
    pub backfill_hard_max: i32,
    pub owner_id: String,
}

impl Default for ScheduleSweepConfig {
    fn default() -> Self {
        Self {
            batch_size: 32,
            claim_ttl_ms: 120_000,
            backfill_hard_max: SCHEDULE_BACKFILL_HARD_MAX,
            owner_id: default_owner_id(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ManifestSchedules {
    #[serde(default)]
    schedules: Vec<ScheduleRegistrationWire>,
}

#[derive(Debug, Deserialize)]
struct ScheduleRegistrationWire {
    name: String,
    #[serde(alias = "workflowName")]
    workflow_name: String,
    schedule: ScheduleDescriptorWire,
    #[serde(default)]
    input: Value,
    overlap: Option<String>,
    #[serde(alias = "catchUp", alias = "catch_up")]
    catch_up: Option<CatchUpWire>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind")]
enum ScheduleDescriptorWire {
    #[serde(rename = "cron")]
    Cron { cron_expr: String, tz: String },
    #[serde(rename = "interval")]
    Interval { interval_ms: i64, anchor: String },
}

#[derive(Debug, Deserialize)]
struct CatchUpWire {
    mode: String,
    max: Option<i32>,
}

#[derive(Debug, Clone)]
enum ScheduleDescriptor {
    Cron {
        cron_expr: String,
        tz_name: String,
        calendar: Calendar,
    },
    Interval {
        interval_ms: i64,
        anchor: IntervalAnchor,
    },
}

#[derive(Debug, Clone, Copy)]
enum IntervalAnchor {
    Epoch,
    Deploy,
}

#[derive(Debug, Clone)]
struct ValidatedSchedule {
    name: String,
    workflow_name: String,
    descriptor: ScheduleDescriptor,
    input: Value,
    overlap: String,
    catch_up: String,
    catch_up_max: i32,
    next_fire_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
struct ClaimedSchedule {
    id: String,
}

#[derive(Debug, Clone)]
struct ScheduleRow {
    id: String,
    app_id: AppId,
    deploy_id: String,
    workflow_name: String,
    descriptor: ScheduleDescriptor,
    input: Value,
    overlap: String,
    catch_up: String,
    catch_up_max: i32,
    next_fire_at: DateTime<Utc>,
    deploy_anchor: DateTime<Utc>,
}

pub async fn run(state: std::sync::Arc<AppState>, tick_secs: u64) {
    tracing::info!(tick_secs, "control workflow_schedules cron starting");
    loop {
        match tick(&state).await {
            Ok(n) if n > 0 => tracing::info!(fired = n, "workflow_schedules tick fired runs"),
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "workflow_schedules tick failed"),
        }
        compio::time::sleep(StdDuration::from_secs(tick_secs)).await;
    }
}

pub async fn tick(state: &AppState) -> Result<usize, RegistryError> {
    tick_with_config(state, ScheduleSweepConfig::default()).await
}

pub async fn tick_with_config(
    state: &AppState,
    config: ScheduleSweepConfig,
) -> Result<usize, RegistryError> {
    let claims = claim_due_schedules(&state.registry, &config).await?;
    let mut fired = 0usize;
    for claim in claims {
        match fire_claimed_schedule(state, &config, &claim).await {
            Ok(n) => fired = fired.saturating_add(n),
            Err(e) => {
                tracing::warn!(schedule_id = %claim.id, error = %e, "workflow_schedules fire failed")
            }
        }
    }
    Ok(fired)
}

pub async fn reconcile_deploy_schedules<C>(
    conn: &C,
    app_id: &AppId,
    deploy_id: &str,
    deploy_hash: &str,
    manifest_json: &str,
) -> Result<(), RegistryError>
where
    C: GenericClient + Sync,
{
    let manifest: ManifestSchedules = serde_json::from_str(manifest_json)
        .map_err(|e| RegistryError::InvalidInput(format!("manifest schedules are invalid: {e}")))?;
    if manifest.schedules.len() > SCHEDULE_MAX_PER_APP {
        return Err(RegistryError::InvalidInput(format!(
            "app declares {} workflow schedules; max is {SCHEDULE_MAX_PER_APP}",
            manifest.schedules.len()
        )));
    }

    let now = db_now(conn).await?;
    let deploy_anchor = deploy_activated_at(conn, deploy_id).await?.unwrap_or(now);
    let mut names = Vec::with_capacity(manifest.schedules.len());
    let mut seen = HashSet::with_capacity(manifest.schedules.len());
    let mut schedules = Vec::with_capacity(manifest.schedules.len());
    for wire in manifest.schedules {
        if !seen.insert(wire.name.clone()) {
            return Err(RegistryError::InvalidInput(format!(
                "duplicate workflow schedule name {:?}",
                wire.name
            )));
        }
        let schedule = validate_schedule(wire, now, deploy_anchor)?;
        names.push(schedule.name.clone());
        schedules.push(schedule);
    }

    for schedule in schedules {
        upsert_schedule(conn, app_id, deploy_id, deploy_hash, &schedule).await?;
    }

    if names.is_empty() {
        conn.execute(
            "DELETE FROM zeroship.workflow_schedules WHERE app_id = $1",
            &[&app_id.as_str()],
        )
        .await
        .map_err(RegistryError::from)?;
    } else {
        conn.execute(
            "DELETE FROM zeroship.workflow_schedules \
              WHERE app_id = $1 AND NOT (name = ANY($2))",
            &[&app_id.as_str(), &names],
        )
        .await
        .map_err(RegistryError::from)?;
    }
    Ok(())
}

async fn upsert_schedule<C>(
    conn: &C,
    app_id: &AppId,
    deploy_id: &str,
    deploy_hash: &str,
    schedule: &ValidatedSchedule,
) -> Result<(), RegistryError>
where
    C: GenericClient + Sync,
{
    let (kind, cron_expr, tz, interval_ms, anchor) = match &schedule.descriptor {
        ScheduleDescriptor::Cron {
            cron_expr, tz_name, ..
        } => (
            "cron",
            Some(cron_expr.clone()),
            Some(tz_name.clone()),
            None,
            None,
        ),
        ScheduleDescriptor::Interval {
            interval_ms,
            anchor,
        } => (
            "interval",
            None,
            None,
            Some(*interval_ms),
            Some(match anchor {
                IntervalAnchor::Epoch => "epoch".to_string(),
                IntervalAnchor::Deploy => "deploy".to_string(),
            }),
        ),
    };
    let id = typed_id::new_workflow_schedule_id();
    conn.execute(
        "INSERT INTO zeroship.workflow_schedules \
            (id, app_id, deploy_id, deploy_hash, name, workflow_name, kind, cron_expr, tz, \
             interval_ms, anchor, input_json, overlap, catch_up, catch_up_max, next_fire_at, \
             enabled, claimed_by, claimed_at, lease_expires, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, \
                 true, NULL, NULL, NULL, now()) \
         ON CONFLICT (app_id, name) DO UPDATE SET \
             deploy_id = EXCLUDED.deploy_id, \
             deploy_hash = EXCLUDED.deploy_hash, \
             workflow_name = EXCLUDED.workflow_name, \
             kind = EXCLUDED.kind, \
             cron_expr = EXCLUDED.cron_expr, \
             tz = EXCLUDED.tz, \
             interval_ms = EXCLUDED.interval_ms, \
             anchor = EXCLUDED.anchor, \
             input_json = EXCLUDED.input_json, \
             overlap = EXCLUDED.overlap, \
             catch_up = EXCLUDED.catch_up, \
             catch_up_max = EXCLUDED.catch_up_max, \
             next_fire_at = EXCLUDED.next_fire_at, \
             enabled = true, \
             claimed_by = NULL, \
             claimed_at = NULL, \
             lease_expires = NULL, \
             updated_at = now()",
        &[
            &id,
            &app_id.as_str(),
            &deploy_id,
            &deploy_hash,
            &schedule.name,
            &schedule.workflow_name,
            &kind,
            &cron_expr,
            &tz,
            &interval_ms,
            &anchor,
            &schedule.input,
            &schedule.overlap,
            &schedule.catch_up,
            &schedule.catch_up_max,
            &schedule.next_fire_at,
        ],
    )
    .await
    .map_err(RegistryError::from)?;
    Ok(())
}

async fn claim_due_schedules(
    registry: &Registry,
    config: &ScheduleSweepConfig,
) -> Result<Vec<ClaimedSchedule>, RegistryError> {
    if config.batch_size <= 0 {
        return Ok(Vec::new());
    }
    let mut conn = registry.conn().await?;
    let tx = conn.transaction().await.map_err(RegistryError::from)?;
    let now = db_now(&tx).await?;
    let lease_expires = now + ChronoDuration::milliseconds(config.claim_ttl_ms);
    let rows = tx
        .query(
            "WITH due AS ( \
               SELECT s.id \
                 FROM zeroship.workflow_schedules s \
                 JOIN zeroship.apps app ON app.id = s.app_id \
                 JOIN zeroship.plans plan ON plan.id = app.plan_id \
                WHERE s.enabled \
                  AND s.next_fire_at <= now() \
                  AND (s.claimed_by IS NULL OR s.lease_expires IS NULL OR s.lease_expires <= now()) \
                  AND app.archived_at IS NULL \
                  AND app.workflows_enabled \
                  AND plan.workflows_allowed \
                  AND NOT plan.archived \
                ORDER BY s.next_fire_at, s.id \
                LIMIT $1 \
                FOR UPDATE OF s SKIP LOCKED \
             ) \
             UPDATE zeroship.workflow_schedules s \
                SET claimed_by = $2, \
                    claimed_at = now(), \
                    lease_expires = $3, \
                    updated_at = now() \
               FROM due \
              WHERE s.id = due.id \
              RETURNING s.id",
            &[&config.batch_size, &config.owner_id, &lease_expires],
        )
        .await
        .map_err(RegistryError::from)?;
    tx.commit().await.map_err(RegistryError::from)?;
    Ok(rows
        .into_iter()
        .map(|row| ClaimedSchedule { id: row.get("id") })
        .collect())
}

async fn fire_claimed_schedule(
    state: &AppState,
    config: &ScheduleSweepConfig,
    claim: &ClaimedSchedule,
) -> Result<usize, RegistryError> {
    let registry = &state.registry;
    let mut conn = registry.conn().await?;
    let tx = conn.transaction().await.map_err(RegistryError::from)?;
    let locked = tx
        .query_one(
            "SELECT pg_try_advisory_xact_lock(hashtextextended($1::text, 0::bigint)) AS locked",
            &[&claim.id],
        )
        .await
        .map_err(RegistryError::from)?
        .get::<_, bool>("locked");
    if !locked {
        tx.commit().await.map_err(RegistryError::from)?;
        return Ok(0);
    }

    // Take lifecycle before any row lock. Deploy updates the app row before it
    // reconciles this schedule row, while archive takes lifecycle before the
    // app row. Reading app_id without a row lock, then taking lifecycle here,
    // prevents schedule -> lifecycle -> app -> schedule from becoming a
    // three-transaction deadlock cycle.
    let Some(app_id) = claimed_schedule_app_id(&tx, &claim.id, &config.owner_id).await? else {
        tx.commit().await.map_err(RegistryError::from)?;
        return Ok(0);
    };
    if !crate::workflow_rollout::workflows_enabled_for_app(&tx, &app_id).await? {
        tx.commit().await.map_err(RegistryError::from)?;
        return Ok(0);
    }

    let Some(row) = load_claimed_schedule(&tx, &claim.id, &config.owner_id).await? else {
        tx.commit().await.map_err(RegistryError::from)?;
        return Ok(0);
    };
    let tables = super::workflow_engine::provision_tables(&tx, &row.app_id).await?;
    let now = db_now(&tx).await?;
    let (instants, next_fire_at) = due_instants(
        &row.descriptor,
        row.next_fire_at,
        now,
        row.deploy_anchor,
        &row.catch_up,
        row.catch_up_max,
        config.backfill_hard_max,
    )?;
    let mut fired = 0usize;
    let mut fired_run_ids = Vec::new();
    let mut last_processed = row.next_fire_at;
    for planned in instants {
        last_processed = planned;
        if row.overlap == "skipIfRunning"
            && active_schedule_run_exists(&tx, &tables, &row.workflow_name, &row.id).await?
        {
            continue;
        }
        let key = schedule_dedup_key(&row.id, planned);
        let run_id = workflow_instance_api::start_scheduled_workflow_run(
            &tx,
            &row.app_id,
            &row.workflow_name,
            &row.deploy_id,
            &row.input,
            &key,
            planned,
        )
        .await?;
        fired_run_ids.push(run_id);
        fired = fired.saturating_add(1);
    }
    let last_epoch = last_processed.timestamp_millis();
    tx.execute(
        "UPDATE zeroship.workflow_schedules \
            SET last_fire_at = $2, \
                last_fired_epoch = $3, \
                next_fire_at = $4, \
                claimed_by = NULL, \
                claimed_at = NULL, \
                lease_expires = NULL, \
                updated_at = now() \
          WHERE id = $1",
        &[&row.id, &last_processed, &last_epoch, &next_fire_at],
    )
    .await
    .map_err(RegistryError::from)?;
    tx.commit().await.map_err(RegistryError::from)?;
    for run_id in fired_run_ids {
        super::workflow_engine::register_run_timer(state, &run_id).await?;
    }
    Ok(fired)
}

/// Read the lifecycle-lock key without taking a row lock. Ownership and app
/// state are rechecked by [`load_claimed_schedule`] after lifecycle is held.
async fn claimed_schedule_app_id<C>(
    conn: &C,
    schedule_id: &str,
    owner_id: &str,
) -> Result<Option<AppId>, RegistryError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            "SELECT app_id FROM zeroship.workflow_schedules \
              WHERE id = $1 AND claimed_by = $2",
            &[&schedule_id, &owner_id],
        )
        .await
        .map_err(RegistryError::from)?;
    rows.first()
        .map(|row| {
            let raw: String = row.get("app_id");
            AppId::parse(&raw).map_err(|e| {
                RegistryError::Database(format!(
                    "workflow_schedules.app_id {raw} is not a canonical app id: {e}"
                ))
            })
        })
        .transpose()
}

/// Re-read a schedule this sweeper claimed, gated on OWNERSHIP - not on lease
/// freshness.
///
/// `claim_due_schedules` stamps one `lease_expires` on the whole batch and
/// `tick_with_config` then fires the claims one at a time, so a freshness
/// predicate here would give every claim in the batch a shared wall-clock
/// budget that per-schedule work (a fresh connection, the app journal's first
/// `CREATE TABLE`, the run insert, the timer registration) spends. Exceeding it
/// made the sweep return fewer fires than it claimed - so `tick_with_config`'s
/// return value tracked how busy the machine was. Measured on this tree at load
/// ~45: `retention_and_schedule_sweeps_visit_multiple_app_journals` got 1 of 2
/// and `schedule_overlap_policy_skip_blocks_live_run_and_allow_fires_concurrent_run`
/// got 0 of 1, both green alone.
///
/// Ownership is the correct gate and is exact: any takeover goes through
/// `claim_due_schedules`, which rewrites `claimed_by`, and a completed fire
/// nulls it. Together with the `FOR UPDATE` below and the caller's
/// `pg_try_advisory_xact_lock`, a claim fires at most once however long the
/// sweeper took to get here. `lease_expires` keeps its real job in
/// `claim_due_schedules`: letting a LATER sweep re-claim a schedule whose
/// sweeper died mid-fire.
///
/// App activity is the other load-time gate. Archive may commit after the
/// batch claim but before this transaction begins, so checking only in
/// `claim_due_schedules` would still start a run after archive. The caller has
/// already taken the shared lifecycle lock from an unlocked app-id read; these
/// predicates recheck the joined state under the schedule row lock.
///
/// Lock only `s`. The caller already owns shared app-lifecycle before this
/// read, while archive owns exclusive lifecycle before updating
/// `zeroship.apps`. `start_scheduled_workflow_run` re-enters the same shared
/// transaction lock harmlessly.
async fn load_claimed_schedule<C>(
    conn: &C,
    schedule_id: &str,
    owner_id: &str,
) -> Result<Option<ScheduleRow>, RegistryError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            "SELECT s.id, s.app_id, s.deploy_id, s.workflow_name, s.kind, s.cron_expr, s.tz, \
                    s.interval_ms, s.anchor, s.input_json, s.overlap, s.catch_up, \
                    s.catch_up_max, s.next_fire_at, d.activated_at, d.created_at \
               FROM zeroship.workflow_schedules s \
               JOIN zeroship.app_deploys d ON d.id = s.deploy_id \
               JOIN zeroship.apps app ON app.id = s.app_id \
               JOIN zeroship.plans plan ON plan.id = app.plan_id \
              WHERE s.id = $1 \
                AND s.enabled \
                AND s.next_fire_at <= now() \
                AND s.claimed_by = $2 \
                AND app.archived_at IS NULL \
                AND app.workflows_enabled \
                AND plan.workflows_allowed \
                AND NOT plan.archived \
              FOR UPDATE OF s",
            &[&schedule_id, &owner_id],
        )
        .await
        .map_err(RegistryError::from)?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let descriptor = descriptor_from_columns(
        row.get("kind"),
        row.get("cron_expr"),
        row.get("tz"),
        row.get("interval_ms"),
        row.get("anchor"),
    )?;
    let activated_at: Option<DateTime<Utc>> = row.get("activated_at");
    let created_at: DateTime<Utc> = row.get("created_at");
    let app_id_raw: String = row.get("app_id");
    let app_id = AppId::parse(&app_id_raw).map_err(|e| {
        RegistryError::Database(format!(
            "workflow_schedules.app_id {app_id_raw} is not a canonical app id: {e}"
        ))
    })?;
    Ok(Some(ScheduleRow {
        id: row.get("id"),
        app_id,
        deploy_id: row.get("deploy_id"),
        workflow_name: row.get("workflow_name"),
        descriptor,
        input: row.get("input_json"),
        overlap: row.get("overlap"),
        catch_up: row.get("catch_up"),
        catch_up_max: row.get("catch_up_max"),
        next_fire_at: row.get("next_fire_at"),
        deploy_anchor: activated_at.unwrap_or(created_at),
    }))
}

async fn active_schedule_run_exists<C>(
    conn: &C,
    tables: &WorkflowTables,
    workflow_name: &str,
    schedule_id: &str,
) -> Result<bool, RegistryError>
where
    C: GenericClient + Sync,
{
    let prefix = format!("sched:{schedule_id}:%");
    let sql = format!(
        "SELECT 1 \
           FROM {} \
          WHERE workflow_name = $1 \
            AND dedup_key LIKE $2 \
            AND state NOT IN ('completed','failed','cancelled','stalled') \
          LIMIT 1",
        tables.runs
    );
    let rows = conn
        .query(&sql, &[&workflow_name, &prefix])
        .await
        .map_err(RegistryError::from)?;
    Ok(!rows.is_empty())
}

fn schedule_dedup_key(schedule_id: &str, planned: DateTime<Utc>) -> String {
    format!("sched:{schedule_id}:{}", planned.timestamp_millis())
}

fn validate_schedule(
    wire: ScheduleRegistrationWire,
    now: DateTime<Utc>,
    deploy_anchor: DateTime<Utc>,
) -> Result<ValidatedSchedule, RegistryError> {
    if wire.name.trim().is_empty() || wire.name.len() > 128 {
        return Err(RegistryError::InvalidInput(
            "workflow schedule name must be 1-128 bytes".to_string(),
        ));
    }
    if wire.workflow_name.trim().is_empty() || wire.workflow_name.len() > 128 {
        return Err(RegistryError::InvalidInput(
            "workflow schedule workflowName must be 1-128 bytes".to_string(),
        ));
    }
    let overlap = wire.overlap.unwrap_or_else(|| "allow".to_string());
    if overlap != "allow" && overlap != "skipIfRunning" {
        return Err(RegistryError::InvalidInput(format!(
            "workflow schedule {:?} has invalid overlap policy",
            wire.name
        )));
    }
    let (catch_up, catch_up_max) = normalize_catch_up(wire.catch_up.as_ref(), &wire.name)?;
    let descriptor = descriptor_from_wire(wire.schedule)?;
    let next_fire_at = first_fire_at_strictly_after(&descriptor, now, deploy_anchor)?;
    Ok(ValidatedSchedule {
        name: wire.name,
        workflow_name: wire.workflow_name,
        descriptor,
        input: wire.input,
        overlap,
        catch_up,
        catch_up_max,
        next_fire_at,
    })
}

fn normalize_catch_up(
    wire: Option<&CatchUpWire>,
    schedule_name: &str,
) -> Result<(String, i32), RegistryError> {
    let Some(wire) = wire else {
        return Ok(("skip".to_string(), 0));
    };
    match wire.mode.as_str() {
        "skip" => Ok(("skip".to_string(), 0)),
        "backfill" => {
            let max = wire.max.ok_or_else(|| {
                RegistryError::InvalidInput(format!(
                    "workflow schedule {schedule_name:?} backfill catchUp requires max"
                ))
            })?;
            if !(1..=SCHEDULE_BACKFILL_HARD_MAX).contains(&max) {
                return Err(RegistryError::InvalidInput(format!(
                    "workflow schedule {schedule_name:?} backfill max must be 1..={SCHEDULE_BACKFILL_HARD_MAX}"
                )));
            }
            Ok(("backfill".to_string(), max))
        }
        _ => Err(RegistryError::InvalidInput(format!(
            "workflow schedule {schedule_name:?} has invalid catchUp mode"
        ))),
    }
}

fn descriptor_from_wire(wire: ScheduleDescriptorWire) -> Result<ScheduleDescriptor, RegistryError> {
    match wire {
        ScheduleDescriptorWire::Cron { cron_expr, tz } => {
            let calendar = Calendar::parse(&cron_expr, &tz)
                .map_err(|error| RegistryError::InvalidInput(error.to_string()))?;
            Ok(ScheduleDescriptor::Cron {
                cron_expr: calendar.expression().to_owned(),
                tz_name: calendar.timezone().to_owned(),
                calendar,
            })
        }
        ScheduleDescriptorWire::Interval {
            interval_ms,
            anchor,
        } => {
            if interval_ms < SCHEDULE_MIN_INTERVAL_MS {
                return Err(RegistryError::InvalidInput(format!(
                    "workflow schedule interval_ms must be at least {SCHEDULE_MIN_INTERVAL_MS}"
                )));
            }
            let anchor = match anchor.as_str() {
                "epoch" => IntervalAnchor::Epoch,
                "deploy" => IntervalAnchor::Deploy,
                _ => {
                    return Err(RegistryError::InvalidInput(format!(
                        "unsupported workflow schedule interval anchor {anchor:?}"
                    )))
                }
            };
            Ok(ScheduleDescriptor::Interval {
                interval_ms,
                anchor,
            })
        }
    }
}

fn descriptor_from_columns(
    kind: String,
    cron_expr: Option<String>,
    tz: Option<String>,
    interval_ms: Option<i64>,
    anchor: Option<String>,
) -> Result<ScheduleDescriptor, RegistryError> {
    match kind.as_str() {
        "cron" => descriptor_from_wire(ScheduleDescriptorWire::Cron {
            cron_expr: cron_expr.ok_or_else(|| {
                RegistryError::InvalidInput("cron schedule row is missing cron_expr".to_string())
            })?,
            tz: tz.ok_or_else(|| {
                RegistryError::InvalidInput("cron schedule row is missing tz".to_string())
            })?,
        }),
        "interval" => descriptor_from_wire(ScheduleDescriptorWire::Interval {
            interval_ms: interval_ms.ok_or_else(|| {
                RegistryError::InvalidInput(
                    "interval schedule row is missing interval_ms".to_string(),
                )
            })?,
            anchor: anchor.ok_or_else(|| {
                RegistryError::InvalidInput("interval schedule row is missing anchor".to_string())
            })?,
        }),
        _ => Err(RegistryError::InvalidInput(format!(
            "unsupported workflow schedule kind {kind:?}"
        ))),
    }
}

fn due_instants(
    descriptor: &ScheduleDescriptor,
    planned: DateTime<Utc>,
    now: DateTime<Utc>,
    deploy_anchor: DateTime<Utc>,
    catch_up: &str,
    catch_up_max: i32,
    hard_max: i32,
) -> Result<(Vec<DateTime<Utc>>, DateTime<Utc>), RegistryError> {
    if catch_up != "backfill" {
        return Ok((
            vec![planned],
            first_fire_at_strictly_after(descriptor, now, deploy_anchor)?,
        ));
    }
    let cap = catch_up_max.min(hard_max).max(1) as usize;
    let mut instants = vec![planned];
    let mut last = planned;
    while instants.len() < cap {
        let next = first_fire_at_strictly_after(descriptor, last, deploy_anchor)?;
        if next > now {
            return Ok((instants, next));
        }
        instants.push(next);
        last = next;
    }
    let after_last = first_fire_at_strictly_after(descriptor, last, deploy_anchor)?;
    let next = if after_last <= now {
        first_fire_at_strictly_after(descriptor, now, deploy_anchor)?
    } else {
        after_last
    };
    Ok((instants, next))
}

fn first_fire_at_strictly_after(
    descriptor: &ScheduleDescriptor,
    after: DateTime<Utc>,
    deploy_anchor: DateTime<Utc>,
) -> Result<DateTime<Utc>, RegistryError> {
    match descriptor {
        ScheduleDescriptor::Interval {
            interval_ms,
            anchor,
        } => first_interval_fire_after(*interval_ms, *anchor, after, deploy_anchor),
        ScheduleDescriptor::Cron { calendar, .. } => calendar
            .next_after(after)
            .map_err(|error| RegistryError::InvalidInput(error.to_string())),
    }
}

fn first_interval_fire_after(
    interval_ms: i64,
    anchor: IntervalAnchor,
    after: DateTime<Utc>,
    deploy_anchor: DateTime<Utc>,
) -> Result<DateTime<Utc>, RegistryError> {
    if interval_ms < SCHEDULE_MIN_INTERVAL_MS {
        return Err(RegistryError::InvalidInput(
            "workflow schedule interval is below the server floor".to_string(),
        ));
    }
    let anchor_ms = match anchor {
        IntervalAnchor::Epoch => 0,
        IntervalAnchor::Deploy => deploy_anchor.timestamp_millis(),
    };
    let after_ms = after.timestamp_millis();
    if matches!(anchor, IntervalAnchor::Deploy) && after_ms < anchor_ms {
        return Ok(deploy_anchor);
    }
    let elapsed = after_ms.saturating_sub(anchor_ms);
    let periods = elapsed.div_euclid(interval_ms).saturating_add(1);
    let next_ms = anchor_ms
        .checked_add(periods.saturating_mul(interval_ms))
        .ok_or_else(|| {
            RegistryError::InvalidInput("workflow schedule interval overflow".to_string())
        })?;
    Utc.timestamp_millis_opt(next_ms).single().ok_or_else(|| {
        RegistryError::InvalidInput(
            "workflow schedule interval produced invalid timestamp".to_string(),
        )
    })
}

async fn db_now<C>(conn: &C) -> Result<DateTime<Utc>, RegistryError>
where
    C: GenericClient + Sync,
{
    Ok(conn
        .query_one("SELECT now() AS now", &[])
        .await
        .map_err(RegistryError::from)?
        .get("now"))
}

async fn deploy_activated_at<C>(
    conn: &C,
    deploy_id: &str,
) -> Result<Option<DateTime<Utc>>, RegistryError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            "SELECT COALESCE(activated_at, created_at) AS anchor \
               FROM zeroship.app_deploys \
              WHERE id = $1",
            &[&deploy_id],
        )
        .await
        .map_err(RegistryError::from)?;
    Ok(rows.first().map(|row| row.get("anchor")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0)
            .single()
            .expect("valid UTC instant")
    }

    fn cron(expr: &str, tz: &str) -> ScheduleDescriptor {
        descriptor_from_wire(ScheduleDescriptorWire::Cron {
            cron_expr: expr.to_string(),
            tz: tz.to_string(),
        })
        .expect("valid cron descriptor")
    }

    #[test]
    fn spring_forward_nonexistent_local_time_fires_at_first_valid_instant() {
        let schedule = cron("30 2 * * *", "America/New_York");
        let next =
            first_fire_at_strictly_after(&schedule, utc(2026, 3, 8, 6, 0), utc(2026, 3, 8, 6, 0))
                .expect("next fire");
        assert_eq!(next, utc(2026, 3, 8, 7, 0));
    }

    #[test]
    fn fall_back_ambiguous_local_time_fires_once_at_first_occurrence() {
        let schedule = cron("30 1 * * *", "America/New_York");
        let first =
            first_fire_at_strictly_after(&schedule, utc(2026, 11, 1, 4, 0), utc(2026, 11, 1, 4, 0))
                .expect("first fire");
        assert_eq!(first, utc(2026, 11, 1, 5, 30));

        let next = first_fire_at_strictly_after(&schedule, first, first).expect("next fire");
        assert_eq!(next, utc(2026, 11, 2, 6, 30));
    }

    #[test]
    fn interval_anchor_deploy_is_strictly_after_anchor() {
        let schedule = ScheduleDescriptor::Interval {
            interval_ms: 60_000,
            anchor: IntervalAnchor::Deploy,
        };
        let anchor = utc(2026, 7, 6, 12, 0);
        let next = first_fire_at_strictly_after(&schedule, anchor, anchor).expect("next fire");
        assert_eq!(next, utc(2026, 7, 6, 12, 1));
    }
}
