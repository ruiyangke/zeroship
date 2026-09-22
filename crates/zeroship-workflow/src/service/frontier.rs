use super::{
    app::{
        active_deploy, deadline, decode, emit, encode, insert_continued_run, parse_state, NewRun,
    },
    continuations, journal, models,
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
    orm::{Entity, FindOptions, Operation},
    sql::RowLimit,
    value,
};

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
            generation
                .column(models::generations::app_id)
                .eq(deployment.column(models::deploys::app_id))?
                .and(
                    generation
                        .column(models::generations::deploy_id)
                        .eq(deployment.column(models::deploys::id))?,
                ),
        )?
        .filter(
            generation
                .column(models::generations::app_id)
                .eq(app.as_str())?
                .and(
                    generation
                        .column(models::generations::run_id)
                        .eq(id.as_str())?,
                )
                .and(
                    generation
                        .column(models::generations::generation)
                        .eq(run.integer("generation")?)?,
                )
                .and(deployment.column(models::deploys::state).eq("available")?),
        )
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
        generation: run.integer("generation")?,
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

/// Dispatches reclaimed without the executor reporting an outcome, counted
/// against the journal state they died on.
///
/// Two revisions identify that state, because each covers writes the other does
/// not. The frontier revision moves when a committed creator transition opens or
/// settles work, which is what leaves the strikes behind once a dispatch reports
/// anything durable. The journal revision moves when a step a replay has already
/// been handed is rewritten in place - a durable wait settled against the
/// database clock is the case nothing publishes, so no frontier transition
/// covers it. Counting on the frontier revision alone keeps a dead dispatch's
/// strike alive across a wait that genuinely came due, and the run stalls on a
/// frontier it is no longer stuck at; counting on the journal revision alone
/// keeps it alive across a batch that only opened new steps.
async fn stuck_dispatches(
    tx: &mut Transaction,
    app: &AppId,
    run: &Row,
) -> Result<i64, WorkflowServiceError> {
    let counted = tx
        .database()
        .collection(models::tasks::Entity::COLLECTION)?
        .execute(Operation::Count {
            filter: value!({"app_id":app.as_str(), "run_id":run.text("id")?,
                "generation":run.integer("generation")?,
                "frontier_revision":run.integer("frontier_revision")?,
                "journal_revision":run.integer("journal_revision")?, "state":"expired"}),
            options: value!({}),
        })
        .await?;
    match counted {
        zeroship_data_orm::orm::Output::Count(count) => Ok(count),
        _ => Err(WorkflowServiceError::Internal(
            "workflow task count returned rows".into(),
        )),
    }
}

/// Bring a run to rest at `stalled` without rolling anything back.
///
/// The verdict is the host's, not the body's: the platform gave up on work it
/// could not get an outcome for, so there is no creator failure to compensate
/// and no reason to hand a compensator to the same dispatch path that already
/// failed to report. Compensation stays reserved for outcomes a run reported.
async fn stall(
    tx: &mut Transaction,
    app: &AppId,
    run: &Row,
    error: Value,
    now: i64,
) -> Result<RunState, WorkflowServiceError> {
    let update = RunUpdate::Stalled { error };
    let state = parse_state(update.state())?;
    finish(tx, app, run, state, None, update.error(), now).await
}

/// Bring a run whose rollback stopped reporting to rest at the failure it was
/// rolling back, with the undischarged obligations named.
///
/// A run reaches `compensating` because it already failed, and that verdict is
/// the creator's: it is what their code produced and what they query for. A
/// rollback the host gave up on does not overturn it, so the run rests at the
/// state an incomplete rollback always reaches, and the host's liveness verdict
/// is reported inside the compensation summary instead of replacing it.
///
/// The obligations are marked abandoned rather than left pending, because a run
/// at rest whose journal still claims a pending compensator would read as work
/// the platform intends to do. What the creator cannot learn is whether an
/// abandoned compensator applied part of its effect before it stopped
/// reporting; naming the step is what lets them go and look.
async fn abandon(
    tx: &mut Transaction,
    app: &AppId,
    run: &Row,
    reason: Value,
    now: i64,
) -> Result<RunState, WorkflowServiceError> {
    let id = run.text("id")?;
    let generation = run.integer("generation")?;
    let mut steps = Vec::new();
    for mut step in journal::load(tx, app, &id, generation).await?.into_iter().rev() {
        if !step
            .compensation_state
            .as_deref()
            .is_some_and(|state| matches!(state, "pending" | "running"))
        {
            continue;
        }
        steps.push(json!({"ordinal":step.ordinal, "name":step.name}));
        step.compensation_state = Some("abandoned".into());
        journal::update(tx, app, &id, generation, &step).await?;
    }
    if steps.is_empty() {
        return Err(WorkflowServiceError::Internal(
            "compensating workflow has no pending operation".into(),
        ));
    }
    let original = original_error(tx, app, &id, generation).await?;
    let failures = compensation_failures(tx, app, &id, generation).await?;
    let error = compensated_error(
        original,
        &journal::load(tx, app, &id, generation).await?,
        failures,
        Some(Abandonment { steps, reason }),
    );
    finish(tx, app, run, RunState::Failed, None, Some(error), now).await
}

/// Reconcile control intent and durable waits before assigning an executor.
/// A parent's still-propagating cascade counts as recorded cancellation.
///
/// A run whose frontier keeps being dispatched and reclaimed with nothing
/// reported comes to rest here rather than being handed another executor.
/// This is the creator-facing half of the manager's delivery ceiling: the
/// manager stops redelivering a job that never finishes, and a job whose run is
/// terminal settles, so the verdict has to land before that ceiling is spent.
/// A rollback that stops reporting is the same failure one phase later and the
/// same strikes bound it, but its resting state is the failure it was rolling
/// back, not a stall. See [`abandon`].
pub(crate) async fn prepare(
    tx: &mut Transaction,
    app: &AppId,
    run: &Row,
    policy: &AppPolicy,
    now: i64,
) -> Result<bool, WorkflowServiceError> {
    let intent = super::propagation::effective_control(tx, app, run).await?;
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
    let compensating = run.text("state")? == "compensating";
    let stuck = stuck_dispatches(tx, app, run).await?;
    if stuck >= policy.max_stuck_dispatches {
        let reason = crate::engine::stalled_error(stuck, policy.max_stuck_dispatches);
        if compensating {
            abandon(tx, app, run, reason, now).await?;
        } else {
            stall(tx, app, run, reason, now).await?;
        }
        return Ok(false);
    }
    if compensating {
        return compensation_ready(tx, app, run, now).await;
    }
    let progressed = journal::resolve(tx, app, run, now).await?;
    let steps = journal::load(tx, app, &run.text("id")?, run.integer("generation")?).await?;
    let pending = steps.iter().any(|step| step.state == "running");
    // A step whose next attempt is due is forward work, so the run is dispatched
    // even though an unresolved wait sits beside it. Without this the two
    // together would hold each other: the wait is what makes the run look idle,
    // and the attempt is the only thing that can move it.
    let attempt_due = steps.iter().any(|step| {
        step.state == "retrying"
            && step
                .wake_at
                .is_none_or(|at| at.timestamp_millis() <= now)
    });
    if pending && !progressed && !attempt_due {
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
    // Both ceilings bound executions of creator code at one ordinal, so both are
    // checked against the one budget the app was granted. A declaration outside
    // it is refused rather than clamped: a creator who asked for more attempts
    // than the app allows has to learn that, not silently get fewer.
    for outcome in &execution.outcomes {
        let declared = match outcome {
            StepOutcome::StepCompleted {
                compensation_max_attempts,
                ..
            } => *compensation_max_attempts,
            StepOutcome::StepFailed { max_attempts, .. }
            | StepOutcome::RunFailed {
                ordinal: Some(_),
                max_attempts,
                ..
            } => *max_attempts,
            _ => continue,
        };
        if declared <= 0 || declared > policy.max_step_attempts {
            return journal::invalid("workflow step retry policy exceeds the app limit");
        }
    }
    let (checkpoints, update) =
        fold_outcomes(&execution.outcomes).map_err(WorkflowServiceError::InvalidRequest)?;
    journal::append(tx, app, run, policy, checkpoints, now).await?;
    // Checked before continuation so a fenced child cannot continue as new.
    let intent = super::propagation::effective_control(tx, app, run).await?;
    if intent == ControlIntent::Cancel {
        return settle(tx, app, run, RunUpdate::Cancelled, now).await;
    }
    if let RunUpdate::Stalled { error } = update {
        return stall(tx, app, run, error, now).await;
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
            .any(|step| matches!(step.state.as_str(), "running" | "retrying"))
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
        if steps
            .iter()
            .any(|step| matches!(step.state.as_str(), "running" | "retrying"))
        {
            return journal::invalid("workflow cannot continue with unresolved operations");
        }
        // Owing a compensator is the creator's own reachable state, not a
        // malformed report, so it settles the generation under a name the
        // creator can read off the run instead of refusing the completion.
        // Refusing leaves the body producing the same transition on every
        // redelivery until the liveness ceiling reports a stall, which names
        // the wrong cause. Settling also discharges the obligations that
        // blocked the continuation rather than stranding them.
        if steps.iter().any(|step| {
            step.compensation_state
                .as_deref()
                .is_some_and(|state| matches!(state, "pending" | "running"))
        }) {
            return settle(
                tx,
                app,
                run,
                RunUpdate::Failed {
                    error: crate::engine::compensable_carry_error(),
                },
                now,
            )
            .await;
        }
        admit(policy)?;
        let deploy = active_deploy(tx, app).await?;
        let name = run.text("workflow_name")?;
        if !deploy.workflows.contains(&name) {
            return journal::invalid("workflow is absent from the active deployment");
        }
        let source =
            continuations::member(tx, app, &run.text("id")?, run.integer("generation")?).await?;
        let id = typed_id::new_workflow_run_id();
        let key = run.optional_text("key")?;
        let completed = Terminal {
            state: RunState::Completed,
            output: Some(json!({"continuedAsNew":id})),
            error: None,
        };
        finish_run(tx, app, run, completed, now, false).await?;
        let options = StartOptions {
            input: seed_input.unwrap_or(Value::Null),
            key,
            ..Default::default()
        };
        let successor = NewRun {
            id: &id,
            name: &name,
            deploy: &deploy.id,
            options: &options,
        };
        insert_continued_run(tx, app, &successor, now, &source).await?;
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
    // A step with attempts left is forward work the run owes itself, not a wait
    // on anything external, so the run queues rather than sleeping. Its deadline
    // is the run's, because `due_at` is the only clock a reclaimed run keeps:
    // the isolate that failed the attempt is gone long before the next one.
    let retry = steps
        .iter()
        .filter(|step| step.state == "retrying")
        .filter_map(|step| step.wake_at.map(|time| time.timestamp_millis()))
        .min();
    let state = if retry.is_some() || pending.is_empty() || progressed {
        RunState::Queued
    } else if pending.iter().all(|step| step.kind == "sleep") {
        RunState::Sleeping
    } else {
        RunState::Waiting
    };
    let due = if let Some(retry) = retry {
        Some(retry.max(now))
    } else if state == RunState::Queued {
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
    // Delivered pages reach the cascading children; until they finish, the
    // recorded obligation fences every child of this generation.
    super::propagation::cascade(tx, app, &id, run.integer("generation")?, now).await?;
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
        if super::propagation::effective_control(tx, app, run).await? == ControlIntent::Pause {
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
    let original = original_error(tx, app, &id, generation).await?;
    let failures = compensation_failures(tx, app, &id, generation).await?;
    let steps = journal::load(tx, app, &id, generation).await?;
    let state = if failures.is_empty() {
        parse_state(&run.text("compensation_target")?)?
    } else {
        RunState::Failed
    };
    let error = compensated_error(original, &steps, failures, None);
    finish(tx, app, run, state, None, Some(error), now).await
}

/// The failure that started this generation's rollback, as `settle` recorded it.
async fn original_error(
    tx: &Transaction,
    app: &AppId,
    id: &str,
    generation: i64,
) -> Result<Option<Value>, WorkflowServiceError> {
    tx.database()
        .entity::<models::generations::Entity>()?
        .find::<models::GenerationOutcome>(
            models::generations::app_id
                .eq(app.as_str())?
                .and(models::generations::run_id.eq(id)?)
                .and(models::generations::generation.eq(generation)?),
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?
        .first()
        .ok_or_else(|| WorkflowServiceError::Internal("workflow generation is missing".into()))?
        .error
        .as_deref()
        .map(decode)
        .transpose()
}

/// What the host gave up on when it abandoned a rollback: the obligations it
/// never discharged, newest first, and the liveness verdict that stopped it.
struct Abandonment {
    steps: Vec<Value>,
    reason: Value,
}

/// Attach the rollback summary to the failure that started compensation.
///
/// The original error keeps its type and message; `compensation` reports how
/// many compensators ran to a final result, and `partial` outcomes list each
/// failed step. An `abandoned` outcome additionally names the obligations no
/// compensator ever reported on, and the reason the host stopped waiting. Those
/// steps reached no final result, so they are outside the counts rather than
/// inflating them: a compensator that reported a failure and one that reported
/// nothing are different facts and a creator has to be able to tell them apart.
fn compensated_error(
    original: Option<Value>,
    steps: &[crate::engine::StepCheckpoint],
    failures: Vec<Value>,
    abandonment: Option<Abandonment>,
) -> Value {
    let count = |state: &str| {
        steps
            .iter()
            .filter(|step| step.compensation_state.as_deref() == Some(state))
            .count()
    };
    let (completed, failed) = (count("completed"), count("failed"));
    let outcome = if abandonment.is_some() {
        "abandoned"
    } else if failures.is_empty() {
        "completed"
    } else {
        "partial"
    };
    let mut summary = json!({
        "total": completed + failed,
        "completed": completed,
        "failed": failed,
        "outcome": outcome,
    });
    if !failures.is_empty() {
        summary["failures"] = Value::Array(failures);
    }
    if let Some(Abandonment { steps, reason }) = abandonment {
        summary["abandoned"] = Value::Array(steps);
        summary["reason"] = reason;
    }
    let mut error = match original {
        Some(Value::Object(error)) => Value::Object(error),
        Some(cause) => json!({
            "type": "Error", "message": "workflow failed during compensation", "cause": cause,
        }),
        None => json!({"type": "Error", "message": "workflow compensation finished"}),
    };
    error["compensation"] = summary;
    error
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
    let terminal = Terminal {
        state,
        output,
        error,
    };
    finish_run(tx, app, run, terminal, now, true).await
}

/// Result recorded on a run's current generation when it becomes terminal.
struct Terminal {
    state: RunState,
    output: Option<Value>,
    error: Option<Value>,
}

async fn finish_run(
    tx: &mut Transaction,
    app: &AppId,
    run: &Row,
    terminal: Terminal,
    now: i64,
    notify_parents: bool,
) -> Result<RunState, WorkflowServiceError> {
    let Terminal {
        state,
        output,
        error,
    } = terminal;
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
    if notify_parents {
        let member = continuations::member(tx, app, &id, generation).await?;
        if member.is_current {
            super::propagation::notify(tx, app, &member, now).await?;
        }
    }
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
    tx: &Transaction,
    app: &AppId,
    run: &Row,
    successor: &str,
) -> Result<(), WorkflowServiceError> {
    let changed = tx
        .database()
        .entity::<models::runs::Entity>()?
        .update_many(
            models::runs::app_id
                .eq(app.as_str())?
                .and(models::runs::id.eq(successor)?),
            models::runs::parent_id
                .set(run.optional_text("parent_id")?)?
                .and(
                    models::runs::parent_generation
                        .set(run.optional_integer("parent_generation")?)?,
                )?
                .and(models::runs::parent_ordinal.set(run.optional_integer("parent_ordinal")?)?)?
                .and(models::runs::cascade.set(run.integer("cascade")?)?)?
                .and(models::runs::depth.set(run.integer("depth")?)?)?
                .and(models::runs::schedule_id.set(run.optional_text("schedule_id")?)?)?,
        )
        .await?;
    if changed != 1 {
        return Err(WorkflowServiceError::Internal(
            "workflow continuation successor is missing".into(),
        ));
    }
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
        let mut filter = source
            .column(models::steps::app_id)
            .eq(app.as_str())?
            .and(source.column(models::steps::run_id).eq(id)?)
            .and(source.column(models::steps::generation).eq(generation)?)
            .and(
                source
                    .column(models::steps::compensation_error)
                    .is_not_null(),
            );
        if let Some(after) = after {
            filter = filter.and(source.column(models::steps::ordinal).lt(after)?);
        }
        let page = db
            .from(&source)
            .filter(filter)
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
