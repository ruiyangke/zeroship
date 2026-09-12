use super::{
    app::{deadline, decode, encode, lock_app, lock_run, not_found, parse_state},
    frontier, models,
    store::{Row, Transaction},
    types::{
        digest, AppPolicy, CompletionReceipt, ControlIntent, Heartbeat, TaskAssignment, TaskToken,
        WorkerIdentity,
    },
    WorkflowService,
};
use crate::{WorkflowExecution, WorkflowServiceError};
use zeroship_core::{app_id::AppId, typed_id};
use zeroship_data_orm::{
    orm::{Entity, FindOptions, FromRow, Operation, Output},
    sql::{CompareOp, Literal, Operand, Predicate},
    value, Value,
};

#[derive(FromRow)]
#[orm(entity = models::runs)]
struct DueRun {
    id: String,
    due_at: Option<i64>,
}

#[derive(FromRow)]
#[orm(entity = models::app_state)]
struct PollOrder {
    last_polled_at: i64,
}

impl WorkflowService {
    /// Claim service-selected work for an authenticated worker with free capacity.
    pub async fn poll(
        &self,
        worker: &WorkerIdentity,
    ) -> Result<Option<TaskAssignment>, WorkflowServiceError> {
        let mut remaining = 128i64;
        while remaining > 0 {
            let mut tx = self.begin().await?;
            let now = tx.now().await?;
            let db = tx.database();
            let run = db.entity::<models::runs::Entity>()?.alias("r")?;
            let app_state = db.entity::<models::app_state::Entity>()?.alias("a")?;
            let deploy = db.entity::<models::deploys::Entity>()?.alias("d")?;
            let mut candidates = Vec::new();
            // Select each assigned app's first eligible run before applying the
            // global fairness order. Unavailable code never fills the frontier.
            for app in tx.host_app_ids()? {
                let candidate = db
                    .from(&run)
                    .inner_join(
                        &app_state,
                        run.column(models::runs::app_id)
                            .eq_column(app_state.column(models::app_state::app_id))?,
                    )?
                    .left_join(
                        &deploy,
                        Predicate::And(vec![
                            run.column(models::runs::app_id)
                                .eq_column(deploy.column(models::deploys::app_id))?,
                            run.column(models::runs::deploy_id)
                                .eq_column(deploy.column(models::deploys::id))?,
                            deploy.column(models::deploys::state).eq("available")?,
                        ]),
                    )?
                    .filter(Predicate::And(vec![
                        run.column(models::runs::app_id).eq(app.as_str())?,
                        Predicate::compare(
                            Operand::Path(run.column(models::runs::due_at).asc().path),
                            CompareOp::Lte,
                            Operand::Lit(Literal::Int(now)),
                        ),
                        Predicate::Or(vec![
                            Predicate::is_not_null(Operand::Path(
                                run.column(models::runs::task_id).asc().path,
                            )),
                            Predicate::Not(Box::new(run.column(models::runs::control).eq("none")?)),
                            Predicate::is_not_null(Operand::Path(
                                deploy.column(models::deploys::id).asc().path,
                            )),
                        ]),
                    ]))
                    .order_by(run.column(models::runs::due_at).asc())
                    .order_by(run.column(models::runs::id).asc())
                    .select((run.row::<DueRun>(), app_state.row::<PollOrder>()))?
                    .limit(1)?
                    .all()
                    .await?;
                if let Some((run, order)) = candidate.into_iter().next() {
                    candidates.push((order.last_polled_at, run.due_at, app, run.id));
                    candidates.sort_by(|left, right| {
                        (&left.0, &left.1, left.2.as_str()).cmp(&(
                            &right.0,
                            &right.1,
                            right.2.as_str(),
                        ))
                    });
                    candidates.truncate(remaining as usize);
                }
            }
            tx.commit().await?;
            if candidates.is_empty() {
                return Ok(None);
            }
            let mut advanced = false;
            for (_, _, app, id) in candidates {
                remaining -= 1;
                let mut tx = self.begin().await?;
                let policy = lock_app(&mut tx, &app).await?;
                let mut run = lock_run(&mut tx, &app, &id).await?;
                let now = tx.now().await?;
                let task_rows = tx
                    .database()
                    .collection(models::tasks::Entity::COLLECTION)?;
                let run_rows = tx.database().collection(models::runs::Entity::COLLECTION)?;
                tx.database()
                    .collection(models::app_state::Entity::COLLECTION)?
                    .update(
                        value!({"app_id":app.as_str()}),
                        value!({"last_polled_at":now}),
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
                    let changed = task_rows.execute(Operation::Update {
                        filter:value!({"id":task, "app_id":app.as_str(), "run_id":id.clone(),
                            "generation":run.integer("generation")?, "epoch":run.integer("lease_epoch")?,
                            "state":"leased", "deadline":{"$lte":now}}),
                        patch:value!({"state":"expired", "finished_at":now}), many:true,
                    }).await?;
                    if !matches!(changed, Output::Count(1)) {
                        return Err(WorkflowServiceError::Internal(
                            "workflow lease frontier disagrees with its task".into(),
                        ));
                    }
                    run_rows
                        .update(
                            value!({"app_id":app.as_str(), "id":id.clone()}),
                            value!({"task_id":null}),
                        )
                        .await?;
                    run = lock_run(&mut tx, &app, &id).await?;
                    advanced = true;
                }
                if !frontier::prepare(&mut tx, &app, &run, now).await? {
                    advanced = true;
                    tx.commit().await?;
                    continue;
                }
                // Missing code blocks execution, not lease recovery or control.
                // Recheck under the app lock after any deployment-state race.
                let available = tx
                    .database()
                    .entity::<models::deploys::Entity>()?
                    .find::<models::DeploymentHash>(
                        models::deploys::app_id
                            .eq(app.as_str())?
                            .and(models::deploys::id.eq(run.text("deploy_id")?)?)
                            .and(models::deploys::state.eq("available")?),
                        FindOptions {
                            limit: Some(1),
                            ..Default::default()
                        },
                    )
                    .await?;
                if available.is_empty() {
                    tx.commit().await?;
                    advanced = true;
                    continue;
                }
                if !policy.admission || !policy.dispatch || policy.max_running == 0 {
                    tx.commit().await?;
                    continue;
                }
                let Output::Count(running) = task_rows
                    .count(
                        value!({"app_id":app.as_str(), "state":"leased", "deadline":{"$gt":now}}),
                        value!({}),
                    )
                    .await?
                else {
                    return Err(WorkflowServiceError::Internal(
                        "workflow count returned rows".into(),
                    ));
                };
                if running >= policy.max_running {
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
                task_rows.insert(value!({
                    "id":task_id.clone(), "app_id":app.as_str(), "run_id":id.clone(), "generation":generation,
                    "worker":worker.as_str(), "epoch":epoch, "token_hash":token.hash(), "deadline":expires,
                    "state":"leased", "created_at":now,
                })).await?;
                let state = if run.text("state")? == "compensating" {
                    "compensating"
                } else {
                    "running"
                };
                let claimed = run_rows.execute(Operation::Update {
                    filter:value!({"app_id":app.as_str(), "id":id, "generation":generation,
                        "lease_epoch":run.integer("lease_epoch")?, "task_id":null}),
                    patch:value!({"task_id":task_id.clone(), "lease_epoch":epoch, "due_at":expires, "state":state}),
                    many:true,
                }).await?;
                if !matches!(claimed, Output::Count(1)) {
                    return Err(stale_lease());
                }
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
        let mut tx = self.begin().await?;
        let claim = authorized_task(&mut tx, worker, task_id, token).await?;
        claim.validate_live()?;
        if !claim.policy.admission || !claim.policy.dispatch {
            let deadline = claim.task.deadline;
            let control = match ControlIntent::parse(&claim.run.text("control")?)? {
                ControlIntent::None => ControlIntent::Pause,
                requested => requested,
            };
            tx.commit().await?;
            return Ok(Heartbeat {
                deadline,
                lease_ms: deadline - claim.now,
                control,
            });
        }
        let expires = deadline(claim.now, claim.policy.lease_ms)?;
        claim.update_task(&tx, value!({"deadline":expires})).await?;
        claim.update_run(&tx, value!({"due_at":expires})).await?;
        claim.validate_at(tx.now().await?)?;
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
        let mut tx = self.begin().await?;
        let claim = authorized_task(&mut tx, worker, task_id, token).await?;
        if claim.task.state == "completed" {
            if claim.task.completion_digest.as_deref() != Some(&body_digest) {
                return Err(WorkflowServiceError::Conflict(
                    "workflow task was completed with another result".into(),
                ));
            }
            let receipt = decode(claim.task.receipt.as_deref().ok_or_else(|| {
                WorkflowServiceError::Internal("completed workflow task has no receipt".into())
            })?)?;
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
            generation: claim.task.generation,
            state,
            committed_at: claim.now,
        };
        claim
            .update_task(
                &tx,
                value!({"state":"completed", "completion_digest":body_digest,
            "receipt":encode(&receipt)?, "finished_at":claim.now}),
            )
            .await?;
        claim.validate_at(tx.now().await?)?;
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
        let mut tx = self.begin().await?;
        let claim = authorized_task(&mut tx, worker, task_id, token).await?;
        if claim.task.state == "released" {
            return Ok(());
        }
        claim.validate_live()?;
        claim
            .update_task(&tx, value!({"state":"released", "finished_at":claim.now}))
            .await?;
        claim
            .update_run(&tx, value!({"task_id":null, "due_at":claim.now}))
            .await?;
        tx.commit().await
    }
}

pub(crate) struct AuthorizedTask {
    pub(crate) app: AppId,
    pub(crate) policy: AppPolicy,
    pub(crate) task: models::TaskRecord,
    pub(crate) run: Row,
    pub(crate) now: i64,
}
impl AuthorizedTask {
    pub(crate) fn validate_live(&self) -> Result<(), WorkflowServiceError> {
        self.validate_at(self.now)
    }
    /// Recheck after awaited writes: the final mutation can itself wait past
    /// the deadline, even while this transaction holds the app and run locks.
    pub(crate) fn validate_at(&self, now: i64) -> Result<(), WorkflowServiceError> {
        if self.task.state != "leased"
            || self.task.deadline <= now
            || self.run.optional_text("task_id")?.as_deref() != Some(self.task.id.as_str())
            || self.run.integer("generation")? != self.task.generation
            || self.run.integer("lease_epoch")? != self.task.epoch
            || self.run.text("id")? != self.task.run_id
            || self.app.as_str() != self.task.app_id
        {
            return Err(stale_lease());
        }
        Ok(())
    }

    async fn update_task(
        &self,
        tx: &Transaction,
        patch: Value,
    ) -> Result<(), WorkflowServiceError> {
        let changed = tx.database().collection(models::tasks::Entity::COLLECTION)?.execute(Operation::Update {
            filter:value!({"id":self.task.id.clone(), "app_id":self.app.as_str(), "run_id":self.task.run_id.clone(),
                "generation":self.task.generation, "epoch":self.task.epoch, "deadline":self.task.deadline, "state":"leased"}),
            patch, many:true,
        }).await?;
        if !matches!(changed, Output::Count(1)) {
            return Err(stale_lease());
        }
        Ok(())
    }

    async fn update_run(&self, tx: &Transaction, patch: Value) -> Result<(), WorkflowServiceError> {
        let changed = tx.database().collection(models::runs::Entity::COLLECTION)?.execute(Operation::Update {
            filter:value!({"app_id":self.app.as_str(), "id":self.task.run_id.clone(),
                "generation":self.task.generation, "lease_epoch":self.task.epoch, "task_id":self.task.id.clone()}),
            patch, many:true,
        }).await?;
        if !matches!(changed, Output::Count(1)) {
            return Err(stale_lease());
        }
        Ok(())
    }
}

fn stale_lease() -> WorkflowServiceError {
    WorkflowServiceError::Conflict("workflow task lease is no longer current".into())
}
pub(crate) async fn authorized_task(
    tx: &mut Transaction,
    worker: &WorkerIdentity,
    task_id: &str,
    token: &TaskToken,
) -> Result<AuthorizedTask, WorkflowServiceError> {
    typed_id::parse_with_prefix(task_id, typed_id::WORKFLOW_DISPATCH_PREFIX)
        .map_err(|_| not_found("workflow task"))?;
    let tasks = tx.database().entity::<models::tasks::Entity>()?;
    let lookup = || -> Result<_, WorkflowServiceError> {
        Ok(models::tasks::id
            .eq(task_id)?
            .and(models::tasks::worker.eq(worker.as_str())?)
            .and(models::tasks::token_hash.eq(token.hash())?))
    };
    let options = || FindOptions {
        limit: Some(1),
        ..Default::default()
    };
    let source = tasks.alias("t")?;
    let initial = tx
        .database()
        .from(&source)
        .filter(Predicate::And(vec![
            Predicate::Or(
                tx.host_app_ids()?
                    .into_iter()
                    .map(|app| source.column(models::tasks::app_id).eq(app.as_str()))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            source.column(models::tasks::id).eq(task_id)?,
            source.column(models::tasks::worker).eq(worker.as_str())?,
            source.column(models::tasks::token_hash).eq(token.hash())?,
        ]))
        .select(source.row::<models::TaskRecord>())?
        .limit(1)?
        .all()
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| not_found("workflow task"))?;
    let app = AppId::parse(&initial.app_id).map_err(|_| {
        WorkflowServiceError::Internal("invalid persisted workflow app identity".into())
    })?;
    let policy = lock_app(tx, &app).await?;
    let run = lock_run(tx, &app, &initial.run_id).await?;
    // The initial lookup establishes scope only. Re-read after taking the app
    // lock so concurrent completion, release and reclaim cannot evade fencing.
    let task = tasks
        .find::<models::TaskRecord>(
            lookup()?
                .and(models::tasks::app_id.eq(app.as_str())?)
                .and(models::tasks::run_id.eq(initial.run_id)?)
                .and(models::tasks::generation.eq(initial.generation)?)
                .and(models::tasks::epoch.eq(initial.epoch)?),
            options(),
        )
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
