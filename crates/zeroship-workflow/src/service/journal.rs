use super::{
    app::{decode, encode, insert_root_run, parse_state},
    store::{Row, Transaction},
    AppPolicy,
};
use crate::{
    engine::{JournalStep, StepCheckpoint},
    operations::{ConflictPolicy, StartOptions},
    validation, WorkflowServiceError,
};
use serde_json::{json, Value};
use zeroship_core::{app_id::AppId, typed_id};

pub(crate) async fn load(
    tx: &mut Transaction,
    app: &AppId,
    id: &str,
    generation: i64,
) -> Result<Vec<StepCheckpoint>, WorkflowServiceError> {
    let steps = tx.table("steps");
    tx.query(&format!("SELECT record FROM {steps} WHERE app_id=$1 AND run_id=$2 AND generation=$3 ORDER BY ordinal"), &[app.as_str().into(),id.into(),generation.into()]).await?.iter().map(|row| decode(&row.text("record")?)).collect()
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
    let steps = tx.table("steps");
    tx.execute(&format!("UPDATE {steps} SET state=$5,record=$6 WHERE app_id=$1 AND run_id=$2 AND generation=$3 AND ordinal=$4"), &[app.as_str().into(),id.into(),generation.into(),i64::from(step.ordinal).into(),step.state.clone().into(),encode(step)?.into()]).await?;
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
    let steps = tx.table("steps");
    for mut step in checkpoints {
        if step.ordinal < 0
            || step.name.is_empty()
            || step.name.len() > 256
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
        tx.execute(&format!("INSERT INTO {steps} (app_id,run_id,generation,ordinal,name,occurrence,origin_generation,kind,state,record,compensation_retry_ms) VALUES ($1,$2,$3,$4,$5,$6,$3,$7,$8,$9,$10)"),
            &[app.as_str().into(),id.clone().into(),generation.into(),i64::from(step.ordinal).into(),step.name.clone().into(),i64::from(step.name_occurrence).into(),step.kind.clone().into(),step.state.clone().into(),encode(&step)?.into(),policy.compensation_retry_ms.into()]).await?;
        if step.state == "running" {
            let waits = tx.table("waits");
            tx.execute(&format!("INSERT INTO {waits} (app_id,run_id,generation,ordinal,kind,signal_type,topic,max_signal_age,due_at,child_id) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)"),
                &[app.as_str().into(),id.clone().into(),generation.into(),i64::from(step.ordinal).into(),step.kind.clone().into(),step.signal_type.clone().into(),step.topic.clone().into(),step.max_signal_age_ms.into(),step.wake_at.map(|time|time.timestamp_millis()).into(),step.child_run_id.clone().into()]).await?;
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
    let deploys = tx.table("deploys");
    let deploy_id = parent.text("deploy_id")?;
    let rows = tx
        .query(
            &format!(
                "SELECT manifest FROM {deploys} WHERE app_id=$1 AND id=$2 AND state='available'"
            ),
            &[app.as_str().into(), deploy_id.clone().into()],
        )
        .await?;
    let deploy: super::DeployRegistration = decode(
        &rows
            .first()
            .ok_or_else(|| {
                WorkflowServiceError::Unavailable("pinned child deployment is unavailable".into())
            })?
            .text("manifest")?,
    )?;
    if !deploy.workflows.contains(name) {
        return invalid("child workflow is absent from the pinned deployment");
    }
    let options = step.child_options.clone().unwrap_or_default();
    let runs = tx.table("runs");
    if let Some(key) = &options.key {
        let rows = tx
            .query(
                &format!("SELECT id FROM {runs} WHERE app_id=$1 AND workflow_name=$2 AND key=$3"),
                &[app.as_str().into(), name.into(), key.clone().into()],
            )
            .await?;
        if let Some(row) = rows.first() {
            let id = row.text("id")?;
            // A keyed child must not introduce an ancestor wait cycle.
            let ancestors=tx.query(&format!("WITH RECURSIVE ancestors AS (SELECT id,parent_id FROM {runs} WHERE app_id=$1 AND id=$2 UNION ALL SELECT r.id,r.parent_id FROM {runs} r JOIN ancestors a ON a.parent_id=r.id WHERE r.app_id=$1) SELECT id FROM ancestors WHERE id=$3"), &[app.as_str().into(),parent.text("id")?.into(),id.clone().into()]).await?;
            if !ancestors.is_empty() {
                return invalid("child workflow would wait on its ancestor");
            }
            let waits = tx.table("waits");
            let cycle = tx.query(&format!("WITH RECURSIVE dependencies(id) AS (SELECT id FROM {runs} WHERE app_id=$1 AND id=$2 UNION SELECT w.child_id FROM {waits} w JOIN dependencies d ON w.run_id=d.id JOIN {runs} r ON r.app_id=w.app_id AND r.id=w.run_id AND r.generation=w.generation WHERE w.app_id=$1 AND w.child_id IS NOT NULL) SELECT id FROM dependencies WHERE id=$3"),
                &[app.as_str().into(),id.clone().into(),parent.text("id")?.into()]).await?;
            if !cycle.is_empty() {
                return invalid("child workflow would create a dependency cycle");
            }
            return Ok(id);
        }
    }
    if parent.integer("depth")? >= policy.max_child_depth {
        return Err(WorkflowServiceError::ResourceExhausted(
            "workflow child depth limit reached".into(),
        ));
    }
    let live=tx.query(&format!("SELECT COUNT(*) AS total FROM {runs} WHERE app_id=$1 AND state NOT IN ('completed','failed','cancelled')"), &[app.as_str().into()]).await?;
    if live[0].integer("total")? >= policy.max_live_runs {
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
    tx.execute(&format!("UPDATE {runs} SET parent_id=$3,parent_generation=$4,parent_ordinal=$5,cascade=$6,depth=$7 WHERE app_id=$1 AND id=$2"),
        &[app.as_str().into(),id.clone().into(),parent.text("id")?.into(),parent.integer("generation")?.into(),i64::from(step.ordinal).into(),i64::from(options.cascade).into(),(parent.integer("depth")?+1).into()]).await?;
    Ok(id)
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
            let signals = tx.table("signals");
            let oldest = step
                .max_signal_age_ms
                .map(|age| now.saturating_sub(age))
                .unwrap_or(i64::MIN);
            let latest = step
                .wake_at
                .map_or(now, |due| due.timestamp_millis().min(now));
            let rows=tx.query(&format!("SELECT id,payload,created_at,origin,delivery,topic FROM {signals} WHERE app_id=$1 AND run_id=$2 AND signal_type=$3 AND consumed_generation IS NULL AND created_at >= $4 AND created_at <= $5 AND (target_generation IS NULL OR (target_generation=$6 AND target_ordinal=$7)) ORDER BY created_at,id LIMIT 1"),
                &[app.as_str().into(),id.clone().into(),step.signal_type.clone().into(),oldest.into(),latest.into(),generation.into(),i64::from(step.ordinal).into()]).await?;
            if let Some(signal) = rows.first() {
                let signal_id = signal.text("id")?;
                let created_at =
                    chrono::DateTime::from_timestamp_millis(signal.integer("created_at")?)
                        .ok_or_else(|| {
                            WorkflowServiceError::Internal(
                                "invalid workflow signal timestamp".into(),
                            )
                        })?;
                output = Some(json!({
                    "id":signal_id,"type":step.signal_type,"payload":decode::<Value>(&signal.text("payload")?)?,
                    "createdAt":created_at.to_rfc3339(),"origin":signal.text("origin")?,
                    "delivery":signal.text("delivery")?,"topic":signal.optional_text("topic")?,
                }));
                step.consumed_signal_id = Some(signal_id.clone());
                tx.execute(&format!("UPDATE {signals} SET consumed_generation=$3,consumed_ordinal=$4 WHERE app_id=$1 AND id=$2 AND run_id=$5 AND consumed_generation IS NULL"),
                    &[app.as_str().into(),signal_id.into(),generation.into(),i64::from(step.ordinal).into(),id.clone().into()]).await?;
            } else if expired {
                output = Some(Value::Null);
            }
        }
        if step.kind == "child" {
            let runs = tx.table("runs");
            let generations = tx.table("generations");
            let rows=tx.query(&format!("SELECT r.state,r.generation,g.output,g.output_ref,g.error FROM {runs} r JOIN {generations} g ON g.app_id=r.app_id AND g.run_id=r.id AND g.generation=r.generation WHERE r.app_id=$1 AND r.id=$2"), &[app.as_str().into(),step.child_run_id.clone().into()]).await?;
            let child = rows.first().ok_or_else(|| {
                WorkflowServiceError::Internal("workflow child reference is missing".into())
            })?;
            let state = parse_state(&child.text("state")?)?;
            if state == crate::operations::RunState::Completed {
                if child.optional_text("output_ref")?.is_some() {
                    step.output_ref = Some(
                        super::payloads::inherit_child_output(
                            tx,
                            app,
                            super::payloads::RunGeneration {
                                id: step.child_run_id.as_deref().ok_or_else(|| {
                                    WorkflowServiceError::Internal("missing workflow child".into())
                                })?,
                                generation: child.integer("generation")?,
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
                        child
                            .optional_text("output")?
                            .map(|value| decode(&value))
                            .transpose()?
                            .unwrap_or(Value::Null),
                    );
                }
            } else if state.is_terminal() {
                error=Some(child.optional_text("error")?.map(|value|decode(&value)).transpose()?.unwrap_or(json!({"name":"ChildWorkflowError","message":"child workflow was cancelled"})));
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
    for name in ["waits", "subscriptions"] {
        let table = tx.table(name);
        tx.execute(&format!("DELETE FROM {table} WHERE app_id=$1 AND run_id=$2 AND generation=$3 AND ordinal=$4"), &[app.as_str().into(),id.into(),generation.into(),i64::from(ordinal).into()]).await?;
    }
    Ok(())
}
pub(crate) fn invalid<T>(message: &str) -> Result<T, WorkflowServiceError> {
    Err(WorkflowServiceError::InvalidRequest(message.into()))
}
