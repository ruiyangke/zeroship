use super::{
    app::{
        active_deploy, deadline, emit, lock_app, lock_run, parse_state, request_result,
        store_request, validate_run,
    },
    frontier, journal,
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

impl AppWorkflows {
    pub async fn transition(
        &self,
        request: &RequestId,
        run_id: &str,
        operation: RunOperation,
    ) -> Result<TransitionedRun, WorkflowServiceError> {
        validate_run(run_id)?;
        let digest = digest(&(run_id, operation))?;
        let mut tx = self.service.store.begin().await?;
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
            let runs = tx.table("runs");
            let leased = run.optional_text("task_id")?.is_some();
            match operation {
                RunOperation::Pause => {
                    tx.execute(
                        &format!("UPDATE {runs} SET control='pause' WHERE app_id=$1 AND id=$2"),
                        &[self.app.as_str().into(), run_id.into()],
                    )
                    .await?;
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
                    tx.execute(&format!("UPDATE {runs} SET control='none',state=CASE WHEN task_id IS NULL THEN $3 ELSE state END,due_at=CASE WHEN task_id IS NULL THEN $4 ELSE due_at END WHERE app_id=$1 AND id=$2"),
                        &[self.app.as_str().into(),run_id.into(),phase.into(),now.into()]).await?;
                    if !leased {
                        state = parse_state(phase)?;
                    }
                }
                RunOperation::Cancel => {
                    tx.execute(&format!("UPDATE {runs} SET control='cancel',due_at=CASE WHEN task_id IS NULL THEN $3 ELSE due_at END WHERE app_id=$1 AND id=$2"),
                        &[self.app.as_str().into(),run_id.into(),now.into()]).await?;
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
        let deploy_policy = options.deploy_policy()?;
        let digest = digest(&(run_id, &options))?;
        let mut tx = self.service.store.begin().await?;
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
        let tasks = tx.table("tasks");
        let runs = tx.table("runs");
        if parse_state(&run.text("state")?)?.is_terminal() {
            let live = tx.query(&format!("SELECT COUNT(*) AS total FROM {runs} WHERE app_id=$1 AND state NOT IN ('completed','failed','cancelled')"), &[self.app.as_str().into()]).await?;
            if live[0].integer("total")? >= policy.max_live_runs {
                return Err(WorkflowServiceError::ResourceExhausted(
                    "workflow live-run limit reached".into(),
                ));
            }
        }
        let live=tx.query(&format!("SELECT id FROM {tasks} WHERE app_id=$1 AND run_id=$2 AND generation=$3 AND state='leased' AND deadline>$4"),
            &[self.app.as_str().into(),run_id.into(),current.into(),now.into()]).await?;
        let descendants=tx.query(&format!("WITH RECURSIVE descendants AS (SELECT id,state FROM {runs} WHERE app_id=$1 AND parent_id=$2 UNION ALL SELECT r.id,r.state FROM {runs} r JOIN descendants d ON r.parent_id=d.id WHERE r.app_id=$1) SELECT id FROM descendants WHERE state NOT IN ('completed','failed','cancelled')"), &[self.app.as_str().into(),run_id.into()]).await?;
        let waits = tx.table("waits");
        let waiting_children=tx.query(&format!("SELECT r.id FROM {runs} r JOIN {waits} w ON w.app_id=r.app_id AND w.child_id=r.id WHERE w.app_id=$1 AND w.run_id=$2 AND w.generation=$3 AND r.state NOT IN ('completed','failed','cancelled')"), &[self.app.as_str().into(),run_id.into(),current.into()]).await?;
        RestartSafety {
            live_lease: !live.is_empty(),
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
        tx.execute(&format!("UPDATE {tasks} SET state='expired',finished_at=$4 WHERE app_id=$1 AND run_id=$2 AND generation=$3 AND state='leased'"), &[self.app.as_str().into(),run_id.into(),current.into(),now.into()]).await?;
        for table in ["waits", "subscriptions"] {
            let table = tx.table(table);
            tx.execute(
                &format!("DELETE FROM {table} WHERE app_id=$1 AND run_id=$2 AND generation=$3"),
                &[self.app.as_str().into(), run_id.into(), current.into()],
            )
            .await?;
        }
        let signals = tx.table("signals");
        for step in steps.iter().filter(|step| step.ordinal >= prefix) {
            if let Some(signal) = &step.consumed_signal_id {
                tx.execute(&format!("DELETE FROM {signals} WHERE app_id=$1 AND run_id=$2 AND id=$3 AND delivery='topic'"), &[self.app.as_str().into(),run_id.into(),signal.clone().into()]).await?;
                tx.execute(&format!("UPDATE {signals} SET consumed_generation=NULL,consumed_ordinal=NULL WHERE app_id=$1 AND run_id=$2 AND id=$3 AND delivery='direct'"), &[self.app.as_str().into(),run_id.into(),signal.clone().into()]).await?;
            }
        }
        tx.execute(&format!("DELETE FROM {signals} WHERE app_id=$1 AND run_id=$2 AND delivery='topic' AND target_generation=$3 AND target_ordinal >= $4"), &[self.app.as_str().into(),run_id.into(),current.into(),i64::from(prefix).into()]).await?;
        tx.execute(&format!("UPDATE {generations} SET state=CASE WHEN terminal_at IS NULL THEN 'restarted' ELSE state END,terminal_at=COALESCE(terminal_at,$4) WHERE app_id=$1 AND run_id=$2 AND generation=$3"), &[self.app.as_str().into(),run_id.into(),current.into(),now.into()]).await?;
        tx.execute(&format!("INSERT INTO {generations} (app_id,run_id,generation,deploy_id,input,state,started_at) SELECT app_id,run_id,$4,$5,input,'queued',$6 FROM {generations} WHERE app_id=$1 AND run_id=$2 AND generation=$3"),
            &[self.app.as_str().into(),run_id.into(),current.into(),generation.into(),deploy.clone().into(),now.into()]).await?;
        let steps_table = tx.table("steps");
        tx.execute(&format!("INSERT INTO {steps_table} (app_id,run_id,generation,ordinal,name,occurrence,origin_generation,kind,state,record,compensation_attempts,compensation_due_at,compensation_error,compensation_retry_ms) SELECT app_id,run_id,$4,ordinal,name,occurrence,origin_generation,kind,state,record,compensation_attempts,compensation_due_at,compensation_error,compensation_retry_ms FROM {steps_table} WHERE app_id=$1 AND run_id=$2 AND generation=$3 AND ordinal<$5"),
            &[self.app.as_str().into(),run_id.into(),current.into(),generation.into(),i64::from(prefix).into()]).await?;
        tx.execute(&format!("UPDATE {runs} SET generation=$3,deploy_id=$4,state='queued',control='none',due_at=$5,task_id=NULL,terminal_at=NULL,compensation_target=NULL,signal_epoch=$6 WHERE app_id=$1 AND id=$2"),
            &[self.app.as_str().into(),run_id.into(),generation.into(),deploy.clone().into(),now.into(),signal_epoch.into()]).await?;
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
