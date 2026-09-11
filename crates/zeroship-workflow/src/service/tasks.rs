use super::{
    app::{deadline, decode, encode, lock_app, lock_run, not_found, parse_state},
    frontier,
    store::{Row, Transaction},
    types::{
        digest, AppPolicy, CompletionReceipt, ControlIntent, Heartbeat, TaskAssignment, TaskToken,
        WorkerIdentity,
    },
    WorkflowService,
};
use crate::{WorkflowExecution, WorkflowServiceError};
use zeroship_core::{app_id::AppId, typed_id};

impl WorkflowService {
    /// Claim service-selected work for an authenticated worker with free capacity.
    pub async fn poll(
        &self,
        worker: &WorkerIdentity,
    ) -> Result<Option<TaskAssignment>, WorkflowServiceError> {
        let mut remaining = 128i64;
        while remaining > 0 {
            let mut tx = self.store.begin().await?;
            let now = tx.now().await?;
            let runs = tx.table("runs");
            let tasks = tx.table("tasks");
            let apps = tx.table("app_state");
            let candidates = tx.query(&format!("WITH candidates AS (SELECT app_id,id,due_at,ROW_NUMBER() OVER (PARTITION BY app_id ORDER BY due_at,id) AS position FROM {runs} WHERE due_at <= $1) SELECT c.app_id,c.id FROM candidates c JOIN {apps} a ON a.app_id=c.app_id WHERE c.position=1 ORDER BY a.last_polled_at,c.due_at,c.app_id LIMIT $2"), &[now.into(),remaining.into()]).await?;
            tx.commit().await?;
            if candidates.is_empty() {
                return Ok(None);
            }
            let mut advanced = false;
            for candidate in candidates {
                remaining -= 1;
                let app = AppId::parse(&candidate.text("app_id")?).map_err(|_| {
                    WorkflowServiceError::Internal("invalid persisted workflow app identity".into())
                })?;
                let id = candidate.text("id")?;
                let mut tx = self.store.begin().await?;
                let policy = lock_app(&mut tx, &app).await?;
                let mut run = lock_run(&mut tx, &app, &id).await?;
                let now = tx.now().await?;
                tx.execute(
                    &format!("UPDATE {apps} SET last_polled_at=$2 WHERE app_id=$1"),
                    &[app.as_str().into(), now.into()],
                )
                .await?;
                if run.optional_integer("due_at")?.is_none_or(|due| due > now)
                    || parse_state(&run.text("state")?)?.is_terminal()
                {
                    tx.commit().await?;
                    advanced = true;
                    continue;
                }
                if let Some(task) = run.optional_text("task_id")? {
                    let changed = tx.execute(&format!("UPDATE {tasks} SET state='expired',finished_at=$2 WHERE id=$1 AND state='leased' AND deadline <= $2"), &[task.into(),now.into()]).await?;
                    if changed != 1 {
                        return Err(WorkflowServiceError::Internal(
                            "workflow lease frontier disagrees with its task".into(),
                        ));
                    }
                    tx.execute(
                        &format!("UPDATE {runs} SET task_id=NULL WHERE app_id=$1 AND id=$2"),
                        &[app.as_str().into(), id.clone().into()],
                    )
                    .await?;
                    run = lock_run(&mut tx, &app, &id).await?;
                }
                if !frontier::prepare(&mut tx, &app, &run, now).await? {
                    advanced = true;
                    tx.commit().await?;
                    continue;
                }
                if !policy.admission || !policy.dispatch || policy.max_running == 0 {
                    tx.commit().await?;
                    continue;
                }
                let running = tx.query(&format!("SELECT COUNT(*) AS total FROM {tasks} WHERE app_id=$1 AND state='leased' AND deadline > $2"), &[app.as_str().into(),now.into()]).await?;
                if running[0].integer("total")? >= policy.max_running {
                    tx.commit().await?;
                    continue;
                }
                run = lock_run(&mut tx, &app, &id).await?;
                let invocation = frontier::invocation(&mut tx, &app, &run).await?;
                let token = TaskToken::mint();
                let task_id = typed_id::new_workflow_dispatch_id();
                let generation = run.integer("generation")?;
                let epoch = run.integer("lease_epoch")?.checked_add(1).ok_or_else(|| {
                    WorkflowServiceError::ResourceExhausted("workflow lease epoch exhausted".into())
                })?;
                let expires = deadline(now, policy.lease_ms)?;
                tx.execute(&format!("INSERT INTO {tasks} (id,app_id,run_id,generation,worker,epoch,token_hash,deadline,state,created_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,'leased',$9)"),
                &[task_id.clone().into(),app.as_str().into(),id.clone().into(),generation.into(),worker.as_str().into(),epoch.into(),token.hash().into(),expires.into(),now.into()]).await?;
                tx.execute(&format!("UPDATE {runs} SET task_id=$3,lease_epoch=$4,due_at=$5,state=CASE WHEN state='compensating' THEN state ELSE 'running' END WHERE app_id=$1 AND id=$2"),
                &[app.as_str().into(),id.into(),task_id.clone().into(),epoch.into(),expires.into()]).await?;
                tx.commit().await?;
                return Ok(Some(TaskAssignment {
                    id: task_id,
                    token,
                    generation,
                    epoch,
                    deadline: expires,
                    lease_ms: policy.lease_ms,
                    invocation,
                }));
            }
            if !advanced {
                break;
            }
        }
        Ok(None)
    }

    pub async fn heartbeat(
        &self,
        worker: &WorkerIdentity,
        task_id: &str,
        token: &TaskToken,
    ) -> Result<Heartbeat, WorkflowServiceError> {
        let mut tx = self.store.begin().await?;
        let claim = authorized_task(&mut tx, worker, task_id, token).await?;
        claim.validate_live()?;
        let expires = deadline(claim.now, claim.policy.lease_ms)?;
        let tasks = tx.table("tasks");
        let runs = tx.table("runs");
        tx.execute(
            &format!("UPDATE {tasks} SET deadline=$2 WHERE id=$1"),
            &[task_id.into(), expires.into()],
        )
        .await?;
        tx.execute(
            &format!("UPDATE {runs} SET due_at=$3 WHERE app_id=$1 AND id=$2"),
            &[
                claim.app.as_str().into(),
                claim.run.text("id")?.into(),
                expires.into(),
            ],
        )
        .await?;
        let control = ControlIntent::parse(&claim.run.text("control")?)?;
        tx.commit().await?;
        Ok(Heartbeat {
            deadline: expires,
            lease_ms: claim.policy.lease_ms,
            control,
        })
    }

    /// Commit a frontier and its retry receipt in the same transaction.
    pub async fn complete(
        &self,
        worker: &WorkerIdentity,
        task_id: &str,
        token: &TaskToken,
        execution: WorkflowExecution,
    ) -> Result<CompletionReceipt, WorkflowServiceError> {
        let body_digest = digest(&execution)?;
        let mut tx = self.store.begin().await?;
        let claim = authorized_task(&mut tx, worker, task_id, token).await?;
        if claim.task.text("state")? == "completed" {
            if claim.task.optional_text("completion_digest")?.as_deref() != Some(&body_digest) {
                return Err(WorkflowServiceError::Conflict(
                    "workflow task was completed with another result".into(),
                ));
            }
            let receipt = decode(&claim.task.text("receipt")?)?;
            tx.commit().await?;
            return Ok(receipt);
        }
        claim.validate_live()?;
        let state = frontier::apply(
            &mut tx,
            &claim.app,
            &claim.run,
            &claim.policy,
            execution,
            claim.now,
        )
        .await?;
        claim.validate_at(tx.now().await?)?;
        let receipt = CompletionReceipt {
            task_id: task_id.into(),
            app_id: claim.app.clone(),
            run_id: claim.run.text("id")?,
            generation: claim.task.integer("generation")?,
            state,
            committed_at: claim.now,
        };
        let tasks = tx.table("tasks");
        tx.execute(&format!("UPDATE {tasks} SET state='completed',completion_digest=$2,receipt=$3,finished_at=$4 WHERE id=$1"), &[task_id.into(),body_digest.into(),encode(&receipt)?.into(),claim.now.into()]).await?;
        tx.commit().await?;
        Ok(receipt)
    }

    /// Release only after the runner has stopped executing the assignment.
    pub async fn release(
        &self,
        worker: &WorkerIdentity,
        task_id: &str,
        token: &TaskToken,
    ) -> Result<(), WorkflowServiceError> {
        let mut tx = self.store.begin().await?;
        let claim = authorized_task(&mut tx, worker, task_id, token).await?;
        if claim.task.text("state")? == "released" {
            return Ok(());
        }
        claim.validate_live()?;
        let tasks = tx.table("tasks");
        let runs = tx.table("runs");
        tx.execute(
            &format!("UPDATE {tasks} SET state='released',finished_at=$2 WHERE id=$1"),
            &[task_id.into(), claim.now.into()],
        )
        .await?;
        tx.execute(
            &format!("UPDATE {runs} SET task_id=NULL,due_at=$3 WHERE app_id=$1 AND id=$2"),
            &[
                claim.app.as_str().into(),
                claim.run.text("id")?.into(),
                claim.now.into(),
            ],
        )
        .await?;
        tx.commit().await
    }
}

pub(crate) struct AuthorizedTask {
    pub(crate) app: AppId,
    pub(crate) policy: AppPolicy,
    pub(crate) task: Row,
    pub(crate) run: Row,
    pub(crate) now: i64,
}
impl AuthorizedTask {
    pub(crate) fn validate_live(&self) -> Result<(), WorkflowServiceError> {
        self.validate_at(self.now)
    }
    pub(crate) fn validate_at(&self, now: i64) -> Result<(), WorkflowServiceError> {
        if self.task.text("state")? != "leased"
            || self.task.integer("deadline")? <= now
            || self.run.optional_text("task_id")?.as_deref() != Some(self.task.text("id")?.as_str())
            || self.run.integer("generation")? != self.task.integer("generation")?
            || self.run.integer("lease_epoch")? != self.task.integer("epoch")?
        {
            return Err(WorkflowServiceError::Conflict(
                "workflow task lease is no longer current".into(),
            ));
        }
        Ok(())
    }
}
pub(crate) async fn authorized_task(
    tx: &mut Transaction,
    worker: &WorkerIdentity,
    task_id: &str,
    token: &TaskToken,
) -> Result<AuthorizedTask, WorkflowServiceError> {
    typed_id::parse_with_prefix(task_id, typed_id::WORKFLOW_DISPATCH_PREFIX)
        .map_err(|_| not_found("workflow task"))?;
    let tasks = tx.table("tasks");
    let lookup = format!("SELECT * FROM {tasks} WHERE id=$1 AND worker=$2 AND token_hash=$3");
    let params = [task_id.into(), worker.as_str().into(), token.hash().into()];
    let task = tx
        .query(&lookup, &params)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| not_found("workflow task"))?;
    let app = AppId::parse(&task.text("app_id")?).map_err(|_| {
        WorkflowServiceError::Internal("invalid persisted workflow app identity".into())
    })?;
    let policy = lock_app(tx, &app).await?;
    let run = lock_run(tx, &app, &task.text("run_id")?).await?;
    // The initial lookup establishes scope only. Re-read after taking the app
    // lock so concurrent completion, release and reclaim cannot evade fencing.
    let task = tx
        .query(&lookup, &params)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| not_found("workflow task"))?;
    let now = tx.now().await?;
    Ok(AuthorizedTask {
        app,
        policy,
        task,
        run,
        now,
    })
}
