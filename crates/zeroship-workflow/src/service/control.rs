use super::{
    app::{
        active_deploy, deadline, emit, live_runs, lock_app, lock_run, parse_state, request_result,
        store_request, validate_run,
    },
    frontier, journal, models,
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
use zeroship_data_orm::{
    orm::{Entity, Operation, Output},
    sql::Predicate,
    value,
};

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
        let runs = tx.table("runs");
        if parse_state(&run.text("state")?)?.is_terminal() {
            if live_runs(&tx, &self.app).await? >= policy.max_live_runs {
                return Err(WorkflowServiceError::ResourceExhausted(
                    "workflow live-run limit reached".into(),
                ));
            }
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
        let descendants=tx.query(&format!("WITH RECURSIVE descendants AS (SELECT id,state FROM {runs} WHERE app_id=$1 AND parent_id=$2 UNION ALL SELECT r.id,r.state FROM {runs} r JOIN descendants d ON r.parent_id=d.id WHERE r.app_id=$1) SELECT id FROM descendants WHERE state NOT IN ('completed','failed','cancelled')"), &[self.app.as_str().into(),run_id.into()]).await?;
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
            active_descendants: !descendants.is_empty() || !waiting_children.is_empty(),
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
        let generations = tx.table("generations");
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
        tx.execute(&format!("INSERT INTO {generations} (app_id,run_id,generation,deploy_id,input,input_ref,state,started_at) SELECT app_id,run_id,$4,$5,input,input_ref,'queued',$6 FROM {generations} WHERE app_id=$1 AND run_id=$2 AND generation=$3"),
            &[self.app.as_str().into(),run_id.into(),current.into(),generation.into(),deploy.clone().into(),now.into()]).await?;
        let steps_table = tx.table("steps");
        tx.execute(&format!("INSERT INTO {steps_table} (app_id,run_id,generation,ordinal,name,occurrence,origin_generation,kind,state,record,compensation_attempts,compensation_due_at,compensation_error,compensation_retry_ms) SELECT app_id,run_id,$4,ordinal,name,occurrence,origin_generation,kind,state,record,compensation_attempts,compensation_due_at,compensation_error,compensation_retry_ms FROM {steps_table} WHERE app_id=$1 AND run_id=$2 AND generation=$3 AND ordinal<$5"),
            &[self.app.as_str().into(),run_id.into(),current.into(),generation.into(),i64::from(prefix).into()]).await?;
        let refs = tx.table("payload_refs");
        tx.execute(&format!("INSERT INTO {refs} (app_id,run_id,generation,slot,ordinal,payload_id) SELECT app_id,run_id,$4,slot,ordinal,payload_id FROM {refs} WHERE app_id=$1 AND run_id=$2 AND generation=$3 AND (slot='input' OR (slot='step' AND ordinal<$5))"), &[self.app.as_str().into(),run_id.into(),current.into(),generation.into(),i64::from(prefix).into()]).await?;
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
