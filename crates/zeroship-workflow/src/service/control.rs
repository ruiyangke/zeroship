use super::{
    app::{
        active_deploy, deadline, emit, live_runs, lock_app, lock_run, parse_state, request_result,
        store_request, validate_run,
    },
    frontier, journal, models,
    store::Transaction,
    types::digest,
    AppWorkflows, RequestId,
};
use crate::{
    lifecycle::RestartSafety,
    operations::{
        RestartDeploy, RestartOptions, RestartedRun, RunOperation, RunState, TransitionedRun,
    },
    WorkflowServiceError,
};
use serde_json::json;
use std::collections::BTreeSet;
use zeroship_core::app_id::AppId;
use zeroship_data_orm::{
    orm::{Entity, FindOptions, Operation, Output},
    sql::{CompareOp, Literal, Operand, Predicate, RowLimit},
    value,
};

mod replay;

const MAX_DESCENDANT_INSPECTIONS: usize = 16_384;

impl AppWorkflows {
    pub async fn transition(
        &self,
        request: &RequestId,
        run_id: &str,
        operation: RunOperation,
    ) -> Result<TransitionedRun, WorkflowServiceError> {
        validate_run(run_id)?;
        let digest = digest(&(run_id, operation))?;
        let mut tx = self.service.begin().await?;
        let policy = lock_app(&mut tx, &self.app).await?;
        let now = tx.now().await?;
        if let Some(receipt) =
            request_result(&mut tx, &self.app, request, "transition", &digest, now).await?
        {
            return Ok(receipt);
        }
        let run = lock_run(&mut tx, &self.app, run_id).await?;
        let mut state = parse_state(&run.text("state")?)?;
        if state.is_terminal() {
            if operation != RunOperation::Cancel {
                return Err(WorkflowServiceError::Conflict(
                    "workflow run is terminal".into(),
                ));
            }
        } else {
            let runs = tx.database().collection(models::runs::Entity::COLLECTION)?;
            let filter = value!({"app_id":self.app.as_str(), "id":run_id});
            // The run lock remains held while these patches use its lease state.
            let leased = run.optional_text("task_id")?.is_some();
            match operation {
                RunOperation::Pause => {
                    runs.update(filter, value!({"control":"pause"})).await?;
                    if !leased {
                        frontier::park(&mut tx, &self.app, &run).await?;
                        state = RunState::Paused;
                    }
                }
                RunOperation::Resume => {
                    policy.admit()?;
                    let phase = if run.optional_text("compensation_target")?.is_some() {
                        "compensating"
                    } else {
                        "queued"
                    };
                    let patch = if leased {
                        value!({"control":"none"})
                    } else {
                        value!({"control":"none", "state":phase, "due_at":now})
                    };
                    runs.update(filter, patch).await?;
                    if !leased {
                        state = parse_state(phase)?;
                    }
                }
                RunOperation::Cancel => {
                    let patch = if leased {
                        value!({"control":"cancel"})
                    } else {
                        value!({"control":"cancel", "due_at":now})
                    };
                    runs.update(filter, patch).await?;
                }
            }
        }
        let result = TransitionedRun { state };
        store_request(
            &mut tx,
            &self.app,
            request,
            "transition",
            &digest,
            &result,
            deadline(now, policy.request_retention_ms)?,
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn restart(
        &self,
        request: &RequestId,
        run_id: &str,
        options: RestartOptions,
    ) -> Result<RestartedRun, WorkflowServiceError> {
        validate_run(run_id)?;
        let deploy_policy = crate::lifecycle::restart_deploy_policy(&options)?;
        let digest = digest(&(run_id, &options))?;
        let mut tx = self.service.begin().await?;
        let policy = lock_app(&mut tx, &self.app).await?;
        let now = tx.now().await?;
        if let Some(receipt) =
            request_result(&mut tx, &self.app, request, "restart", &digest, now).await?
        {
            return Ok(receipt);
        }
        policy.admit()?;
        let run = lock_run(&mut tx, &self.app, run_id).await?;
        let current = run.integer("generation")?;
        let steps = journal::load(&mut tx, &self.app, run_id, current).await?;
        let from = if let Some(target) = &options.from {
            let matches: Vec<_> = steps
                .iter()
                .filter(|step| {
                    step.name == target.name
                        && target.occurrence.is_none_or(|occurrence| {
                            i64::from(occurrence) == i64::from(step.name_occurrence)
                        })
                })
                .collect();
            if matches.len() != 1 {
                return Err(WorkflowServiceError::InvalidRequest(
                    "restart target is missing or ambiguous".into(),
                ));
            }
            Some(matches[0].ordinal)
        } else {
            None
        };
        let prefix = from.unwrap_or(0);
        let retained: Vec<_> = steps.iter().filter(|step| step.ordinal < prefix).collect();
        let tasks = tx
            .database()
            .collection(models::tasks::Entity::COLLECTION)?;
        if parse_state(&run.text("state")?)?.is_terminal()
            && live_runs(&tx, &self.app).await? >= policy.max_live_runs
        {
            return Err(WorkflowServiceError::ResourceExhausted(
                "workflow live-run limit reached".into(),
            ));
        }
        let Output::Count(live) = tasks
            .count(
                value!({"app_id":self.app.as_str(), "run_id":run_id, "generation":current,
                "state":"leased", "deadline":{"$gt":now}}),
                value!({}),
            )
            .await?
        else {
            return Err(WorkflowServiceError::Internal(
                "workflow count returned rows".into(),
            ));
        };
        let active_descendants = has_active_descendants(&tx, &self.app, run_id).await?;
        let db = tx.database();
        let child = db.entity::<models::runs::Entity>()?.alias("r")?;
        let wait = db.entity::<models::waits::Entity>()?.alias("w")?;
        let waiting_children = db
            .from(&child)
            .inner_join(
                &wait,
                Predicate::And(vec![
                    wait.column(models::waits::app_id)
                        .eq_column(child.column(models::runs::app_id))?,
                    wait.column(models::waits::child_id)
                        .eq_column(child.column(models::runs::id))?,
                ]),
            )?
            .filter(Predicate::And(vec![
                wait.column(models::waits::app_id).eq(self.app.as_str())?,
                wait.column(models::waits::run_id).eq(run_id)?,
                wait.column(models::waits::generation).eq(current)?,
                Predicate::Not(Box::new(Predicate::Or(vec![
                    child.column(models::runs::state).eq("completed")?,
                    child.column(models::runs::state).eq("failed")?,
                    child.column(models::runs::state).eq("cancelled")?,
                ]))),
            ]))
            .select(child.row::<models::KeyedRun>())?
            .limit(1)?
            .all()
            .await?;
        RestartSafety {
            live_lease: live != 0,
            active_descendants: active_descendants || !waiting_children.is_empty(),
            active_compensation: run.optional_text("compensation_target")?.is_some()
                && steps.iter().any(|step| {
                    step.compensation_state
                        .as_deref()
                        .is_some_and(|state| matches!(state, "pending" | "running"))
                }),
            compensated_prefix: retained.iter().any(|step| {
                step.compensation_state
                    .as_deref()
                    .is_some_and(|state| state != "pending")
            }),
        }
        .check()?;
        if retained.iter().any(|step| step.state == "running") {
            return Err(WorkflowServiceError::Conflict(
                "restart prefix contains unresolved operations".into(),
            ));
        }
        let deploy = if deploy_policy == RestartDeploy::Latest {
            let deploy = active_deploy(&mut tx, &self.app).await?;
            if !deploy.workflows.contains(&run.text("workflow_name")?) {
                return Err(WorkflowServiceError::Conflict(
                    "workflow is absent from the active deployment".into(),
                ));
            }
            deploy.id
        } else {
            run.text("deploy_id")?
        };
        let generation = current.checked_add(1).ok_or_else(|| {
            WorkflowServiceError::ResourceExhausted("workflow generation exhausted".into())
        })?;
        let signal_epoch = run.integer("signal_epoch")?.checked_add(1).ok_or_else(|| {
            WorkflowServiceError::ResourceExhausted("workflow signal epoch exhausted".into())
        })?;
        let previous = tx
            .database()
            .entity::<models::generations::Entity>()?
            .find::<models::GenerationInput>(
                models::generations::app_id
                    .eq(self.app.as_str())?
                    .and(models::generations::run_id.eq(run_id)?)
                    .and(models::generations::generation.eq(current)?),
                FindOptions {
                    limit: Some(1),
                    ..Default::default()
                },
            )
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| {
                WorkflowServiceError::Internal("workflow current generation is missing".into())
            })?;
        tasks.execute(Operation::Update {
            filter:value!({"app_id":self.app.as_str(), "run_id":run_id, "generation":current, "state":"leased"}),
            patch:value!({"state":"expired", "finished_at":now}), many:true,
        }).await?;
        for table in [
            models::waits::Entity::COLLECTION,
            models::subscriptions::Entity::COLLECTION,
        ] {
            tx.database().collection(table)?.execute(Operation::Purge {
                filter:value!({"app_id":self.app.as_str(), "run_id":run_id, "generation":current}), many:true,
            }).await?;
        }
        let signals = tx
            .database()
            .collection(models::signals::Entity::COLLECTION)?;
        for step in steps.iter().filter(|step| step.ordinal >= prefix) {
            if let Some(signal) = &step.consumed_signal_id {
                signals.execute(Operation::Purge {
                    filter:value!({"app_id":self.app.as_str(), "run_id":run_id, "id":signal.as_str(), "delivery":"topic"}), many:false,
                }).await?;
                signals.update(
                    value!({"app_id":self.app.as_str(), "run_id":run_id, "id":signal.as_str(), "delivery":"direct"}),
                    value!({"consumed_generation":null, "consumed_ordinal":null}),
                ).await?;
            }
        }
        signals
            .execute(Operation::Purge {
                filter: value!({"app_id":self.app.as_str(), "run_id":run_id, "delivery":"topic",
                "target_generation":current, "target_ordinal":{"$gte":i64::from(prefix)}}),
                many: true,
            })
            .await?;
        tx.database().collection(models::generations::Entity::COLLECTION)?.update(
            value!({"app_id":self.app.as_str(), "run_id":run_id, "generation":current, "terminal_at":null}),
            value!({"state":"restarted", "terminal_at":now}),
        ).await?;
        tx.database()
            .collection(models::generations::Entity::COLLECTION)?
            .insert(value!({
                "id":super::types::storage_id(), "app_id":self.app.as_str(), "run_id":run_id, "generation":generation,
                "deploy_id":deploy.clone(), "input":previous.input, "input_ref":previous.input_ref,
                "state":"queued", "started_at":now,
            }))
            .await?;
        replay::copy_prefix(&tx, &self.app, run_id, current, generation, prefix).await?;
        tx.database().collection(models::runs::Entity::COLLECTION)?.update(
            value!({"app_id":self.app.as_str(), "id":run_id}),
            value!({"generation":generation, "deploy_id":deploy.clone(), "state":"queued", "control":"none",
                "due_at":now, "task_id":null, "terminal_at":null, "compensation_target":null, "signal_epoch":signal_epoch}),
        ).await?;
        let result = RestartedRun {
            run_id: run_id.into(),
            state: RunState::Queued,
            restarted_from_ordinal: from.map(|value| value as u32),
            pinned_to: deploy,
        };
        emit(
            &mut tx,
            &self.app,
            &format!("{run_id}:{generation}:restart"),
            "workflow.restart",
            json!({"runId":run_id,"generation":generation}),
            now,
        )
        .await?;
        store_request(
            &mut tx,
            &self.app,
            request,
            "restart",
            &digest,
            &result,
            deadline(now, policy.request_retention_ms)?,
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }
}

/// Restart holds the app lock while inspecting descendants and changing the
/// generation. Exhaustion or corrupted ancestry cannot authorize a restart.
async fn has_active_descendants(
    tx: &Transaction,
    app: &AppId,
    root: &str,
) -> Result<bool, WorkflowServiceError> {
    let db = tx.database();
    let run = db.entity::<models::runs::Entity>()?.alias("r")?;
    let page_limit = RowLimit::default().get();
    let mut pending = vec![root.to_owned()];
    let mut inspected = BTreeSet::from([root.to_owned()]);
    while let Some(parent) = pending.pop() {
        let mut after: Option<String> = None;
        loop {
            let mut filter = vec![
                run.column(models::runs::app_id).eq(app.as_str())?,
                run.column(models::runs::parent_id)
                    .eq(Some(parent.as_str()))?,
            ];
            if let Some(after) = &after {
                filter.push(Predicate::compare(
                    Operand::Path(run.column(models::runs::id).asc().path),
                    CompareOp::Gt,
                    Operand::Lit(Literal::Text(after.clone())),
                ));
            }
            let page = db
                .from(&run)
                .filter(Predicate::And(filter))
                .order_by(run.column(models::runs::id).asc())
                .select(run.row::<models::KeyedRun>())?
                .limit(page_limit)?
                .all()
                .await?;
            let count = page.len();
            for descendant in page {
                if !inspected.insert(descendant.id.clone()) {
                    return Err(WorkflowServiceError::Internal(
                        "workflow descendants contain a cycle".into(),
                    ));
                }
                if inspected.len() > MAX_DESCENDANT_INSPECTIONS {
                    return Err(WorkflowServiceError::ResourceExhausted(
                        "workflow descendant inspection limit reached".into(),
                    ));
                }
                if !parse_state(&descendant.state)?.is_terminal() {
                    return Ok(true);
                }
                after = Some(descendant.id.clone());
                pending.push(descendant.id);
            }
            if count < page_limit as usize {
                break;
            }
        }
    }
    Ok(false)
}
