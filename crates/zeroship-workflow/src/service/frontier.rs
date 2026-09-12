use super::{
    app::{active_deploy, deadline, decode, emit, encode, insert_root_run, parse_state},
    journal,
    store::{Row, Transaction},
    AppPolicy, ControlIntent,
};
use crate::{
    engine::{fold_outcomes, RunUpdate, StepOutcome},
    operations::{RunState, StartOptions},
    WorkflowExecution, WorkflowInvocation, WorkflowServiceError, WorkflowTrigger,
};
use chrono::DateTime;
use serde_json::{json, Value};
use zeroship_core::{app_id::AppId, typed_id};

pub(crate) async fn invocation(
    tx: &mut Transaction,
    app: &AppId,
    run: &Row,
) -> Result<WorkflowInvocation, WorkflowServiceError> {
    let id = run.text("id")?;
    let generations = tx.table("generations");
    let deploys = tx.table("deploys");
    let rows=tx.query(&format!("SELECT g.input,g.input_ref,g.started_at,d.hash FROM {generations} g JOIN {deploys} d ON d.app_id=g.app_id AND d.id=g.deploy_id WHERE g.app_id=$1 AND g.run_id=$2 AND g.generation=$3 AND d.state='available'"), &[app.as_str().into(),id.clone().into(),run.integer("generation")?.into()]).await?;
    let row = rows.first().ok_or_else(|| {
        WorkflowServiceError::Unavailable("workflow pinned executable is unavailable".into())
    })?;
    let workflow_name = run.text("workflow_name")?;
    Ok(WorkflowInvocation {
        app_id: app.as_str().into(),
        deploy_id: run.text("deploy_id")?,
        deploy_hash: row.text("hash")?,
        run_id: id.clone(),
        workflow_name: workflow_name.clone(),
        phase: if run.text("state")? == "compensating" {
            "compensating"
        } else {
            "forward"
        }
        .into(),
        trigger: WorkflowTrigger {
            input: Some(decode(&row.text("input")?)?),
            input_ref: row
                .optional_text("input_ref")?
                .map(|value| decode(&value))
                .transpose()?,
            started_at: DateTime::from_timestamp_millis(row.integer("started_at")?).ok_or_else(
                || WorkflowServiceError::Internal("invalid workflow start time".into()),
            )?,
            run_id: id.clone(),
            workflow_name,
        },
        journal: journal::replay(&journal::load(tx, app, &id, run.integer("generation")?).await?),
    })
}

/// Reconcile control intent and durable waits before assigning an executor.
pub(crate) async fn prepare(
    tx: &mut Transaction,
    app: &AppId,
    run: &Row,
    now: i64,
) -> Result<bool, WorkflowServiceError> {
    let intent = ControlIntent::parse(&run.text("control")?)?;
    if intent == ControlIntent::Cancel {
        settle(tx, app, run, RunUpdate::Cancelled, now).await?;
        return if has_compensation(tx, app, run).await? {
            compensation_ready(tx, app, run, now).await
        } else {
            Ok(false)
        };
    }
    if intent == ControlIntent::Pause {
        park(tx, app, run).await?;
        return Ok(false);
    }
    if run.text("state")? == "compensating" {
        return compensation_ready(tx, app, run, now).await;
    }
    let progressed = journal::resolve(tx, app, run, now).await?;
    let steps = journal::load(tx, app, &run.text("id")?, run.integer("generation")?).await?;
    let pending = steps.iter().any(|step| step.state == "running");
    if pending && !progressed {
        suspend(tx, app, run, now, false).await?;
        return Ok(false);
    }
    Ok(true)
}

pub(crate) async fn apply(
    tx: &mut Transaction,
    app: &AppId,
    run: &Row,
    policy: &AppPolicy,
    execution: WorkflowExecution,
    now: i64,
) -> Result<RunState, WorkflowServiceError> {
    if execution.outcomes.is_empty() {
        return journal::invalid("workflow completion requires an outcome");
    }
    if execution.outcomes.len() > policy.max_frontier {
        return Err(WorkflowServiceError::ResourceExhausted(
            "workflow frontier exceeds the configured limit".into(),
        ));
    }
    if encode(&execution)?.len() > policy.max_input_bytes {
        return Err(WorkflowServiceError::PayloadTooLarge);
    }
    if run.text("state")? == "compensating" {
        return compensate(tx, app, run, execution, now).await;
    }
    if execution.outcomes.iter().any(|outcome| {
        matches!(
            outcome,
            StepOutcome::CompensationCompleted { .. } | StepOutcome::CompensationFailed { .. }
        )
    }) {
        return journal::invalid("forward task cannot submit compensation results");
    }
    for outcome in &execution.outcomes {
        if let StepOutcome::StepCompleted {
            compensation_max_attempts,
            ..
        } = outcome
        {
            if *compensation_max_attempts <= 0
                || *compensation_max_attempts > policy.max_compensation_attempts
            {
                return journal::invalid(
                    "workflow compensation retry policy exceeds the app limit",
                );
            }
        }
    }
    let (checkpoints, update) =
        fold_outcomes(&execution.outcomes).map_err(WorkflowServiceError::InvalidRequest)?;
    journal::append(tx, app, run, policy, checkpoints, now).await?;
    let intent = ControlIntent::parse(&run.text("control")?)?;
    if intent == ControlIntent::Cancel {
        return settle(tx, app, run, RunUpdate::Cancelled, now).await;
    }
    if matches!(update, RunUpdate::Failed { .. } | RunUpdate::Cancelled) {
        return settle(tx, app, run, update, now).await;
    }
    if let RunUpdate::Completed { output, output_ref } = update {
        if let Some(reference) = &output_ref {
            if output.is_some() {
                return journal::invalid("workflow output cannot be both inline and referenced");
            }
            super::payloads::promote(
                tx,
                app,
                run,
                super::payloads::RunGeneration {
                    id: &run.text("id")?,
                    generation: run.integer("generation")?,
                },
                super::PayloadSlot::Output,
                reference,
                now,
            )
            .await?;
            let generations = tx.table("generations");
            tx.execute(&format!("UPDATE {generations} SET output_ref=$4 WHERE app_id=$1 AND run_id=$2 AND generation=$3"), &[app.as_str().into(),run.text("id")?.into(),run.integer("generation")?.into(),encode(reference)?.into()]).await?;
        }
        if journal::load(tx, app, &run.text("id")?, run.integer("generation")?)
            .await?
            .iter()
            .any(|step| step.state == "running")
        {
            return journal::invalid("workflow cannot complete with unresolved operations");
        }
        return finish(tx, app, run, RunState::Completed, output, None, now).await;
    }
    if let RunUpdate::ContinuedAsNew {
        seed_input,
        seed_input_ref,
    } = update
    {
        if seed_input_ref.is_some() && seed_input.is_some() {
            return journal::invalid("workflow input cannot be both inline and referenced");
        }
        let steps = journal::load(tx, app, &run.text("id")?, run.integer("generation")?).await?;
        if steps.iter().any(|step| {
            step.state == "running"
                || step
                    .compensation_state
                    .as_deref()
                    .is_some_and(|state| matches!(state, "pending" | "running"))
        }) {
            return journal::invalid(
                "workflow cannot continue with unresolved work or compensation obligations",
            );
        }
        policy.admit()?;
        let deploy = active_deploy(tx, app).await?;
        let name = run.text("workflow_name")?;
        if !deploy.workflows.contains(&name) {
            return journal::invalid("workflow is absent from the active deployment");
        }
        let id = typed_id::new_workflow_run_id();
        let key = run.optional_text("key")?;
        finish(
            tx,
            app,
            run,
            RunState::Completed,
            Some(json!({"continuedAsNew":id})),
            None,
            now,
        )
        .await?;
        insert_root_run(
            tx,
            app,
            &id,
            &name,
            &deploy.id,
            &StartOptions {
                input: seed_input.unwrap_or(Value::Null),
                key,
                ..Default::default()
            },
            now,
        )
        .await?;
        if let Some(reference) = seed_input_ref {
            super::payloads::promote(
                tx,
                app,
                run,
                super::payloads::RunGeneration {
                    id: &id,
                    generation: 0,
                },
                super::PayloadSlot::Input,
                &reference,
                now,
            )
            .await?;
            let generations = tx.table("generations");
            tx.execute(&format!("UPDATE {generations} SET input_ref=$3 WHERE app_id=$1 AND run_id=$2 AND generation=0"), &[app.as_str().into(),id.clone().into(),encode(&reference)?.into()]).await?;
        }
        link_continuation(tx, app, run, &id).await?;
        return Ok(RunState::Completed);
    }
    if intent == ControlIntent::Pause {
        park(tx, app, run).await?;
        return Ok(RunState::Paused);
    }
    let progressed = journal::resolve(tx, app, run, now).await?;
    suspend(tx, app, run, now, progressed).await
}

async fn suspend(
    tx: &mut Transaction,
    app: &AppId,
    run: &Row,
    now: i64,
    progressed: bool,
) -> Result<RunState, WorkflowServiceError> {
    let id = run.text("id")?;
    let steps = journal::load(tx, app, &id, run.integer("generation")?).await?;
    let pending: Vec<_> = steps
        .iter()
        .filter(|step| step.state == "running")
        .collect();
    let state = if pending.is_empty() || progressed {
        RunState::Queued
    } else if pending.iter().all(|step| step.kind == "sleep") {
        RunState::Sleeping
    } else {
        RunState::Waiting
    };
    let due = if state == RunState::Queued {
        Some(now)
    } else {
        pending
            .iter()
            .filter_map(|step| step.wake_at.map(|time| time.timestamp_millis()))
            .min()
    };
    let runs = tx.table("runs");
    tx.execute(
        &format!("UPDATE {runs} SET state=$3,due_at=$4,task_id=NULL WHERE app_id=$1 AND id=$2"),
        &[
            app.as_str().into(),
            id.into(),
            state.as_str().into(),
            due.into(),
        ],
    )
    .await?;
    Ok(state)
}
pub(crate) async fn park(
    tx: &mut Transaction,
    app: &AppId,
    run: &Row,
) -> Result<(), WorkflowServiceError> {
    let runs = tx.table("runs");
    tx.execute(
        &format!(
            "UPDATE {runs} SET state='paused',due_at=NULL,task_id=NULL WHERE app_id=$1 AND id=$2"
        ),
        &[app.as_str().into(), run.text("id")?.into()],
    )
    .await?;
    Ok(())
}
async fn has_compensation(
    tx: &mut Transaction,
    app: &AppId,
    run: &Row,
) -> Result<bool, WorkflowServiceError> {
    Ok(
        journal::load(tx, app, &run.text("id")?, run.integer("generation")?)
            .await?
            .iter()
            .any(|step| {
                step.compensation_state
                    .as_deref()
                    .is_some_and(|state| matches!(state, "pending" | "running"))
            }),
    )
}
async fn settle(
    tx: &mut Transaction,
    app: &AppId,
    run: &Row,
    update: RunUpdate,
    now: i64,
) -> Result<RunState, WorkflowServiceError> {
    let id = run.text("id")?;
    let target = parse_state(update.state())?;
    let runs = tx.table("runs");
    // Child cancellation remains durable while a child owns an accepted task.
    tx.execute(&format!("UPDATE {runs} SET control='cancel',due_at=CASE WHEN task_id IS NULL THEN $4 ELSE due_at END WHERE app_id=$1 AND parent_id=$2 AND parent_generation=$3 AND cascade=1 AND state NOT IN ('completed','failed','cancelled')"), &[app.as_str().into(),id.clone().into(),run.integer("generation")?.into(),now.into()]).await?;
    if has_compensation(tx, app, run).await? {
        let generations = tx.table("generations");
        tx.execute(
            &format!(
                "UPDATE {generations} SET error=$4 WHERE app_id=$1 AND run_id=$2 AND generation=$3"
            ),
            &[
                app.as_str().into(),
                id.clone().into(),
                run.integer("generation")?.into(),
                update
                    .error()
                    .map(|value| encode(&value))
                    .transpose()?
                    .into(),
            ],
        )
        .await?;
        tx.execute(&format!("UPDATE {runs} SET state='compensating',compensation_target=$3,control='none',due_at=$4,task_id=NULL WHERE app_id=$1 AND id=$2"), &[app.as_str().into(),id.into(),target.as_str().into(),now.into()]).await?;
        Ok(RunState::Compensating)
    } else {
        finish(tx, app, run, target, None, update.error(), now).await
    }
}
async fn compensation_ready(
    tx: &mut Transaction,
    app: &AppId,
    run: &Row,
    now: i64,
) -> Result<bool, WorkflowServiceError> {
    let id = run.text("id")?;
    let pending = journal::load(tx, app, &id, run.integer("generation")?)
        .await?
        .into_iter()
        .rev()
        .find(|step| {
            step.compensation_state
                .as_deref()
                .is_some_and(|state| matches!(state, "pending" | "running"))
        })
        .ok_or_else(|| {
            WorkflowServiceError::Internal("compensating workflow has no pending operation".into())
        })?;
    let steps = tx.table("steps");
    let rows=tx.query(&format!("SELECT compensation_due_at FROM {steps} WHERE app_id=$1 AND run_id=$2 AND generation=$3 AND ordinal=$4"), &[app.as_str().into(),id.clone().into(),run.integer("generation")?.into(),i64::from(pending.ordinal).into()]).await?;
    let due = rows[0]
        .optional_integer("compensation_due_at")?
        .unwrap_or(now);
    if due > now {
        let runs = tx.table("runs");
        tx.execute(
            &format!("UPDATE {runs} SET due_at=$3 WHERE app_id=$1 AND id=$2"),
            &[app.as_str().into(), id.into(), due.into()],
        )
        .await?;
        return Ok(false);
    }
    Ok(true)
}

async fn compensate(
    tx: &mut Transaction,
    app: &AppId,
    run: &Row,
    execution: WorkflowExecution,
    now: i64,
) -> Result<RunState, WorkflowServiceError> {
    let id = run.text("id")?;
    let generation = run.integer("generation")?;
    let mut pending = journal::load(tx, app, &id, generation)
        .await?
        .into_iter()
        .rev()
        .find(|step| {
            step.compensation_state
                .as_deref()
                .is_some_and(|state| matches!(state, "pending" | "running"))
        })
        .ok_or_else(|| {
            WorkflowServiceError::Internal("compensating workflow has no pending operation".into())
        })?;
    let (ordinal, name, occurrence, error) = match execution.outcomes.as_slice() {
        [StepOutcome::CompensationCompleted {
            ordinal,
            name,
            name_occurrence,
        }] => (*ordinal, name, *name_occurrence, None),
        [StepOutcome::CompensationFailed {
            ordinal,
            name,
            name_occurrence,
            error,
        }] => (*ordinal, name, *name_occurrence, Some(error.clone())),
        _ => return journal::invalid("compensation task requires a compensation outcome"),
    };
    if ordinal != pending.ordinal || *name != pending.name || occurrence != pending.name_occurrence
    {
        return journal::invalid("compensation outcome does not match the pending operation");
    }
    let steps = tx.table("steps");
    let rows=tx.query(&format!("SELECT compensation_attempts,compensation_retry_ms FROM {steps} WHERE app_id=$1 AND run_id=$2 AND generation=$3 AND ordinal=$4"), &[app.as_str().into(),id.clone().into(),generation.into(),i64::from(ordinal).into()]).await?;
    let attempts = rows[0]
        .integer("compensation_attempts")?
        .checked_add(1)
        .ok_or_else(|| WorkflowServiceError::Internal("compensation attempt overflow".into()))?;
    let retry = error.is_some() && attempts < i64::from(pending.compensation_max_attempts);
    pending.compensation_state = Some(
        if retry {
            "pending"
        } else if error.is_some() {
            "failed"
        } else {
            "completed"
        }
        .into(),
    );
    journal::update(tx, app, &id, generation, &pending).await?;
    let due = if retry {
        Some(deadline(now, rows[0].integer("compensation_retry_ms")?)?)
    } else {
        None
    };
    tx.execute(&format!("UPDATE {steps} SET compensation_attempts=$5,compensation_due_at=$6,compensation_error=$7 WHERE app_id=$1 AND run_id=$2 AND generation=$3 AND ordinal=$4"),
        &[app.as_str().into(),id.clone().into(),generation.into(),i64::from(ordinal).into(),attempts.into(),due.into(),error.map(|value|encode(&value)).transpose()?.into()]).await?;
    if has_compensation(tx, app, run).await? {
        if ControlIntent::parse(&run.text("control")?)? == ControlIntent::Pause {
            park(tx, app, run).await?;
            return Ok(RunState::Paused);
        }
        let runs = tx.table("runs");
        tx.execute(
            &format!("UPDATE {runs} SET task_id=NULL,due_at=$3 WHERE app_id=$1 AND id=$2"),
            &[
                app.as_str().into(),
                id.clone().into(),
                due.unwrap_or(now).into(),
            ],
        )
        .await?;
        return Ok(RunState::Compensating);
    }
    let generations = tx.table("generations");
    let rows = tx
        .query(
            &format!(
                "SELECT error FROM {generations} WHERE app_id=$1 AND run_id=$2 AND generation=$3"
            ),
            &[app.as_str().into(), id.clone().into(), generation.into()],
        )
        .await?;
    let original: Option<Value> = rows[0]
        .optional_text("error")?
        .map(|value| decode(&value))
        .transpose()?;
    let failures=tx.query(&format!("SELECT ordinal,compensation_error FROM {steps} WHERE app_id=$1 AND run_id=$2 AND generation=$3 AND compensation_error IS NOT NULL ORDER BY ordinal DESC"), &[app.as_str().into(),id.into(),generation.into()]).await?;
    if failures.is_empty() {
        return finish(
            tx,
            app,
            run,
            parse_state(&run.text("compensation_target")?)?,
            None,
            original,
            now,
        )
        .await;
    }
    let failures:Vec<Value>=failures.iter().map(|row|Ok(json!({"ordinal":row.integer("ordinal")?,"error":decode::<Value>(&row.text("compensation_error")?)?}))).collect::<Result<_,WorkflowServiceError>>()?;
    finish(tx,app,run,RunState::Failed,None,Some(json!({"name":"WorkflowCompensationError","message":"workflow compensation did not fully succeed","cause":original,"failures":failures})),now).await
}
pub(crate) async fn finish(
    tx: &mut Transaction,
    app: &AppId,
    run: &Row,
    state: RunState,
    output: Option<Value>,
    error: Option<Value>,
    now: i64,
) -> Result<RunState, WorkflowServiceError> {
    let id = run.text("id")?;
    let generation = run.integer("generation")?;
    let runs = tx.table("runs");
    let generations = tx.table("generations");
    tx.execute(&format!("UPDATE {runs} SET state=$3,control='none',task_id=NULL,due_at=NULL,key=NULL,terminal_at=$4 WHERE app_id=$1 AND id=$2"), &[app.as_str().into(),id.clone().into(),state.as_str().into(),now.into()]).await?;
    tx.execute(&format!("UPDATE {generations} SET state=$4,output=$5,error=$6,terminal_at=$7 WHERE app_id=$1 AND run_id=$2 AND generation=$3"), &[app.as_str().into(),id.clone().into(),generation.into(),state.as_str().into(),output.map(|value|encode(&value)).transpose()?.into(),error.map(|value|encode(&value)).transpose()?.into(),now.into()]).await?;
    for table in ["waits", "subscriptions"] {
        let table = tx.table(table);
        tx.execute(
            &format!("DELETE FROM {table} WHERE app_id=$1 AND run_id=$2 AND generation=$3"),
            &[app.as_str().into(), id.clone().into(), generation.into()],
        )
        .await?;
    }
    let waits = tx.table("waits");
    tx.execute(&format!("UPDATE {runs} SET due_at=$3 WHERE app_id=$1 AND task_id IS NULL AND control='none' AND state IN ('waiting','sleeping','queued') AND EXISTS (SELECT 1 FROM {waits} w WHERE w.app_id=$1 AND w.run_id={runs}.id AND w.generation={runs}.generation AND w.child_id=$2)"), &[app.as_str().into(),id.clone().into(),now.into()]).await?;
    emit(
        tx,
        app,
        &format!("{id}:{generation}:terminal"),
        "workflow.terminal",
        json!({"runId":id,"generation":generation,"state":state}),
        now,
    )
    .await?;
    Ok(state)
}

async fn link_continuation(
    tx: &mut Transaction,
    app: &AppId,
    run: &Row,
    successor: &str,
) -> Result<(), WorkflowServiceError> {
    let id = run.text("id")?;
    let runs = tx.table("runs");
    tx.execute(
        &format!("UPDATE {runs} SET continued_to_id=$3 WHERE app_id=$1 AND id=$2"),
        &[app.as_str().into(), id.clone().into(), successor.into()],
    )
    .await?;
    tx.execute(&format!("UPDATE {runs} SET continued_from_id=$3,parent_id=$4,parent_generation=$5,parent_ordinal=$6,cascade=$7,depth=$8,schedule_id=$9 WHERE app_id=$1 AND id=$2"),
        &[app.as_str().into(),successor.into(),id.clone().into(),run.optional_text("parent_id")?.into(),run.optional_integer("parent_generation")?.into(),run.optional_integer("parent_ordinal")?.into(),run.integer("cascade")?.into(),run.integer("depth")?.into(),run.optional_text("schedule_id")?.into()]).await?;
    // A parent's durable wait follows the continuation. It must not observe the
    // intermediate run's terminal acknowledgement as the child's final result.
    let waits = tx.table("waits");
    let steps = tx.table("steps");
    let parents=tx.query(&format!("SELECT s.run_id,s.generation,s.record FROM {steps} s JOIN {waits} w ON w.app_id=s.app_id AND w.run_id=s.run_id AND w.generation=s.generation AND w.ordinal=s.ordinal WHERE w.app_id=$1 AND w.child_id=$2 ORDER BY s.run_id,s.ordinal"), &[app.as_str().into(),id.clone().into()]).await?;
    for parent in parents {
        let mut step: crate::engine::StepCheckpoint = decode(&parent.text("record")?)?;
        step.child_run_id = Some(successor.into());
        journal::update(
            tx,
            app,
            &parent.text("run_id")?,
            parent.integer("generation")?,
            &step,
        )
        .await?;
    }
    tx.execute(
        &format!("UPDATE {waits} SET child_id=$3 WHERE app_id=$1 AND child_id=$2"),
        &[app.as_str().into(), id.into(), successor.into()],
    )
    .await?;
    Ok(())
}
