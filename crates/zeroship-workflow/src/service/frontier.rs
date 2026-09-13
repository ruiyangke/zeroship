use super::{
    app::{active_deploy, deadline, decode, emit, encode, insert_root_run, parse_state},
    journal, models,
    store::{Row, Transaction},
    AppPolicy, ControlIntent,
};
use crate::service::policy::admit;
use crate::{
    engine::{fold_outcomes, RunUpdate, StepOutcome},
    operations::{RunState, StartOptions},
    WorkflowExecution, WorkflowInvocation, WorkflowServiceError, WorkflowTrigger,
};
use chrono::DateTime;
use serde_json::{json, Value};
use zeroship_core::{app_id::AppId, typed_id};
use zeroship_data_orm::{
    orm::{Entity, FindOptions, FromRow, Operation},
    sql::{CompareOp, Literal, Operand, Predicate, RowLimit},
    value,
};

#[derive(FromRow)]
#[orm(entity = models::waits)]
struct WaitingParent {
    id: String,
    run_id: String,
    generation: i64,
}

#[derive(FromRow)]
#[orm(entity = models::runs)]
struct CancelledChild {
    id: String,
}

pub(crate) async fn invocation(
    tx: &mut Transaction,
    app: &AppId,
    run: &Row,
) -> Result<WorkflowInvocation, WorkflowServiceError> {
    let id = run.text("id")?;
    let db = tx.database();
    let generation = db.entity::<models::generations::Entity>()?.alias("g")?;
    let deployment = db.entity::<models::deploys::Entity>()?.alias("d")?;
    let (input, deployment) = db
        .from(&generation)
        .inner_join(
            &deployment,
            Predicate::And(vec![
                generation
                    .column(models::generations::app_id)
                    .eq_column(deployment.column(models::deploys::app_id))?,
                generation
                    .column(models::generations::deploy_id)
                    .eq_column(deployment.column(models::deploys::id))?,
            ]),
        )?
        .filter(Predicate::And(vec![
            generation
                .column(models::generations::app_id)
                .eq(app.as_str())?,
            generation
                .column(models::generations::run_id)
                .eq(id.as_str())?,
            generation
                .column(models::generations::generation)
                .eq(run.integer("generation")?)?,
            deployment.column(models::deploys::state).eq("available")?,
        ]))
        .select((
            generation.row::<models::GenerationInput>(),
            deployment.row::<models::DeploymentHash>(),
        ))?
        .limit(1)?
        .all()
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| {
            WorkflowServiceError::Unavailable("workflow pinned executable is unavailable".into())
        })?;
    let workflow_name = run.text("workflow_name")?;
    Ok(WorkflowInvocation {
        app_id: app.as_str().into(),
        deploy_id: run.text("deploy_id")?,
        deploy_hash: deployment.hash,
        run_id: id.clone(),
        workflow_name: workflow_name.clone(),
        phase: if run.text("state")? == "compensating" {
            "compensating"
        } else {
            "forward"
        }
        .into(),
        trigger: WorkflowTrigger {
            input: Some(decode(&input.input)?),
            input_ref: input.input_ref.map(|value| decode(&value)).transpose()?,
            started_at: DateTime::from_timestamp_millis(input.started_at).ok_or_else(|| {
                WorkflowServiceError::Internal("invalid workflow start time".into())
            })?,
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
            tx.database().collection(models::generations::Entity::COLLECTION)?.update(
                value!({"app_id":app.as_str(), "run_id":run.text("id")?, "generation":run.integer("generation")?}),
                value!({"output_ref":encode(reference)?}),
            ).await?;
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
        admit(policy)?;
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
            tx.database()
                .collection(models::generations::Entity::COLLECTION)?
                .update(
                    value!({"app_id":app.as_str(), "run_id":id.clone(), "generation":0}),
                    value!({"input_ref":encode(&reference)?}),
                )
                .await?;
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
    tx.database()
        .collection(models::runs::Entity::COLLECTION)?
        .update(
            value!({"app_id":app.as_str(), "id":id}),
            value!({"state":state.as_str(), "due_at":due, "task_id":null}),
        )
        .await?;
    Ok(state)
}
pub(crate) async fn park(
    tx: &mut Transaction,
    app: &AppId,
    run: &Row,
) -> Result<(), WorkflowServiceError> {
    tx.database()
        .collection(models::runs::Entity::COLLECTION)?
        .update(
            value!({"app_id":app.as_str(), "id":run.text("id")?}),
            value!({"state":"paused", "due_at":null, "task_id":null}),
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
    let runs = tx.database().collection(models::runs::Entity::COLLECTION)?;
    // Child cancellation remains durable while a child owns an accepted task.
    for leased in [true, false] {
        runs.execute(Operation::Update {
            filter:value!({"app_id":app.as_str(), "parent_id":id.clone(), "parent_generation":run.integer("generation")?,
                "cascade":1, "state":{"$nin":["completed","failed","cancelled"]}, "task_id":{"$exists":leased}}),
            patch:if leased { value!({"control":"cancel"}) } else { value!({"control":"cancel", "due_at":now}) },
            many:true,
        }).await?;
    }
    publish_cancelled_children(tx, app, &id, run.integer("generation")?, now).await?;
    if has_compensation(tx, app, run).await? {
        tx.database().collection(models::generations::Entity::COLLECTION)?.update(
            value!({"app_id":app.as_str(), "run_id":id.clone(), "generation":run.integer("generation")?}),
            value!({"error":update.error().map(|value|encode(&value)).transpose()?}),
        ).await?;
        runs.update(value!({"app_id":app.as_str(), "id":id}),
            value!({"state":"compensating", "compensation_target":target.as_str(), "control":"none", "due_at":now, "task_id":null})).await?;
        Ok(RunState::Compensating)
    } else {
        finish(tx, app, run, target, None, update.error(), now).await
    }
}

async fn publish_cancelled_children(
    tx: &Transaction,
    app: &AppId,
    parent: &str,
    generation: i64,
    now: i64,
) -> Result<(), WorkflowServiceError> {
    let source = tx.database().entity::<models::runs::Entity>()?.alias("r")?;
    let mut after: Option<String> = None;
    loop {
        let mut filter = vec![
            source.column(models::runs::app_id).eq(app.as_str())?,
            source.column(models::runs::parent_id).eq(Some(parent))?,
            source
                .column(models::runs::parent_generation)
                .eq(Some(generation))?,
            source.column(models::runs::cascade).eq(1i64)?,
            source.column(models::runs::control).eq("cancel")?,
            source.column(models::runs::task_id).eq(None::<String>)?,
        ];
        if let Some(after) = &after {
            filter.push(Predicate::compare(
                Operand::Path(source.column(models::runs::id).asc().path),
                CompareOp::Gt,
                Operand::Lit(Literal::Text(after.clone())),
            ));
        }
        let page = tx
            .database()
            .from(&source)
            .filter(Predicate::And(filter))
            .order_by(source.column(models::runs::id).asc())
            .select(source.row::<CancelledChild>())?
            .limit(RowLimit::default().get())?
            .all()
            .await?;
        if page.is_empty() {
            return Ok(());
        }
        for child in page {
            super::publication::advance(tx, app, &child.id, now).await?;
            after = Some(child.id);
        }
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
    let due = compensation_record(tx, app, &id, run.integer("generation")?, pending.ordinal)
        .await?
        .compensation_due_at
        .unwrap_or(now);
    if due > now {
        tx.database()
            .collection(models::runs::Entity::COLLECTION)?
            .update(
                value!({"app_id":app.as_str(), "id":id}),
                value!({"due_at":due}),
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
    let record = compensation_record(tx, app, &id, generation, ordinal).await?;
    let attempts = record
        .compensation_attempts
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
        Some(deadline(now, record.compensation_retry_ms)?)
    } else {
        None
    };
    tx.database().collection(models::steps::Entity::COLLECTION)?.update(
        value!({"app_id":app.as_str(), "run_id":id.clone(), "generation":generation, "ordinal":i64::from(ordinal)}),
        value!({"compensation_attempts":attempts, "compensation_due_at":due, "compensation_error":error.map(|value|encode(&value)).transpose()?}),
    ).await?;
    if has_compensation(tx, app, run).await? {
        if ControlIntent::parse(&run.text("control")?)? == ControlIntent::Pause {
            park(tx, app, run).await?;
            return Ok(RunState::Paused);
        }
        tx.database()
            .collection(models::runs::Entity::COLLECTION)?
            .update(
                value!({"app_id":app.as_str(), "id":id.clone()}),
                value!({"task_id":null, "due_at":due.unwrap_or(now)}),
            )
            .await?;
        return Ok(RunState::Compensating);
    }
    let rows = tx
        .database()
        .entity::<models::generations::Entity>()?
        .find::<models::GenerationOutcome>(
            models::generations::app_id
                .eq(app.as_str())?
                .and(models::generations::run_id.eq(id.as_str())?)
                .and(models::generations::generation.eq(generation)?),
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?;
    let original: Option<Value> = rows
        .first()
        .ok_or_else(|| WorkflowServiceError::Internal("workflow generation is missing".into()))?
        .error
        .as_deref()
        .map(decode)
        .transpose()?;
    let failures = compensation_failures(tx, app, &id, generation).await?;
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
    tx.database().collection(models::runs::Entity::COLLECTION)?.update(
        value!({"app_id":app.as_str(), "id":id.clone()}),
        value!({"state":state.as_str(), "control":"none", "task_id":null, "due_at":null, "key":null, "terminal_at":now}),
    ).await?;
    tx.database()
        .collection(models::generations::Entity::COLLECTION)?
        .update(
            value!({"app_id":app.as_str(), "run_id":id.clone(), "generation":generation}),
            value!({"state":state.as_str(), "output":output.map(|value|encode(&value)).transpose()?,
            "error":error.map(|value|encode(&value)).transpose()?, "terminal_at":now}),
        )
        .await?;
    for table in [
        models::waits::Entity::COLLECTION,
        models::subscriptions::Entity::COLLECTION,
    ] {
        tx.database().collection(table)?.execute(Operation::Purge {
            filter:value!({"app_id":app.as_str(), "run_id":id.clone(), "generation":generation}), many:true,
        }).await?;
    }
    wake_parents(tx, app, &id, now).await?;
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

async fn wake_parents(
    tx: &Transaction,
    app: &AppId,
    child: &str,
    now: i64,
) -> Result<(), WorkflowServiceError> {
    let db = tx.database();
    let run = db.entity::<models::runs::Entity>()?.alias("r")?;
    let wait = db.entity::<models::waits::Entity>()?.alias("w")?;
    let runs = db.collection(models::runs::Entity::COLLECTION)?;
    let page_limit = RowLimit::default().get();
    let mut after: Option<String> = None;
    loop {
        let mut filter = vec![
            wait.column(models::waits::app_id).eq(app.as_str())?,
            wait.column(models::waits::child_id).eq(Some(child))?,
            run.column(models::runs::task_id).eq(None::<String>)?,
            run.column(models::runs::control).eq("none")?,
            Predicate::Or(vec![
                run.column(models::runs::state).eq("waiting")?,
                run.column(models::runs::state).eq("sleeping")?,
                run.column(models::runs::state).eq("queued")?,
            ]),
        ];
        if let Some(after) = &after {
            filter.push(Predicate::compare(
                Operand::Path(wait.column(models::waits::id).asc().path),
                CompareOp::Gt,
                Operand::Lit(Literal::Text(after.clone())),
            ));
        }
        let page = db
            .from(&wait)
            .inner_join(
                &run,
                Predicate::And(vec![
                    wait.column(models::waits::app_id)
                        .eq_column(run.column(models::runs::app_id))?,
                    wait.column(models::waits::run_id)
                        .eq_column(run.column(models::runs::id))?,
                    wait.column(models::waits::generation)
                        .eq_column(run.column(models::runs::generation))?,
                ]),
            )?
            .filter(Predicate::And(filter))
            .order_by(wait.column(models::waits::id).asc())
            .select(wait.row::<WaitingParent>())?
            .limit(page_limit)?
            .all()
            .await?;
        let count = page.len();
        for parent in page {
            runs.execute(Operation::Update {
                filter: value!({"app_id":app.as_str(), "id":parent.run_id.clone(), "generation":parent.generation,
                    "task_id":null, "control":"none", "state":{"$in":["waiting","sleeping","queued"]}}),
                patch: value!({"due_at":now}),
                many: true,
            }).await?;
            super::publication::advance(tx, app, &parent.run_id, now).await?;
            after = Some(parent.id);
        }
        if count < page_limit as usize {
            break;
        }
    }
    Ok(())
}

async fn link_continuation(
    tx: &mut Transaction,
    app: &AppId,
    run: &Row,
    successor: &str,
) -> Result<(), WorkflowServiceError> {
    let id = run.text("id")?;
    let runs = tx.database().collection(models::runs::Entity::COLLECTION)?;
    runs.update(
        value!({"app_id":app.as_str(), "id":id.clone()}),
        value!({"continued_to_id":successor}),
    )
    .await?;
    runs.update(value!({"app_id":app.as_str(), "id":successor}), value!({
        "continued_from_id":id.clone(), "parent_id":run.optional_text("parent_id")?,
        "parent_generation":run.optional_integer("parent_generation")?, "parent_ordinal":run.optional_integer("parent_ordinal")?,
        "cascade":run.integer("cascade")?, "depth":run.integer("depth")?, "schedule_id":run.optional_text("schedule_id")?,
    })).await?;
    // A parent's durable wait follows the continuation. It must not observe the
    // intermediate run's terminal acknowledgement as the child's final result.
    retarget_parent_steps(tx, app, &id, successor).await?;
    tx.database()
        .collection(models::waits::Entity::COLLECTION)?
        .execute(Operation::Update {
            filter: value!({"app_id":app.as_str(), "child_id":id}),
            patch: value!({"child_id":successor}),
            many: true,
        })
        .await?;
    Ok(())
}

async fn compensation_record(
    tx: &Transaction,
    app: &AppId,
    id: &str,
    generation: i64,
    ordinal: i32,
) -> Result<models::CompensationRecord, WorkflowServiceError> {
    tx.database()
        .entity::<models::steps::Entity>()?
        .find::<models::CompensationRecord>(
            models::steps::app_id
                .eq(app.as_str())?
                .and(models::steps::run_id.eq(id)?)
                .and(models::steps::generation.eq(generation)?)
                .and(models::steps::ordinal.eq(i64::from(ordinal))?),
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| {
            WorkflowServiceError::Internal("workflow compensation record is missing".into())
        })
}

async fn compensation_failures(
    tx: &Transaction,
    app: &AppId,
    id: &str,
    generation: i64,
) -> Result<Vec<Value>, WorkflowServiceError> {
    let db = tx.database();
    let source = db.entity::<models::steps::Entity>()?.alias("s")?;
    let page_limit = RowLimit::default().get();
    let mut after = None;
    let mut failures = Vec::new();
    loop {
        let mut filter = vec![
            source.column(models::steps::app_id).eq(app.as_str())?,
            source.column(models::steps::run_id).eq(id)?,
            source.column(models::steps::generation).eq(generation)?,
            Predicate::Not(Box::new(
                source
                    .column(models::steps::compensation_error)
                    .eq(None::<String>)?,
            )),
        ];
        if let Some(after) = after {
            filter.push(Predicate::compare(
                Operand::Path(source.column(models::steps::ordinal).asc().path),
                CompareOp::Lt,
                Operand::Lit(Literal::Int(after)),
            ));
        }
        let page = db
            .from(&source)
            .filter(Predicate::And(filter))
            .order_by(source.column(models::steps::ordinal).desc())
            .select(source.row::<models::CompensationFailure>())?
            .limit(page_limit)?
            .all()
            .await?;
        let count = page.len();
        for row in page {
            after = Some(row.ordinal);
            let error = row.compensation_error.as_deref().ok_or_else(|| {
                WorkflowServiceError::Internal("workflow compensation failure has no error".into())
            })?;
            failures.push(json!({"ordinal":row.ordinal, "error":decode::<Value>(error)?}));
        }
        if count < page_limit as usize {
            break;
        }
    }
    Ok(failures)
}

async fn retarget_parent_steps(
    tx: &mut Transaction,
    app: &AppId,
    id: &str,
    successor: &str,
) -> Result<(), WorkflowServiceError> {
    let db = tx.database().clone();
    let step = db.entity::<models::steps::Entity>()?.alias("s")?;
    let wait = db.entity::<models::waits::Entity>()?.alias("w")?;
    let page_limit = RowLimit::default().get();
    let mut after: Option<(String, i64, i64)> = None;
    loop {
        let mut filter = vec![
            wait.column(models::waits::app_id).eq(app.as_str())?,
            wait.column(models::waits::child_id).eq(Some(id))?,
        ];
        if let Some((run_id, generation, ordinal)) = &after {
            filter.push(Predicate::Or(vec![
                Predicate::compare(
                    Operand::Path(step.column(models::steps::run_id).asc().path),
                    CompareOp::Gt,
                    Operand::Lit(Literal::Text(run_id.clone())),
                ),
                Predicate::And(vec![
                    step.column(models::steps::run_id).eq(run_id.as_str())?,
                    Predicate::compare(
                        Operand::Path(step.column(models::steps::generation).asc().path),
                        CompareOp::Gt,
                        Operand::Lit(Literal::Int(*generation)),
                    ),
                ]),
                Predicate::And(vec![
                    step.column(models::steps::run_id).eq(run_id.as_str())?,
                    step.column(models::steps::generation).eq(*generation)?,
                    Predicate::compare(
                        Operand::Path(step.column(models::steps::ordinal).asc().path),
                        CompareOp::Gt,
                        Operand::Lit(Literal::Int(*ordinal)),
                    ),
                ]),
            ]));
        }
        let page = db
            .from(&step)
            .inner_join(
                &wait,
                Predicate::And(vec![
                    step.column(models::steps::app_id)
                        .eq_column(wait.column(models::waits::app_id))?,
                    step.column(models::steps::run_id)
                        .eq_column(wait.column(models::waits::run_id))?,
                    step.column(models::steps::generation)
                        .eq_column(wait.column(models::waits::generation))?,
                    step.column(models::steps::ordinal)
                        .eq_column(wait.column(models::waits::ordinal))?,
                ]),
            )?
            .filter(Predicate::And(filter))
            .order_by(step.column(models::steps::run_id).asc())
            .order_by(step.column(models::steps::generation).asc())
            .order_by(step.column(models::steps::ordinal).asc())
            .select(step.row::<models::ParentStep>())?
            .limit(page_limit)?
            .all()
            .await?;
        let count = page.len();
        for parent in page {
            let mut record: crate::engine::StepCheckpoint = decode(&parent.record)?;
            record.child_run_id = Some(successor.into());
            journal::update(tx, app, &parent.run_id, parent.generation, &record).await?;
            after = Some((parent.run_id, parent.generation, parent.ordinal));
        }
        if count < page_limit as usize {
            break;
        }
    }
    Ok(())
}
