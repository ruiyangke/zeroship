use super::{
    app::{current_run, decode, encode, insert_root_run, keyed_run, live_runs},
    continuations, models,
    store::{Row, Transaction},
    AppPolicy,
};
use crate::{
    engine::{JournalStep, StepCheckpoint},
    operations::{ConflictPolicy, StartOptions},
    validation, WorkflowServiceError,
};
use serde::{Deserialize, Serialize};
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
    run_id: String,
    generation: i64,
    ordinal: i64,
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
            let step = read_checkpoint(tx, app, &row).await?;
            journal.push(step);
        }
        if count < page_limit as usize {
            break;
        }
    }
    Ok(journal)
}
/// The journal as the replay bridge sees it.
///
/// A `retrying` step is deliberately absent: the bridge re-issues an ordinal it
/// holds no row for, so leaving the hole is what makes the body run again. Every
/// later ordinal keeps its own row, so a frontier that failed beside completed
/// siblings replays those from the journal and re-executes only the one that has
/// attempts left.
pub(crate) fn replay(steps: &[StepCheckpoint]) -> Vec<JournalStep> {
    steps
        .iter()
        .filter(|step| step.state != "retrying")
        .map(|step| JournalStep {
            ordinal: step.ordinal,
            name: step.name.clone(),
            name_occurrence: step.name_occurrence,
            kind: step.kind.clone(),
            state: step.state.clone(),
            output: step.output.clone(),
            output_ref: step.output_ref.clone(),
            error: replay_error(step.error.as_ref()),
            child_run_id: step.child_run_id.clone(),
            compensation_state: step.compensation_state.clone(),
        })
        .collect()
}

/// The keys the replay bridge rebuilds a thrown error from.
const REPLAYED_ERROR_KEYS: [&str; 4] = ["type", "message", "stack", "retryable"];

/// A recorded failure as the replay bridge reads it.
///
/// `wfDeserializeError` in `crates/zeroship-workflow-v8/js/dispatch.js` picks a
/// class by `type`, takes `message`, overwrites `stack` and copies `retryable`
/// when it is a boolean. It reads nothing else, so every other key a body
/// recorded is already dropped there rather than reaching a `catch`. This is the
/// one unbounded field of the row with no reference form at any size, and the
/// view is rebuilt for every dispatch, so it is narrowed here instead of
/// travelling to the worker to be discarded.
///
/// The stored checkpoint keeps its value whole: [`retryable`] decides whether a
/// failed step gets another execution, [`validate_child_checkpoint`] reads the
/// type of an unsettled child join, and [`validate_child_result`] matches a
/// settled one against the error the child itself recorded.
///
/// A recorded value that is not an object carries none of these keys and leaves
/// an empty one, which is what the bridge already makes of it: neither spelling
/// selects a class or a message, so both reach the body as a bare `Error`.
fn replay_error(error: Option<&Value>) -> Option<Value> {
    let error = error?;
    let mut projected = serde_json::Map::new();
    for key in REPLAYED_ERROR_KEYS {
        if let Some(value) = error.get(key) {
            projected.insert(key.to_owned(), value.clone());
        }
    }
    Some(Value::Object(projected))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCheckpoint {
    step: StepCheckpoint,
    child_member_id: Option<String>,
    child_result_member_id: Option<String>,
}

pub(super) fn encode_checkpoint(
    step: &StepCheckpoint,
    accepted: Option<&str>,
    result: Option<&str>,
) -> Result<String, WorkflowServiceError> {
    encode(&StoredCheckpoint {
        step: step.clone(),
        child_member_id: accepted.map(str::to_owned),
        child_result_member_id: result.map(str::to_owned),
    })
}

pub(super) fn decode_checkpoint(
    record: &str,
    accepted: Option<&str>,
    result: Option<&str>,
) -> Result<StepCheckpoint, WorkflowServiceError> {
    let record: StoredCheckpoint = decode(record)?;
    if record.child_member_id.as_deref() != accepted
        || record.child_result_member_id.as_deref() != result
    {
        return Err(WorkflowServiceError::Internal(
            "workflow checkpoint projections changed".into(),
        ));
    }
    Ok(record.step)
}

pub(super) async fn read_checkpoint(
    tx: &Transaction,
    app: &AppId,
    row: &models::StoredStep,
) -> Result<StepCheckpoint, WorkflowServiceError> {
    let step = decode_checkpoint(
        &row.record,
        row.child_member_id.as_deref(),
        row.child_result_member_id.as_deref(),
    )?;
    if i64::from(step.ordinal) != row.ordinal {
        return Err(WorkflowServiceError::Internal(
            "workflow checkpoint ordinal changed".into(),
        ));
    }
    validate_child_checkpoint(tx, app, row, &step).await?;
    Ok(step)
}

async fn validate_child_checkpoint(
    tx: &Transaction,
    app: &AppId,
    row: &models::StoredStep,
    step: &StepCheckpoint,
) -> Result<(), WorkflowServiceError> {
    let invalid =
        || WorkflowServiceError::Internal("invalid workflow child checkpoint linkage".into());
    if step.kind != "child" {
        if row.child_member_id.is_some() || row.child_result_member_id.is_some() {
            return Err(invalid());
        }
        return Ok(());
    }
    let accepted =
        continuations::historical(tx, app, row.child_member_id.as_deref().ok_or_else(invalid)?)
            .await?;
    let expected = if let Some(result) = &row.child_result_member_id {
        if !matches!(step.state.as_str(), "completed" | "failed") {
            return Err(invalid());
        }
        let result = continuations::historical(tx, app, result).await?;
        if result.head_id != accepted.head_id || result.revision < accepted.revision {
            return Err(invalid());
        }
        validate_child_result(step, &result)?;
        result.run_id
    } else {
        if step.state != "running"
            && !(step.state == "failed"
                && step
                    .error
                    .as_ref()
                    .and_then(|error| error.get("type"))
                    .and_then(Value::as_str)
                    == Some("ChildTimeoutError"))
        {
            return Err(invalid());
        }
        accepted.run_id
    };
    if step.child_run_id.as_deref() != Some(expected.as_str()) {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn validate_child_result(
    step: &StepCheckpoint,
    result: &continuations::HistoricalMember,
) -> Result<(), WorkflowServiceError> {
    let invalid =
        || WorkflowServiceError::Internal("workflow child result provenance changed".into());
    match result.state.as_str() {
        "completed" => {
            let reference = result
                .outcome
                .output_ref
                .as_deref()
                .map(decode)
                .transpose()?;
            // A completed child's result is the object its descriptor names, and
            // a child that returned nothing names no object, so the consuming
            // checkpoint carries no inline value either way. A stored checkpoint
            // round-trips an inline JSON null as an absent output, so both
            // spellings of nothing are the same consumed result.
            if step.state != "completed"
                || step.output.as_ref().is_some_and(|value| !value.is_null())
                || step.output_ref != reference
                || step.error.is_some()
            {
                return Err(invalid());
            }
        }
        "failed" | "cancelled" => {
            let error = result.outcome.error.as_deref().map(decode::<Value>).transpose()?.unwrap_or_else(|| json!({"type":"ChildCancelledError","message":"child workflow was cancelled"}));
            if step.state != "failed"
                || step.error.as_ref() != Some(&error)
                || step.output.is_some()
                || step.output_ref.is_some()
            {
                return Err(invalid());
            }
        }
        _ => return Err(invalid()),
    }
    Ok(())
}
pub(crate) async fn update(
    tx: &mut Transaction,
    app: &AppId,
    id: &str,
    generation: i64,
    step: &StepCheckpoint,
) -> Result<(), WorkflowServiceError> {
    save_checkpoint(tx, app, id, generation, step, None).await
}

/// Rewrite one step row in place, and record that the journal moved.
///
/// This is the single funnel for rewriting a step a replay has already been
/// handed, so the run's journal revision is advanced here rather than at each
/// caller: a caller added later inherits the bookkeeping instead of having to
/// remember it, and a rewrite that reached the row without advancing the
/// revision would leave two dispatches disagreeing about one revision.
///
/// The frontier revision cannot carry this. It authorizes one dispatch and is
/// pinned for that authorization's whole lifetime - the advance job is
/// published at it, `publication_id` hashes it into an immutable operation,
/// [`super::tasks::assign`] stamps it on the task inside the transaction that
/// consumes that job, and `authorize_task` refuses every later claim whose task
/// disagrees with the run. Moving it from here would dispatch runs that could
/// never report.
async fn save_checkpoint(
    tx: &Transaction,
    app: &AppId,
    id: &str,
    generation: i64,
    step: &StepCheckpoint,
    result: Option<&str>,
) -> Result<(), WorkflowServiceError> {
    let mut row = tx
        .database()
        .entity::<models::steps::Entity>()?
        .find::<models::StoredStep>(
            models::steps::app_id
                .eq(app.as_str())?
                .and(models::steps::run_id.eq(id)?)
                .and(models::steps::generation.eq(generation)?)
                .and(models::steps::ordinal.eq(i64::from(step.ordinal))?),
            FindOptions {
                limit: Some(1),
                ..FindOptions::default()
            },
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| WorkflowServiceError::Internal("workflow checkpoint is missing".into()))?;
    read_checkpoint(tx, app, &row).await?;
    if let Some(result) = result {
        row.child_result_member_id = Some(result.to_owned());
    }
    row.record = encode_checkpoint(
        step,
        row.child_member_id.as_deref(),
        row.child_result_member_id.as_deref(),
    )?;
    validate_child_checkpoint(tx, app, &row, step).await?;
    let changed = tx
        .database()
        .entity::<models::steps::Entity>()?
        .update_many(
            models::steps::app_id
                .eq(app.as_str())?
                .and(models::steps::run_id.eq(id)?)
                .and(models::steps::generation.eq(generation)?)
                .and(models::steps::ordinal.eq(i64::from(step.ordinal))?),
            models::steps::state
                .set(step.state.as_str())?
                .and(models::steps::record.set(row.record)?)?
                .and(models::steps::child_result_member_id.set(row.child_result_member_id)?)?,
        )
        .await?;
    if changed != 1 {
        return Err(WorkflowServiceError::Internal(
            "workflow checkpoint changed".into(),
        ));
    }
    let moved = tx
        .database()
        .collection(models::runs::Entity::COLLECTION)?
        .execute(Operation::Update {
            filter: value!({"app_id":app.as_str(), "id":id, "journal_revision":{"$lt":i64::MAX}}),
            patch: value!({"$inc":{"journal_revision":1}}),
            many: true,
        })
        .await?;
    if !matches!(moved, Output::Count(1)) {
        return Err(WorkflowServiceError::ResourceExhausted(
            "workflow journal revision exhausted".into(),
        ));
    }
    Ok(())
}

/// Does a step that reported this failure get another execution?
///
/// The journal carries `retryable` exactly when the thrown error declared one,
/// and a declared `false` is the body saying this failure cannot be cleared by
/// running it again. An error that declares nothing is retried: absent is not a
/// refusal, and the SDK's own permanent conditions all declare theirs.
fn retryable(error: Option<&Value>) -> bool {
    error
        .and_then(|error| error.get("retryable"))
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

/// Settle one execution of a step against the attempts it has left.
///
/// Every execution of a `run` body that reports an outcome is counted, so a
/// completed row says what it cost rather than implying one execution. A failure
/// with attempts remaining is then not a terminal journal fact: the row records
/// what has been spent and when the next attempt is due, and the replay bridge
/// is handed no row at all, so the body runs again. A failure with none left
/// records the error the run then fails on.
///
/// `prior` is what earlier executions of this ordinal already spent: zero when
/// the row is being created, and the held row's count when one is replacing it.
fn settle_attempt(
    step: &mut StepCheckpoint,
    prior: i32,
    policy: &AppPolicy,
    now: i64,
) -> Result<(), WorkflowServiceError> {
    if step.kind != "run" {
        return Ok(());
    }
    step.attempts = prior
        .checked_add(1)
        .ok_or_else(|| WorkflowServiceError::Internal("workflow step attempt overflow".into()))?;
    if !matches!(step.state.as_str(), "failed" | "retrying") {
        return Ok(());
    }
    step.state = "failed".into();
    step.wake_at = None;
    if step.attempts < step.max_attempts && retryable(step.error.as_ref()) {
        step.state = "retrying".into();
        step.wake_at = Some(
            chrono::DateTime::from_timestamp_millis(super::app::deadline(
                now,
                policy.retry_delay_ms,
            )?)
            .ok_or_else(|| {
                WorkflowServiceError::Internal("workflow retry deadline is out of range".into())
            })?,
        );
    }
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
        // What an earlier execution of this ordinal already spent, when the row
        // it left is one this outcome may replace. `None` means the checkpoint
        // is new and everything below is creating it.
        let mut held = None;
        if let Some(existing) = journal.iter().find(|entry| entry.ordinal == step.ordinal) {
            // A step with attempts left holds its ordinal without settling it,
            // so the outcome of the next execution replaces the row rather than
            // rewriting history. Identity still has to match: the replay bridge
            // was handed no row here, so this is the one place a body that
            // re-issued a different operation at this ordinal can be caught.
            if existing.state == "retrying" {
                if existing.name != step.name
                    || existing.name_occurrence != step.name_occurrence
                    || existing.kind != step.kind
                {
                    return invalid("workflow checkpoint rewrites committed history");
                }
                held = Some((existing.attempts, encode(existing)?.len()));
            } else {
                // Pending effects can be re-reported by a replay suspended on a
                // previously accepted frontier. Their original deadlines and
                // child identities remain authoritative.
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
        } else if step.ordinal as usize != journal.len()
            || step.name_occurrence as usize
                != journal
                    .iter()
                    .filter(|entry| entry.name == step.name)
                    .count()
        {
            return invalid("workflow checkpoint is not the next journal operation");
        }
        settle_attempt(&mut step, held.map_or(0, |(spent, _)| spent), policy, now)?;
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
                Some(run),
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
        let child_member = if step.kind == "child" {
            if step.state != "running" {
                return invalid("child acceptance requires a pending checkpoint");
            }
            let accepted = child(tx, app, run, policy, &step, now).await?;
            step.child_run_id = Some(accepted.run_id);
            Some(accepted.id)
        } else {
            None
        };
        journal_bytes = journal_bytes
            .saturating_sub(held.map_or(0, |(_, spent)| spent))
            .checked_add(encode(&step)?.len())
            .ok_or_else(|| {
                WorkflowServiceError::ResourceExhausted("workflow journal size overflow".into())
            })?;
        if journal_bytes > policy.max_journal_bytes {
            return Err(WorkflowServiceError::ResourceExhausted(
                "workflow journal size limit reached".into(),
            ));
        }
        if held.is_some() {
            save_checkpoint(tx, app, &id, generation, &step, None).await?;
            let ordinal = step.ordinal;
            if let Some(entry) = journal.iter_mut().find(|entry| entry.ordinal == ordinal) {
                *entry = step;
            }
            continue;
        }
        // A compensator runs arbitrarily later than the step it undoes, so the
        // row pins the delay policy held when the step was journalled. A forward
        // retry is scheduled in this same commit, and `settle_attempt` has
        // already read the live value, so there is nothing to pin for it.
        steps.insert(value!({
            "id":super::types::storage_id(), "app_id":app.as_str(), "run_id":id.clone(), "generation":generation,
            "ordinal":i64::from(step.ordinal), "name":step.name.clone(), "occurrence":i64::from(step.name_occurrence),
            "origin_generation":generation, "kind":step.kind.clone(), "state":step.state.clone(),
            "record":encode_checkpoint(&step, child_member.as_deref(), None)?, "compensation_retry_ms":policy.retry_delay_ms,
            "child_member_id":child_member, "child_result_member_id":null,
        })).await?;
        if step.state == "running" {
            tx.database().collection(models::waits::Entity::COLLECTION)?
                .insert(value!({
                    "id":super::types::storage_id(), "app_id":app.as_str(), "run_id":id.clone(), "generation":generation,
                    "ordinal":i64::from(step.ordinal), "kind":step.kind.clone(), "signal_type":step.signal_type.clone(),
                    "topic":step.topic.clone(), "max_signal_age":step.max_signal_age_ms,
                    "due_at":step.wake_at.map(|time|time.timestamp_millis()),
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
) -> Result<continuations::Member, WorkflowServiceError> {
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
            let (run, _) = current_run(tx, app, &id).await?.ok_or_else(|| {
                WorkflowServiceError::Internal("keyed workflow child is missing".into())
            })?;
            return continuations::member(tx, app, &id, run.generation).await;
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
        input_ref: step.child_input_ref.clone(),
        key: options.key,
        on_conflict: ConflictPolicy::Join,
    };
    validation::start(&start)?;
    // The parent's live task staged what it passed the child, so the parent's
    // row is what proves this run may take the object.
    insert_root_run(tx, app, &id, name, &deploy_id, &start, Some(parent), now).await?;
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
    continuations::member(tx, app, &id, 0).await
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
                .and(wait.column(models::waits::kind).eq("child")?);
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
                let target =
                    continuations::resolve(tx, app, &edge.run_id, edge.generation, edge.ordinal)
                        .await?;
                pending.push((target.current.run_id, false));
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
        let mut child_result = None;
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
                .order_by(signals.column(models::signals::delivery_sequence).asc())
                .order_by(signals.column(models::signals::id).asc())
                .select(signals.row::<models::SignalMessage>())?
                .limit(1)?
                .all()
                .await?;
            if let Some(signal) = rows.first() {
                super::fanout::signals::validate_sequence(tx, app, signal.delivery_sequence)
                    .await?;
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
            let child =
                continuations::resolve(tx, app, &id, generation, i64::from(step.ordinal)).await?;
            let state = child.state;
            let outcome = child.outcome;
            if state.is_terminal() {
                step.child_run_id = Some(child.current.run_id.clone());
                child_result = Some(child.current.id);
            }
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
                                generation: child.current.generation,
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
                    // No descriptor means the child returned nothing. The step
                    // still resolves: a JSON null is what the parent's body
                    // receives, and it is what marks this checkpoint completed.
                    output = Some(Value::Null);
                }
            } else if state.is_terminal() {
                error=Some(outcome.error.map(|value|decode(&value)).transpose()?.unwrap_or(json!({"type":"ChildCancelledError","message":"child workflow was cancelled"})));
            } else if expired {
                error = Some(
                    json!({"type":"ChildTimeoutError","message":"child workflow wait expired"}),
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
        save_checkpoint(tx, app, &id, generation, &step, child_result.as_deref()).await?;
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
