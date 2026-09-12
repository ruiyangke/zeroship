use super::super::{
    app::{active_deploy, emit, live_runs},
    journal,
};
use super::*;
use crate::{engine::StepCheckpoint, lifecycle::RestartSafety, operations::RestartDeploy};
use serde_json::json;
use std::collections::BTreeSet;
use zeroship_data_orm::{
    orm::{FindOptions, Operation, Output},
    sql::{CompareOp, Literal, Operand, Predicate, RowLimit},
};

const MAX_DESCENDANT_INSPECTIONS: usize = 16_384;

pub(in crate::service) struct RestartPlan {
    current: i64,
    generation: i64,
    signal_epoch: i64,
    steps: Vec<StepCheckpoint>,
    from: Option<i32>,
    deploy: String,
    previous: models::GenerationInput,
}

pub(in crate::service) async fn prepare(
    tx: &mut Transaction,
    app: &AppId,
    run_id: &str,
    options: &RestartOptions,
    policy: &AppPolicy,
    now: i64,
) -> Result<Preparation<RestartPlan>, WorkflowServiceError> {
    if policy.admit().is_err() {
        return Ok(Preparation::Rejected(Rejection::Denied));
    }
    let deploy_policy = match crate::lifecycle::restart_deploy_policy(options) {
        Ok(policy) => policy,
        Err(WorkflowServiceError::InvalidRequest(message)) => {
            return Ok(Preparation::Rejected(Rejection::Invalid(message)))
        }
        Err(WorkflowServiceError::Conflict(message)) => {
            return Ok(Preparation::Rejected(Rejection::Conflict(message)))
        }
        Err(error) => return Err(error),
    };
    let run = match lock_run(tx, app, run_id).await {
        Ok(run) => run,
        Err(WorkflowServiceError::NotFound(_)) => {
            return Ok(Preparation::Rejected(Rejection::NotFound))
        }
        Err(error) => return Err(error),
    };
    let current = run.integer("generation")?;
    let steps = journal::load(tx, app, run_id, current).await?;
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
            return Ok(Preparation::Rejected(Rejection::Invalid(
                "restart target is missing or ambiguous".into(),
            )));
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
        && live_runs(tx, app).await? >= policy.max_live_runs
    {
        return Err(WorkflowServiceError::ResourceExhausted(
            "workflow live-run limit reached".into(),
        ));
    }
    let Output::Count(live) = tasks
        .count(
            value!({"app_id":app.as_str(), "run_id":run_id, "generation":current,
                "state":"leased", "deadline":{"$gt":now}}),
            value!({}),
        )
        .await?
    else {
        return Err(WorkflowServiceError::Internal(
            "workflow count returned rows".into(),
        ));
    };
    let active_descendants = has_active_descendants(tx, app, run_id).await?;
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
            wait.column(models::waits::app_id).eq(app.as_str())?,
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
    let safety = RestartSafety {
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
    .check();
    if let Err(WorkflowServiceError::Conflict(message)) = safety {
        return Ok(Preparation::Rejected(Rejection::Conflict(message)));
    }
    safety?;
    if retained.iter().any(|step| step.state == "running") {
        return Ok(Preparation::Rejected(Rejection::Conflict(
            "restart prefix contains unresolved operations".into(),
        )));
    }
    let deploy = if deploy_policy == RestartDeploy::Latest {
        let deploy = active_deploy(tx, app).await?;
        if !deploy.workflows.contains(&run.text("workflow_name")?) {
            return Ok(Preparation::Rejected(Rejection::Conflict(
                "workflow is absent from the active deployment".into(),
            )));
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
                .eq(app.as_str())?
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

    Ok(Preparation::Ready(RestartPlan {
        current,
        generation,
        signal_epoch,
        steps,
        from,
        deploy,
        previous,
    }))
}

impl RestartPlan {
    pub(in crate::service) async fn apply(
        self,
        tx: &mut Transaction,
        app: &AppId,
        run_id: &str,
        now: i64,
    ) -> Result<RestartedRun, WorkflowServiceError> {
        let Self {
            current,
            generation,
            signal_epoch,
            steps,
            from,
            deploy,
            previous,
        } = self;
        let prefix = from.unwrap_or(0);
        let tasks = tx
            .database()
            .collection(models::tasks::Entity::COLLECTION)?;
        tasks.execute(Operation::Update {
            filter:value!({"app_id":app.as_str(), "run_id":run_id, "generation":current, "state":"leased"}),
            patch:value!({"state":"expired", "finished_at":now}), many:true,
        }).await?;
        for table in [
            models::waits::Entity::COLLECTION,
            models::subscriptions::Entity::COLLECTION,
        ] {
            tx.database()
                .collection(table)?
                .execute(Operation::Purge {
                    filter: value!({"app_id":app.as_str(), "run_id":run_id, "generation":current}),
                    many: true,
                })
                .await?;
        }
        let signals = tx
            .database()
            .collection(models::signals::Entity::COLLECTION)?;
        for step in steps.iter().filter(|step| step.ordinal >= prefix) {
            if let Some(signal) = &step.consumed_signal_id {
                signals.execute(Operation::Purge {
                    filter:value!({"app_id":app.as_str(), "run_id":run_id, "id":signal.as_str(), "delivery":"topic"}), many:false,
                }).await?;
                signals.update(
                    value!({"app_id":app.as_str(), "run_id":run_id, "id":signal.as_str(), "delivery":"direct"}),
                    value!({"consumed_generation":null, "consumed_ordinal":null}),
                ).await?;
            }
        }
        signals
            .execute(Operation::Purge {
                filter: value!({"app_id":app.as_str(), "run_id":run_id, "delivery":"topic",
                "target_generation":current, "target_ordinal":{"$gte":i64::from(prefix)}}),
                many: true,
            })
            .await?;
        tx.database().collection(models::generations::Entity::COLLECTION)?.update(
            value!({"app_id":app.as_str(), "run_id":run_id, "generation":current, "terminal_at":null}),
            value!({"state":"restarted", "terminal_at":now}),
        ).await?;
        tx.database()
            .collection(models::generations::Entity::COLLECTION)?
            .insert(value!({
                "id":super::super::types::storage_id(), "app_id":app.as_str(), "run_id":run_id, "generation":generation,
                "deploy_id":deploy.clone(), "input":previous.input, "input_ref":previous.input_ref,
                "state":"queued", "started_at":now,
            }))
            .await?;
        replay::copy_prefix(tx, app, run_id, current, generation, prefix).await?;
        tx.database().collection(models::runs::Entity::COLLECTION)?.update(
            value!({"app_id":app.as_str(), "id":run_id}),
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
            tx,
            app,
            &format!("{run_id}:{generation}:restart"),
            "workflow.restart",
            json!({"runId":run_id,"generation":generation}),
            now,
        )
        .await?;

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
