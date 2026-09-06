use chrono::{DateTime, Utc};
use compio_postgres::{GenericClient, NoTls};
use serde_json::Value;
use uuid::Uuid;
use zeroship_core::app_derivation;
use zeroship_core::app_id::AppId;
use zeroship_core::typed_id;

use crate::advance::{
    collect_post_apply_registrations_on_conn, WorkflowAdvanceRegistration,
    WorkflowRunDispatchRequest,
};
use crate::engine::{
    child_signal_type, workflow_engine_limits_from_plan, JournalStep, StepCheckpoint, StepRequest,
    WorkflowEngineConfig, WorkflowOutputRef,
};
use crate::errors::WorkflowError;
use crate::store::pg::{
    cascade_cancel_children_on_conn, compensation_progress_on_conn,
    collect_related_run_lock_ids_on_conn, emit_child_terminal_hook_on_conn,
    insert_resolved_step_on_conn, lock_run_set_for_apply_on_conn, PgStore, WorkflowTables,
};
use crate::store::{ChildTerminalPayload, CompensationProgress, StepWriteOutcome};

// Boxing `Claimed`'s `StepRequest` would shrink the enum, but
// `WorkflowClaimOutcome::Claimed` is matched by-value in
// crates/zeroship-control/tests/workflow_engine_test.rs, which is out of scope
// for this pass (crate boundary). The variant-size gap is a stack-copy
// cost, not a correctness issue.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum WorkflowClaimOutcome {
    Claimed(StepRequest),
    Terminal(Vec<WorkflowAdvanceRegistration>),
    ClaimLost,
    Backpressure(String),
}

#[derive(Debug, Clone)]
struct CandidateRun {
    run_id: String,
    app_id: Uuid,
    workflow_name: String,
    deploy_id: String,
    deploy_hash: String,
    state: String,
    wake_at: Option<DateTime<Utc>>,
    input: Option<Value>,
    started_at: DateTime<Utc>,
    waiting_step_key: Option<String>,
    cancel_requested: bool,
    claimed_by: Option<String>,
    lease_expires: Option<DateTime<Utc>>,
}

pub async fn claim_workflow_run(
    db_url: &str,
    request: &WorkflowRunDispatchRequest,
    config: &WorkflowEngineConfig,
) -> Result<WorkflowClaimOutcome, WorkflowError> {
    let (mut client, connection) = compio_postgres::connect(db_url, NoTls).await?;
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            tracing::error!(error = %e, "workflow claim pg connection error");
        }
    })
    .detach();

    PgStore::provision(&client, &request.app_id).await?;
    let tx = client.transaction().await?;
    let outcome = claim_workflow_run_on_conn(&tx, request, config).await?;
    tx.commit().await?;
    Ok(outcome)
}

pub async fn claim_workflow_run_on_conn<C>(
    tx: &C,
    request: &WorkflowRunDispatchRequest,
    config: &WorkflowEngineConfig,
) -> Result<WorkflowClaimOutcome, WorkflowError>
where
    C: GenericClient + Sync,
{
    // This shared transaction lock closes the scheduler-check-to-worker-claim
    // race. Archive takes the exclusive form before setting archived_at, so a
    // claim either commits before archive returns or observes the marker.
    tx.query_one(
        "SELECT pg_advisory_xact_lock_shared(hashtextextended($1, 0))",
        &[&app_derivation::lifecycle_lock_seed(&AppId::from_uuid(
            &request.app_id,
        ))],
    )
    .await?;
    let tables = WorkflowTables::for_app_id(&request.app_id);
    let select_sql = format!(
        "SELECT r.id, r.app_id, r.workflow_name, r.deploy_id, d.deploy_hash, \
                r.state, r.wake_at, r.input, r.started_at, r.waiting_step_key, r.cancel_requested, \
                r.claimed_by, r.lease_expires, \
                app.plan_id, plan.name AS plan_name, plan.runtime_limits_json \
           FROM {runs} r \
           JOIN zeroship.apps app ON app.id = r.app_id \
           JOIN zeroship.plans plan ON plan.id = app.plan_id \
           JOIN zeroship.app_deploys d ON d.id = r.deploy_id \
          WHERE r.id = $1 \
            AND r.app_id = $2 \
            AND app.archived_at IS NULL \
            AND app.workflows_enabled \
            AND plan.workflows_allowed \
            AND NOT plan.archived",
        runs = tables.runs
    );
    let rows = tx.query(&select_sql, &[&request.run_id, &request.app_id]).await?;
    let Some(row) = rows.first() else {
        return Ok(WorkflowClaimOutcome::ClaimLost);
    };

    let candidate = CandidateRun {
        run_id: row.get("id"),
        app_id: row.get("app_id"),
        workflow_name: row.get("workflow_name"),
        deploy_id: row.get("deploy_id"),
        deploy_hash: row.get("deploy_hash"),
        state: row.get("state"),
        wake_at: row.get("wake_at"),
        input: row.get("input"),
        started_at: row.get("started_at"),
        waiting_step_key: row.get("waiting_step_key"),
        cancel_requested: row.get("cancel_requested"),
        claimed_by: row.get("claimed_by"),
        lease_expires: row.get("lease_expires"),
    };
    let plan_id: String = row.get("plan_id");
    let plan_name: Option<String> = row.get("plan_name");
    let runtime_limits: Option<Value> = row.get("runtime_limits_json");
    let claim_config = workflow_engine_limits_from_plan(
        config,
        &plan_id,
        plan_name.as_deref(),
        runtime_limits.as_ref(),
    );

    if is_terminal_state(&candidate.state) {
        return Ok(WorkflowClaimOutcome::Terminal(
            collect_post_apply_registrations_on_conn(tx, request.app_id, &request.run_id, true)
                .await?,
        ));
    }

    if !is_claimable_state(&candidate.state) {
        return Ok(WorkflowClaimOutcome::ClaimLost);
    }

    if candidate
        .lease_expires
        .is_some_and(|lease_expires| lease_expires > Utc::now())
        && candidate.claimed_by.is_some()
    {
        return Ok(WorkflowClaimOutcome::ClaimLost);
    }

    let lock_ids = collect_related_run_lock_ids_on_conn(tx, &tables, &candidate.run_id).await?;
    let locked_rows = lock_run_set_for_apply_on_conn(tx, &tables, &lock_ids).await?;
    let Some(locked_candidate) = locked_rows.iter().find(|row| row.id == candidate.run_id) else {
        return Ok(WorkflowClaimOutcome::ClaimLost);
    };
    let mut candidate = candidate;
    candidate.app_id = locked_candidate.app_id;
    candidate.workflow_name = locked_candidate.workflow_name.clone();
    candidate.deploy_id = locked_candidate.deploy_id.clone();
    candidate.state = locked_candidate.state.clone();
    candidate.wake_at = locked_candidate.wake_at;
    candidate.waiting_step_key = locked_candidate.waiting_step_key.clone();
    candidate.cancel_requested = locked_candidate.cancel_requested;
    candidate.claimed_by = locked_candidate.claimed_by.clone();
    candidate.lease_expires = locked_candidate.lease_expires;

    if is_terminal_state(&candidate.state) {
        return Ok(WorkflowClaimOutcome::Terminal(
            collect_post_apply_registrations_on_conn(tx, request.app_id, &request.run_id, true)
                .await?,
        ));
    }

    if !is_claimable_state(&candidate.state) {
        return Ok(WorkflowClaimOutcome::ClaimLost);
    }

    if candidate
        .lease_expires
        .is_some_and(|lease_expires| lease_expires > Utc::now())
        && candidate.claimed_by.is_some()
    {
        return Ok(WorkflowClaimOutcome::ClaimLost);
    }

    claim_one_locked(tx, &tables, &claim_config, candidate).await
}

pub async fn renew_workflow_claim(
    db_url: &str,
    app_id: Uuid,
    run_id: &str,
    owner_id: &str,
    dispatch_nonce: &str,
    claim_ttl_ms: i64,
) -> Result<bool, WorkflowError> {
    let (client, connection) = compio_postgres::connect(db_url, NoTls).await?;
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            tracing::error!(error = %e, "workflow heartbeat pg connection error");
        }
    })
    .detach();
    renew_workflow_claim_on_conn(
        &client,
        app_id,
        run_id,
        owner_id,
        dispatch_nonce,
        claim_ttl_ms,
    )
    .await
}

pub async fn renew_workflow_claim_on_conn<C>(
    conn: &C,
    app_id: Uuid,
    run_id: &str,
    owner_id: &str,
    dispatch_nonce: &str,
    claim_ttl_ms: i64,
) -> Result<bool, WorkflowError>
where
    C: GenericClient + Sync,
{
    let tables = WorkflowTables::for_app_id(&app_id);
    let lease_expires = Utc::now() + chrono::Duration::milliseconds(claim_ttl_ms);
    let sql = format!(
        "UPDATE {runs} \
            SET lease_expires = $1 \
          WHERE id = $2 \
            AND claimed_by = $3 \
            AND dispatch_nonce = $4",
        runs = tables.runs
    );
    let changed = conn
        .execute(&sql, &[&lease_expires, &run_id, &owner_id, &dispatch_nonce])
        .await?;
    Ok(changed > 0)
}

async fn claim_one_locked<C>(
    tx: &C,
    tables: &WorkflowTables,
    config: &WorkflowEngineConfig,
    candidate: CandidateRun,
) -> Result<WorkflowClaimOutcome, WorkflowError>
where
    C: GenericClient + Sync,
{
    let inflight_sql = format!(
        "SELECT COUNT(*)::bigint AS n \
           FROM {runs} \
          WHERE app_id = $1 \
            AND id <> $2 \
            AND state IN ('running','compensating') \
            AND claimed_by IS NOT NULL \
            AND lease_expires IS NOT NULL \
            AND lease_expires > now()",
        runs = tables.runs
    );
    let inflight = tx
        .query(&inflight_sql, &[&candidate.app_id, &candidate.run_id])
        .await?;
    let inflight: i64 = inflight[0].get("n");
    if inflight >= config.max_inflight_per_app {
        return Ok(WorkflowClaimOutcome::Backpressure(format!(
            "workflow app {} has {inflight} in-flight claims",
            candidate.app_id
        )));
    }

    if !candidate.cancel_requested && candidate.wake_at.is_some_and(|wake_at| wake_at > Utc::now()) {
        return Ok(WorkflowClaimOutcome::Terminal(
            collect_post_apply_registrations_on_conn(tx, candidate.app_id, &candidate.run_id, true)
                .await?,
        ));
    }

    if candidate.cancel_requested && candidate.state != "compensating" {
        if cancel_requested_run_for_app(tx, tables, &candidate.run_id).await? {
            return Ok(WorkflowClaimOutcome::Terminal(
                collect_post_apply_registrations_on_conn(
                    tx,
                    candidate.app_id,
                    &candidate.run_id,
                    true,
                )
                .await?,
            ));
        }
        return Ok(WorkflowClaimOutcome::ClaimLost);
    }

    if candidate.state == "compensating"
        && !has_due_compensation(tx, tables, &candidate.run_id).await?
    {
        if finalize_compensation_if_drained(tx, tables, &candidate.run_id).await? {
            return Ok(WorkflowClaimOutcome::Terminal(
                collect_post_apply_registrations_on_conn(
                    tx,
                    candidate.app_id,
                    &candidate.run_id,
                    true,
                )
                .await?,
            ));
        }
        return Ok(WorkflowClaimOutcome::ClaimLost);
    }

    let dispatch_nonce = typed_id::new_workflow_dispatch_id();
    let lease_expires = Utc::now() + chrono::Duration::milliseconds(config.claim_ttl_ms);
    if let Some(key) = candidate.waiting_step_key.as_deref() {
        if !resolve_due_waiting_step(tx, tables, config, &candidate.run_id, key, &dispatch_nonce)
            .await?
        {
            return Ok(WorkflowClaimOutcome::ClaimLost);
        }
    }

    let update_sql = format!(
        "UPDATE {runs} \
            SET claimed_by = $1, \
                lease_expires = $2, \
                dispatch_nonce = $3, \
                state = CASE WHEN state = 'compensating' THEN 'compensating' ELSE 'running' END, \
                terminal_at = NULL, \
                last_dispatch_at = now() \
          WHERE id = $4 \
            AND state IN ('queued','running','sleeping','waiting','compensating') \
            AND (claimed_by IS NULL OR lease_expires IS NULL OR lease_expires <= now()) \
          RETURNING id",
        runs = tables.runs
    );
    let rows = tx
        .query(
            &update_sql,
            &[
                &config.owner_id,
                &lease_expires,
                &dispatch_nonce,
                &candidate.run_id,
            ],
        )
        .await?;
    if rows.is_empty() {
        return Ok(WorkflowClaimOutcome::ClaimLost);
    }

    let journal = load_journal(tx, tables, &candidate.run_id).await?;
    Ok(WorkflowClaimOutcome::Claimed(StepRequest {
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
    }))
}

async fn load_journal<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
) -> Result<Vec<JournalStep>, WorkflowError>
where
    C: GenericClient + Sync,
{
    let sql = format!(
        "SELECT ordinal, name, name_occurrence, kind, state, output, error, child_run_id, \
                output_kind, output_hash, output_size, output_content_type, \
                compensation_state \
           FROM {steps} \
          WHERE run_id = $1 \
          ORDER BY ordinal",
        steps = tables.steps
    );
    let rows = conn.query(&sql, &[&run_id]).await?;
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

fn is_claimable_state(state: &str) -> bool {
    matches!(
        state,
        "queued" | "running" | "sleeping" | "waiting" | "compensating"
    )
}

fn is_terminal_state(state: &str) -> bool {
    matches!(state, "completed" | "failed" | "cancelled" | "stalled")
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

fn parse_waiting_step_key(key: &str) -> Result<WaitingStep, WorkflowError> {
    let parts: Vec<&str> = key.split(':').collect();
    match parts.as_slice() {
        ["sleep", ordinal, name] => {
            let ordinal = ordinal.parse::<i32>().map_err(|_| {
                WorkflowError::Invalid(format!("invalid sleep waiting_step_key ordinal: {key}"))
            })?;
            Ok(WaitingStep::Sleep {
                ordinal,
                name: (*name).to_string(),
            })
        }
        ["wait", ordinal, name, signal_type] => {
            let ordinal = ordinal.parse::<i32>().map_err(|_| {
                WorkflowError::Invalid(format!("invalid wait waiting_step_key ordinal: {key}"))
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
                WorkflowError::Invalid(format!("invalid wait waiting_step_key ordinal: {key}"))
            })?;
            let max_signal_age_ms = max_age.parse::<i64>().map_err(|_| {
                WorkflowError::Invalid(format!("invalid wait waiting_step_key max age: {key}"))
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
                WorkflowError::Invalid(format!("invalid child waiting_step_key ordinal: {key}"))
            })?;
            Ok(WaitingStep::Child {
                ordinal,
                name: (*name).to_string(),
            })
        }
        _ => Err(WorkflowError::Invalid(format!(
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
) -> Result<bool, WorkflowError>
where
    C: GenericClient + Sync,
{
    match parse_waiting_step_key(key)? {
        WaitingStep::Sleep { ordinal, name } => {
            if insert_resolved_step_on_conn(
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
                &format!(
                    "UPDATE {runs} \
                        SET waiting_step_key = NULL, wake_at = now() \
                      WHERE id = $1",
                    runs = tables.runs
                ),
                &[&run_id],
            )
            .await?;
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
                    &format!(
                        "SELECT wake_at \
                           FROM {steps} \
                          WHERE run_id = $1 \
                            AND ordinal = $2 \
                            AND name = $3 \
                            AND kind = 'wait_signal' \
                            AND state = 'running' \
                          FOR UPDATE",
                        steps = tables.steps
                    ),
                    &[&run_id, &ordinal, &name],
                )
                .await?;
            let Some(step_row) = step_rows.first() else {
                return Err(WorkflowError::Invalid(format!(
                    "waiting_step_key {key} has no running workflow_steps row"
                )));
            };
            let deadline: Option<DateTime<Utc>> = step_row.get("wake_at");
            let now = Utc::now();
            let min_created_at =
                max_signal_age_ms.map(|age| now - chrono::Duration::milliseconds(age));
            if let Some(stale_cutoff) = min_created_at.as_ref() {
                tx.execute(
                    &format!(
                        "UPDATE {signals} \
                            SET consumed_by = $1 \
                          WHERE run_id = $1 \
                            AND type = $2 \
                            AND consumed_by IS NULL \
                            AND created_at < $3",
                        signals = tables.signals
                    ),
                    &[&run_id, &signal_type, stale_cutoff],
                )
                .await?;
            }
            let signal = tx
                .query(
                    &format!(
                        "SELECT id, payload, created_at, origin, delivery, topic \
                           FROM {signals} \
                          WHERE run_id = $1 \
                            AND type = $2 \
                            AND consumed_by IS NULL \
                            AND ($3::timestamptz IS NULL OR created_at >= $3) \
                          ORDER BY created_at, id \
                          LIMIT 1 \
                          FOR UPDATE SKIP LOCKED",
                        signals = tables.signals
                    ),
                    &[&run_id, &signal_type, &min_created_at],
                )
                .await?;

            let Some(row) = signal.first() else {
                if deadline.is_some_and(|deadline| deadline <= now) {
                    if insert_resolved_step_on_conn(
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
                        &format!(
                            "UPDATE {runs} \
                                SET waiting_step_key = NULL, wake_at = now() \
                              WHERE id = $1",
                            runs = tables.runs
                        ),
                        &[&run_id],
                    )
                    .await?;
                    delete_workflow_subscription(tx, tables, run_id, ordinal).await?;
                    return Ok(true);
                }

                tx.execute(
                    &format!(
                        "UPDATE {runs} \
                            SET state = 'waiting', wake_at = $2 \
                          WHERE id = $1",
                        runs = tables.runs
                    ),
                    &[&run_id, &deadline],
                )
                .await?;
                return Ok(false);
            };

            let signal_id: String = row.get("id");
            let payload: Option<Value> = row.get("payload");
            let created_at: DateTime<Utc> = row.get("created_at");
            let origin: String = row.get("origin");
            let delivery: String = row.get("delivery");
            let topic: Option<String> = row.get("topic");
            if insert_resolved_step_on_conn(
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
                &format!(
                    "UPDATE {signals} \
                        SET consumed_by = $1 \
                      WHERE id = $2 AND consumed_by IS NULL",
                    signals = tables.signals
                ),
                &[&run_id, &signal_id],
            )
            .await?;
            tx.execute(
                &format!(
                    "UPDATE {runs} \
                        SET waiting_step_key = NULL, wake_at = now() \
                      WHERE id = $1",
                    runs = tables.runs
                ),
                &[&run_id],
            )
            .await?;
            delete_workflow_subscription(tx, tables, run_id, ordinal).await?;
            Ok(true)
        }
        WaitingStep::Child { ordinal, name } => {
            let step_rows = tx
                .query(
                    &format!(
                        "SELECT wake_at, child_run_id, name_occurrence \
                           FROM {steps} \
                          WHERE run_id = $1 \
                            AND ordinal = $2 \
                            AND name = $3 \
                            AND kind = 'child' \
                            AND state = 'running' \
                          FOR UPDATE",
                        steps = tables.steps
                    ),
                    &[&run_id, &ordinal, &name],
                )
                .await?;
            let Some(step_row) = step_rows.first() else {
                return Err(WorkflowError::Invalid(format!(
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
                    &format!(
                        "SELECT id, payload \
                           FROM {signals} \
                          WHERE run_id = $1 \
                            AND type = $2 \
                            AND consumed_by IS NULL \
                          ORDER BY created_at, id \
                          LIMIT 1 \
                          FOR UPDATE SKIP LOCKED",
                        signals = tables.signals
                    ),
                    &[&run_id, &signal_type],
                )
                .await?;

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
                        &format!(
                            "SELECT sig.id, sig.payload, s.ordinal, s.name, s.name_occurrence, s.child_run_id \
                               FROM {steps} s \
                               JOIN {signals} sig \
                                 ON sig.run_id = s.run_id \
                                AND sig.type = s.signal_type \
                                AND sig.consumed_by IS NULL \
                              WHERE s.run_id = $1 \
                                AND s.kind = 'child' \
                                AND s.state = 'running' \
                              ORDER BY sig.created_at, sig.id \
                              LIMIT 1 \
                              FOR UPDATE OF sig SKIP LOCKED",
                            steps = tables.steps,
                            signals = tables.signals,
                        ),
                        &[&run_id],
                    )
                    .await?;
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
                            &format!(
                                "UPDATE {runs} \
                                    SET cancel_requested = true, wake_at = now() \
                                  WHERE id = $1 \
                                    AND parent_cascade \
                                    AND state NOT IN ('completed','failed','cancelled','stalled')",
                                runs = tables.runs
                            ),
                            &[child_run_id],
                        )
                        .await?;
                    }
                    if insert_resolved_step_on_conn(
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
                        &format!(
                            "UPDATE {runs} \
                                SET waiting_step_key = NULL, wake_at = now() \
                              WHERE id = $1",
                            runs = tables.runs
                        ),
                        &[&run_id],
                    )
                    .await?;
                    return Ok(true);
                }

                tx.execute(
                    &format!(
                        "UPDATE {runs} \
                            SET state = 'waiting', wake_at = $2 \
                          WHERE id = $1",
                        runs = tables.runs
                    ),
                    &[&run_id, &deadline],
                )
                .await?;
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
            if insert_resolved_step_on_conn(tx, tables, config, &checkpoint, run_id, dispatch_nonce, 1)
                .await?
                == StepWriteOutcome::CapExceeded
            {
                return Ok(false);
            }
            tx.execute(
                &format!(
                    "UPDATE {signals} \
                        SET consumed_by = $1 \
                      WHERE id = $2 AND consumed_by IS NULL",
                    signals = tables.signals
                ),
                &[&run_id, &signal_id],
            )
            .await?;
            tx.execute(
                &format!(
                    "UPDATE {runs} \
                        SET waiting_step_key = NULL, wake_at = now() \
                      WHERE id = $1",
                    runs = tables.runs
                ),
                &[&run_id],
            )
            .await?;
            Ok(true)
        }
    }
}

async fn has_due_compensation<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
) -> Result<bool, WorkflowError>
where
    C: GenericClient + Sync,
{
    let sql = format!(
        "SELECT 1 \
           FROM {steps} \
          WHERE run_id = $1 \
            AND ( \
                compensation_state = 'pending' \
                OR (compensation_state = 'running' \
                    AND (compensation_wake_at IS NULL OR compensation_wake_at <= now())) \
            ) \
          ORDER BY ordinal DESC \
          LIMIT 1",
        steps = tables.steps
    );
    let rows = conn.query(&sql, &[&run_id]).await?;
    Ok(!rows.is_empty())
}

async fn finalize_compensation_if_drained<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
) -> Result<bool, WorkflowError>
where
    C: GenericClient + Sync,
{
    let progress = compensation_progress_on_conn(conn, tables, run_id).await?;
    if progress.remaining() > 0 {
        return Ok(false);
    }
    let select_sql = format!(
        "SELECT compensation_target, error \
           FROM {runs} \
          WHERE id = $1 AND state = 'compensating' \
          FOR UPDATE",
        runs = tables.runs
    );
    let rows = conn.query(&select_sql, &[&run_id]).await?;
    let Some(row) = rows.first() else {
        return Ok(false);
    };
    let target = row
        .get::<_, Option<String>>("compensation_target")
        .unwrap_or_else(|| "failed".to_string());
    let current_error: Option<Value> = row.get("error");
    let outcome = progress.terminal_outcome().to_string();
    let error = compensation_progress_error(current_error, progress, Some(outcome.as_str()));
    let update_sql = format!(
        "UPDATE {runs} \
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
        runs = tables.runs
    );
    let changed = conn
        .execute(&update_sql, &[&run_id, &target, &error, &outcome])
        .await?;
    if changed > 0 && matches!(target.as_str(), "failed" | "cancelled") {
        emit_child_terminal_hook_on_conn(
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
        cascade_cancel_children_on_conn(conn, tables, run_id).await?;
    }
    Ok(changed > 0)
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
) -> Result<bool, WorkflowError>
where
    C: GenericClient + Sync,
{
    let sql = format!(
        "UPDATE {runs} \
            SET state = 'cancelled', \
                cancel_requested = false, \
                wake_at = NULL, \
                terminal_at = now(), \
                waiting_step_key = NULL, \
                claimed_by = NULL, \
                lease_expires = NULL, \
                dispatch_nonce = NULL \
          WHERE id = $1 \
            AND cancel_requested \
            AND state NOT IN ('completed','failed','cancelled','stalled')",
        runs = tables.runs
    );
    let changed = conn.execute(&sql, &[&run_id]).await?;
    if changed == 0 {
        return Ok(false);
    }

    emit_child_terminal_hook_on_conn(
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
    cascade_cancel_children_on_conn(conn, tables, run_id).await?;
    Ok(true)
}

async fn delete_workflow_subscription<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
    ordinal: i32,
) -> Result<(), WorkflowError>
where
    C: GenericClient + Sync,
{
    let sql = format!(
        "DELETE FROM {subscriptions} \
          WHERE run_id = $1 AND ordinal = $2",
        subscriptions = tables.subscriptions
    );
    conn.execute(&sql, &[&run_id, &ordinal]).await?;
    Ok(())
}
