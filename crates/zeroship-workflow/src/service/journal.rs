use super::{
    app::{current_run, decode, encode, insert_root_run, keyed_run, live_runs, parse_state},
    models,
    store::{Row, Transaction},
    AppPolicy,
};
use crate::{
    engine::{JournalStep, StepCheckpoint},
    operations::{ConflictPolicy, StartOptions},
    validation, WorkflowServiceError,
};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use zeroship_core::{app_id::AppId, typed_id};
use zeroship_data_orm::{
    orm::{Entity, FindOptions, FromRow, Operation, Output},
    sql::RowLimit,
    value,
};

const MAX_DEPENDENCY_INSPECTIONS: usize = 16_384;

#[derive(FromRow)]
#[orm(entity = models::runs)]
struct RunParent {
    parent_id: Option<String>,
}

#[derive(FromRow)]
#[orm(entity = models::waits)]
struct ChildDependency {
    id: String,
    child_id: Option<String>,
}

pub(crate) async fn load(
    tx: &mut Transaction,
    app: &AppId,
    id: &str,
    generation: i64,
) -> Result<Vec<StepCheckpoint>, WorkflowServiceError> {
    let db = tx.database();
    let source = db.entity::<models::steps::Entity>()?.alias("s")?;
    let mut journal = Vec::new();
    let mut after = None;
    let page_limit = RowLimit::default().get();
    loop {
        let mut predicates = source
            .column(models::steps::app_id)
            .eq(app.as_str())?
            .and(source.column(models::steps::run_id).eq(id)?)
            .and(source.column(models::steps::generation).eq(generation)?);
        if let Some(after) = after {
            predicates = predicates.and(source.column(models::steps::ordinal).gt(after)?);
        }
        let page = db
            .from(&source)
            .filter(predicates)
            .order_by(source.column(models::steps::ordinal).asc())
            .select(source.row::<models::StoredStep>())?
            .limit(page_limit)?
            .all()
            .await?;
        let count = page.len();
        for row in page {
            after = Some(row.ordinal);
            journal.push(decode(&row.record)?);
        }
        if count < page_limit as usize {
            break;
        }
    }
    Ok(journal)
}
pub(crate) fn replay(steps: &[StepCheckpoint]) -> Vec<JournalStep> {
    steps
        .iter()
        .map(|step| JournalStep {
            ordinal: step.ordinal,
            name: step.name.clone(),
            name_occurrence: step.name_occurrence,
            kind: step.kind.clone(),
            state: step.state.clone(),
            output: step.output.clone(),
            output_ref: step.output_ref.clone(),
            error: step.error.clone(),
            child_run_id: step.child_run_id.clone(),
            compensation_state: step.compensation_state.clone(),
        })
        .collect()
}
pub(crate) async fn update(
    tx: &mut Transaction,
    app: &AppId,
    id: &str,
    generation: i64,
    step: &StepCheckpoint,
) -> Result<(), WorkflowServiceError> {
    tx.database().collection(models::steps::Entity::COLLECTION)?
        .update(value!({"app_id":app.as_str(), "run_id":id, "generation":generation, "ordinal":i64::from(step.ordinal)}),
            value!({"state":step.state.clone(), "record":encode(step)?})).await?;
    Ok(())
}

pub(crate) async fn append(
    tx: &mut Transaction,
    app: &AppId,
    run: &Row,
    policy: &AppPolicy,
    checkpoints: Vec<StepCheckpoint>,
    now: i64,
) -> Result<(), WorkflowServiceError> {
    let id = run.text("id")?;
    let generation = run.integer("generation")?;
    let mut journal = load(tx, app, &id, generation).await?;
    let mut journal_bytes = journal.iter().try_fold(0usize, |total, step| {
        total.checked_add(encode(step)?.len()).ok_or_else(|| {
            WorkflowServiceError::ResourceExhausted("workflow journal size overflow".into())
        })
    })?;
    let steps = tx
        .database()
        .collection(models::steps::Entity::COLLECTION)?;
    for mut step in checkpoints {
        if step.ordinal < 0
            || step.name.is_empty()
            || step.name.len() > validation::STEP_NAME_MAX_BYTES
            || step.name_occurrence < 0
        {
            return invalid("invalid workflow checkpoint identity");
        }
        if let Some(existing) = journal.iter().find(|entry| entry.ordinal == step.ordinal) {
            // Pending effects can be re-reported by a replay suspended on a
            // previously accepted frontier. Their original deadlines and child
            // identities remain authoritative.
            if existing.state != "running"
                || existing.name != step.name
                || existing.name_occurrence != step.name_occurrence
                || existing.kind != step.kind
                || existing.signal_type != step.signal_type
                || existing.topic != step.topic
            {
                return invalid("workflow checkpoint rewrites committed history");
            }
            continue;
        }
        if step.ordinal as usize != journal.len()
            || step.name_occurrence as usize
                != journal
                    .iter()
                    .filter(|entry| entry.name == step.name)
                    .count()
        {
            return invalid("workflow checkpoint is not the next journal operation");
        }
        if step.consumed_signal_id.is_some() {
            return invalid("signal consumption belongs to the workflow service");
        }
        if let Some(reference) = &step.output_ref {
            if step.state != "completed" || step.kind != "run" || step.output.is_some() {
                return invalid(
                    "payload references require a completed operation without inline output",
                );
            }
            super::payloads::promote(
                tx,
                app,
                run,
                super::payloads::RunGeneration {
                    id: &id,
                    generation,
                },
                super::PayloadSlot::Step {
                    ordinal: step.ordinal,
                },
                reference,
                now,
            )
            .await?;
        }
        if encode(&step)?.len() > policy.max_input_bytes {
            return Err(WorkflowServiceError::PayloadTooLarge);
        }
        if step.max_signal_age_ms.is_some_and(|age| age < 0) {
            return invalid("signal maximum age cannot be negative");
        }
        if let Some(kind) = &step.signal_type {
            if step.kind != "child" {
                validation::signal_type(kind)?;
            }
        }
        if step.kind == "child" {
            step.child_run_id = Some(child(tx, app, run, policy, &step, now).await?);
        }
        journal_bytes = journal_bytes
            .checked_add(encode(&step)?.len())
            .ok_or_else(|| {
                WorkflowServiceError::ResourceExhausted("workflow journal size overflow".into())
            })?;
        if journal_bytes > policy.max_journal_bytes {
            return Err(WorkflowServiceError::ResourceExhausted(
                "workflow journal size limit reached".into(),
            ));
        }
        steps.insert(value!({
            "id":super::types::storage_id(), "app_id":app.as_str(), "run_id":id.clone(), "generation":generation,
            "ordinal":i64::from(step.ordinal), "name":step.name.clone(), "occurrence":i64::from(step.name_occurrence),
            "origin_generation":generation, "kind":step.kind.clone(), "state":step.state.clone(),
            "record":encode(&step)?, "compensation_retry_ms":policy.compensation_retry_ms,
        })).await?;
        if step.state == "running" {
            tx.database().collection(models::waits::Entity::COLLECTION)?
                .insert(value!({
                    "id":super::types::storage_id(), "app_id":app.as_str(), "run_id":id.clone(), "generation":generation,
                    "ordinal":i64::from(step.ordinal), "kind":step.kind.clone(), "signal_type":step.signal_type.clone(),
                    "topic":step.topic.clone(), "max_signal_age":step.max_signal_age_ms,
                    "due_at":step.wake_at.map(|time|time.timestamp_millis()), "child_id":step.child_run_id.clone(),
                })).await?;
            super::signals::subscribe(tx, app, &id, generation, &step, now).await?;
        }
        journal.push(step);
    }
    Ok(())
}

async fn child(
    tx: &mut Transaction,
    app: &AppId,
    parent: &Row,
    policy: &AppPolicy,
    step: &StepCheckpoint,
    now: i64,
) -> Result<String, WorkflowServiceError> {
    let name = step.child_workflow_name.as_deref().ok_or_else(|| {
        WorkflowServiceError::InvalidRequest("child workflow name is missing".into())
    })?;
    validation::workflow_name(name)?;
    let deploy_id = parent.text("deploy_id")?;
    let rows = tx
        .database()
        .entity::<models::deploys::Entity>()?
        .find::<models::DeploymentManifest>(
            models::deploys::app_id
                .eq(app.as_str())?
                .and(models::deploys::id.eq(deploy_id.as_str())?)
                .and(models::deploys::state.eq("available")?),
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?;
    let deploy: super::DeployRegistration = decode(
        &rows
            .first()
            .ok_or_else(|| {
                WorkflowServiceError::Unavailable("pinned child deployment is unavailable".into())
            })?
            .manifest,
    )?;
    if !deploy.workflows.contains(name) {
        return invalid("child workflow is absent from the pinned deployment");
    }
    let options = step.child_options.clone().unwrap_or_default();
    if let Some(key) = &options.key {
        if let Some(row) = keyed_run(tx, app, name, key).await? {
            let id = row.id;
            validate_child_dependency(tx, app, &parent.text("id")?, &id).await?;
            return Ok(id);
        }
    }
    if parent.integer("depth")? >= policy.max_child_depth {
        return Err(WorkflowServiceError::ResourceExhausted(
            "workflow child depth limit reached".into(),
        ));
    }
    if live_runs(tx, app).await? >= policy.max_live_runs {
        return Err(WorkflowServiceError::ResourceExhausted(
            "workflow live-run limit reached".into(),
        ));
    }
    let id = typed_id::new_workflow_run_id();
    let start = StartOptions {
        input: step.child_input.clone().unwrap_or(Value::Null),
        key: options.key,
        on_conflict: ConflictPolicy::Join,
    };
    validation::start(&start)?;
    insert_root_run(tx, app, &id, name, &deploy_id, &start, now).await?;
    tx.database()
        .collection(models::runs::Entity::COLLECTION)?
        .update(
            value!({"app_id":app.as_str(), "id":id.clone()}),
            value!({
                "parent_id":parent.text("id")?, "parent_generation":parent.integer("generation")?,
                "parent_ordinal":i64::from(step.ordinal), "cascade":i64::from(options.cascade),
                "depth":parent.integer("depth")? + 1,
            }),
        )
        .await?;
    Ok(id)
}

/// The caller holds the app lock across validation and insertion of its wait.
/// Inspect current generations only and refuse an unprovable graph on budget
/// exhaustion rather than accepting a potentially cyclic child dependency.
async fn validate_child_dependency(
    tx: &Transaction,
    app: &AppId,
    parent: &str,
    child: &str,
) -> Result<(), WorkflowServiceError> {
    let db = tx.database();
    let runs = db.entity::<models::runs::Entity>()?;
    let mut remaining = MAX_DEPENDENCY_INSPECTIONS;
    let mut ancestors = BTreeSet::new();
    let mut ancestor = Some(parent.to_owned());
    while let Some(id) = ancestor {
        inspect_dependency(&mut remaining)?;
        if id == child {
            return invalid("child workflow would wait on its ancestor");
        }
        if !ancestors.insert(id.clone()) {
            return Err(WorkflowServiceError::Internal(
                "workflow ancestry contains a cycle".into(),
            ));
        }
        ancestor = runs
            .find::<RunParent>(
                models::runs::app_id
                    .eq(app.as_str())?
                    .and(models::runs::id.eq(id.as_str())?),
                FindOptions {
                    limit: Some(1),
                    ..Default::default()
                },
            )
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| WorkflowServiceError::Internal("workflow ancestor is missing".into()))?
            .parent_id;
    }
    let run = runs.alias("r")?;
    let wait = db.entity::<models::waits::Entity>()?.alias("w")?;
    let mut pending = vec![(child.to_owned(), false)];
    let mut active = BTreeSet::new();
    let mut completed = BTreeSet::new();
    let page_limit = RowLimit::default().get();
    while let Some((id, exiting)) = pending.pop() {
        if exiting {
            active.remove(&id);
            completed.insert(id);
            continue;
        }
        if id == parent {
            return invalid("child workflow would create a dependency cycle");
        }
        if active.contains(&id) {
            return Err(WorkflowServiceError::Internal(
                "workflow dependencies contain a cycle".into(),
            ));
        }
        if completed.contains(&id) {
            continue;
        }
        inspect_dependency(&mut remaining)?;
        active.insert(id.clone());
        pending.push((id.clone(), true));
        let mut after: Option<String> = None;
        loop {
            let mut filter = wait
                .column(models::waits::app_id)
                .eq(app.as_str())?
                .and(wait.column(models::waits::run_id).eq(id.as_str())?)
                .and(wait.column(models::waits::child_id).is_not_null());
            if let Some(after) = &after {
                filter = filter.and(wait.column(models::waits::id).gt(after.as_str())?);
            }
            let page = db
                .from(&wait)
                .inner_join(
                    &run,
                    wait.column(models::waits::app_id)
                        .eq(run.column(models::runs::app_id))?
                        .and(
                            wait.column(models::waits::run_id)
                                .eq(run.column(models::runs::id))?,
                        )
                        .and(
                            wait.column(models::waits::generation)
                                .eq(run.column(models::runs::generation))?,
                        ),
                )?
                .filter(filter)
                .order_by(wait.column(models::waits::id).asc())
                .select(wait.row::<ChildDependency>())?
                .limit(page_limit)?
                .all()
                .await?;
            let count = page.len();
            for edge in page {
                inspect_dependency(&mut remaining)?;
                after = Some(edge.id);
                let target = edge.child_id.ok_or_else(|| {
                    WorkflowServiceError::Internal("workflow child dependency is missing".into())
                })?;
                pending.push((target, false));
            }
            if count < page_limit as usize {
                break;
            }
        }
    }
    Ok(())
}

fn inspect_dependency(remaining: &mut usize) -> Result<(), WorkflowServiceError> {
    *remaining = remaining.checked_sub(1).ok_or_else(|| {
        WorkflowServiceError::ResourceExhausted(
            "workflow dependency inspection limit reached".into(),
        )
    })?;
    Ok(())
}

/// Resolve durable waits using the service clock and app-scoped mailboxes.
/// Returns whether a suspended frontier has made progress.
pub(crate) async fn resolve(
    tx: &mut Transaction,
    app: &AppId,
    run: &Row,
    now: i64,
) -> Result<bool, WorkflowServiceError> {
    let id = run.text("id")?;
    let generation = run.integer("generation")?;
    let mut progress = false;
    for mut step in load(tx, app, &id, generation).await? {
        if step.state != "running" {
            continue;
        }
        let expired = step
            .wake_at
            .is_some_and(|due| due.timestamp_millis() <= now);
        let mut output = None;
        let mut error = None;
        if step.kind == "sleep" && expired {
            output = Some(Value::Null);
        }
        if step.kind == "wait_signal" {
            let signals = tx
                .database()
                .entity::<models::signals::Entity>()?
                .alias("s")?;
            let oldest = step
                .max_signal_age_ms
                .map(|age| now.saturating_sub(age))
                .unwrap_or(i64::MIN);
            let latest = step
                .wake_at
                .map_or(now, |due| due.timestamp_millis().min(now));
            let signal_type = step.signal_type.as_deref().ok_or_else(|| {
                WorkflowServiceError::Internal("workflow signal wait has no type".into())
            })?;
            let rows = tx
                .database()
                .from(&signals)
                .filter(
                    signals
                        .column(models::signals::app_id)
                        .eq(app.as_str())?
                        .and(signals.column(models::signals::run_id).eq(id.as_str())?)
                        .and(
                            signals
                                .column(models::signals::signal_type)
                                .eq(signal_type)?,
                        )
                        .and(
                            signals
                                .column(models::signals::consumed_generation)
                                .eq(None::<i64>)?,
                        )
                        .and(signals.column(models::signals::created_at).gte(oldest)?)
                        .and(signals.column(models::signals::created_at).lte(latest)?)
                        .and(
                            signals
                                .column(models::signals::target_generation)
                                .eq(None::<i64>)?
                                .or(signals
                                    .column(models::signals::target_generation)
                                    .eq(Some(generation))?
                                    .and(
                                        signals
                                            .column(models::signals::target_ordinal)
                                            .eq(Some(i64::from(step.ordinal)))?,
                                    )),
                        ),
                )
                .order_by(signals.column(models::signals::created_at).asc())
                .order_by(signals.column(models::signals::id).asc())
                .select(signals.row::<models::SignalMessage>())?
                .limit(1)?
                .all()
                .await?;
            if let Some(signal) = rows.first() {
                let signal_id = signal.id.clone();
                let created_at = chrono::DateTime::from_timestamp_millis(signal.created_at)
                    .ok_or_else(|| {
                        WorkflowServiceError::Internal("invalid workflow signal timestamp".into())
                    })?;
                output = Some(json!({
                    "id":signal_id,"type":step.signal_type,"payload":decode::<Value>(&signal.payload)?,
                    "createdAt":created_at.to_rfc3339(),"origin":signal.origin,
                    "delivery":signal.delivery,"topic":signal.topic,
                }));
                step.consumed_signal_id = Some(signal_id.clone());
                let consumed = tx.database().collection(models::signals::Entity::COLLECTION)?.execute(Operation::Update {
                    filter:value!({"app_id":app.as_str(), "id":signal_id, "run_id":id.clone(), "consumed_generation":null}),
                    patch:value!({"consumed_generation":generation, "consumed_ordinal":i64::from(step.ordinal)}), many:true,
                }).await?;
                if !matches!(consumed, Output::Count(1)) {
                    return Err(WorkflowServiceError::Conflict(
                        "workflow signal was already consumed".into(),
                    ));
                }
            } else if expired {
                output = Some(Value::Null);
            }
        }
        if step.kind == "child" {
            let child_id = step.child_run_id.as_deref().ok_or_else(|| {
                WorkflowServiceError::Internal("workflow child reference is missing".into())
            })?;
            let (child, outcome) = current_run(tx, app, child_id).await?.ok_or_else(|| {
                WorkflowServiceError::Internal("workflow child reference is missing".into())
            })?;
            let state = parse_state(&child.state)?;
            if state == crate::operations::RunState::Completed {
                if outcome.output_ref.is_some() {
                    step.output_ref = Some(
                        super::payloads::inherit_child_output(
                            tx,
                            app,
                            super::payloads::RunGeneration {
                                id: step.child_run_id.as_deref().ok_or_else(|| {
                                    WorkflowServiceError::Internal("missing workflow child".into())
                                })?,
                                generation: child.generation,
                            },
                            super::payloads::RunGeneration {
                                id: &id,
                                generation,
                            },
                            step.ordinal,
                            now,
                        )
                        .await?,
                    );
                } else {
                    output = Some(
                        outcome
                            .output
                            .map(|value| decode(&value))
                            .transpose()?
                            .unwrap_or(Value::Null),
                    );
                }
            } else if state.is_terminal() {
                error=Some(outcome.error.map(|value|decode(&value)).transpose()?.unwrap_or(json!({"name":"ChildWorkflowError","message":"child workflow was cancelled"})));
            } else if expired {
                error = Some(
                    json!({"name":"WorkflowTimeoutError","message":"child workflow wait expired"}),
                );
            }
        }
        if output.is_none() && error.is_none() && step.output_ref.is_none() {
            continue;
        }
        step.state = if error.is_some() {
            "failed"
        } else {
            "completed"
        }
        .into();
        step.output = output;
        step.error = error;
        update(tx, app, &id, generation, &step).await?;
        clear_wait(tx, app, &id, generation, step.ordinal).await?;
        progress = true;
    }
    Ok(progress)
}
pub(crate) async fn clear_wait(
    tx: &mut Transaction,
    app: &AppId,
    id: &str,
    generation: i64,
    ordinal: i32,
) -> Result<(), WorkflowServiceError> {
    for table in [
        models::waits::Entity::COLLECTION,
        models::subscriptions::Entity::COLLECTION,
    ] {
        tx.database().collection(table)?.execute(Operation::Purge {
            filter:value!({"app_id":app.as_str(), "run_id":id, "generation":generation, "ordinal":i64::from(ordinal)}),
            many:false,
        }).await?;
    }
    Ok(())
}
pub(crate) fn invalid<T>(message: &str) -> Result<T, WorkflowServiceError> {
    Err(WorkflowServiceError::InvalidRequest(message.into()))
}
