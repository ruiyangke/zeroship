use super::{
    app::{deadline, decode, encode, lock_app, lock_app_state, lock_run, not_found, parse_state},
    frontier, models,
    store::{Row, Transaction},
    types::{
        digest, CompletionReceipt, ControlIntent, Heartbeat, TaskAssignment, TaskToken,
        WorkerIdentity,
    },
    AppPolicy, WorkflowService,
};
use crate::{WorkflowExecution, WorkflowServiceError};
use zeroship_core::{app_id::AppId, typed_id};
use zeroship_data_orm::{
    orm::{Entity, FindOptions, FromRow, Operation, Output},
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
        self.run_bound(|service| Box::pin(async move { service.poll_inner(worker).await }))
            .await
    }

    async fn poll_inner(
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
            let delivered = db.entity::<models::job_receipts::Entity>()?.alias("j")?;
            let mut candidates = Vec::new();
            // Select each assigned app's first eligible run before applying the
            // global fairness order. Unavailable code never fills the frontier.
            for app in tx.host_app_ids()? {
                let candidate = db
                    .from(&run)
                    .inner_join(
                        &app_state,
                        run.column(models::runs::app_id)
                            .eq(app_state.column(models::app_state::app_id))?,
                    )?
                    .left_join(
                        &deploy,
                        run.column(models::runs::app_id)
                            .eq(deploy.column(models::deploys::app_id))?
                            .and(
                                run.column(models::runs::deploy_id)
                                    .eq(deploy.column(models::deploys::id))?,
                            )
                            .and(deploy.column(models::deploys::state).eq("available")?),
                    )?
                    .left_join(
                        &delivered,
                        run.column(models::runs::app_id)
                            .eq(delivered.column(models::job_receipts::app_id))?
                            .and(
                                run.column(models::runs::id)
                                    .eq(delivered.column(models::job_receipts::run_id))?,
                            ),
                    )?
                    .filter(
                        delivered
                            .column(models::job_receipts::id)
                            .is_null()
                            .and(run.column(models::runs::app_id).eq(app.as_str())?)
                            .and(run.column(models::runs::due_at).lte(Some(now))?)
                            .and(
                                run.column(models::runs::task_id)
                                    .is_not_null()
                                    .or(run.column(models::runs::control).eq("none")?.negate())
                                    .or(deploy.column(models::deploys::id).is_not_null()),
                            ),
                    )
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
                let (_, policy) = lock_app(&mut tx, &app).await?;
                // Acceptance can take ownership after candidate selection.
                // Recheck under the same app lock before touching its frontier.
                let delivered = tx
                    .database()
                    .collection(models::job_receipts::Entity::COLLECTION)?
                    .count(
                        value!({"app_id":app.as_str(), "run_id":id.clone()}),
                        value!({}),
                    )
                    .await?;
                if !matches!(delivered, Output::Count(0)) {
                    tx.commit().await?;
                    advanced = true;
                    continue;
                }
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
                if !frontier::prepare(&mut tx, &app, &run, &policy, now).await? {
                    super::publication::advance(&tx, &app, &id, now).await?;
                    advanced = true;
                    tx.commit().await?;
                    continue;
                }
                match assign(&mut tx, &app, &run, &policy, worker, now, policy.lease_ms).await? {
                    ReadyClaim::Task(task) => {
                        tx.commit().await?;
                        return Ok(Some(*task));
                    }
                    ReadyClaim::Unavailable => advanced = true,
                    ReadyClaim::Busy => {}
                }
                tx.commit().await?;
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
        self.run_bound(|service| {
            Box::pin(async move { service.heartbeat_inner(worker, task_id, token).await })
        })
        .await
    }

    async fn heartbeat_inner(
        &self,
        worker: &WorkerIdentity,
        task_id: &str,
        token: &TaskToken,
    ) -> Result<Heartbeat, WorkflowServiceError> {
        let mut tx = self.begin().await?;
        let claim = authorized_task(&mut tx, worker, task_id, token).await?;
        if claim.task.job_id.is_some() {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        claim.validate_live()?;
        let control = super::propagation::effective_control(&tx, &claim.app, &claim.run).await?;
        if !claim.policy.admission || !claim.policy.dispatch {
            let deadline = claim.task.deadline;
            let control = match control {
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
        self.run_bound(|service| {
            Box::pin(async move {
                service
                    .complete_inner(worker, task_id, token, execution)
                    .await
            })
        })
        .await
    }

    async fn complete_inner(
        &self,
        worker: &WorkerIdentity,
        task_id: &str,
        token: &TaskToken,
        execution: WorkflowExecution,
    ) -> Result<CompletionReceipt, WorkflowServiceError> {
        let body_digest = digest(&execution)?;
        let mut tx = self.begin().await?;
        let claim = inspect_task(&mut tx, worker, task_id, token).await?;
        if claim.task.job_id.is_some() {
            return Err(WorkflowServiceError::PermissionDenied);
        }
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
        let claim = claim.authorize(&mut tx)?;
        let receipt = complete_in(&mut tx, &claim, execution, &body_digest).await?;
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
        self.run_bound(|service| {
            Box::pin(async move { service.release_inner(worker, task_id, token).await })
        })
        .await
    }

    async fn release_inner(
        &self,
        worker: &WorkerIdentity,
        task_id: &str,
        token: &TaskToken,
    ) -> Result<(), WorkflowServiceError> {
        let mut tx = self.begin().await?;
        let claim = inspect_task(&mut tx, worker, task_id, token).await?;
        if claim.task.job_id.is_some() {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        if claim.task.state == "released" {
            return Ok(());
        }
        claim.validate_live()?;
        let claim = claim.authorize(&mut tx)?;
        claim
            .update_task(&tx, value!({"state":"released", "finished_at":claim.now}))
            .await?;
        claim
            .update_run(&tx, value!({"task_id":null, "due_at":claim.now}))
            .await?;
        super::publication::record(&tx, &claim.app, &claim.run.text("id")?, claim.now).await?;
        tx.commit().await
    }
}

pub(crate) struct AuthorizedTask {
    inspection: TaskInspection,
    pub(crate) policy: AppPolicy,
}
impl std::ops::Deref for AuthorizedTask {
    type Target = TaskInspection;
    fn deref(&self) -> &Self::Target {
        &self.inspection
    }
}

/// Task identity and retained settlement can be inspected without renewed policy.
pub struct TaskInspection {
    pub(crate) app: AppId,
    pub(crate) task: models::TaskRecord,
    pub(crate) run: Row,
    pub(crate) now: i64,
}
impl TaskInspection {
    pub(crate) fn authorize(
        self,
        tx: &mut Transaction,
    ) -> Result<AuthorizedTask, WorkflowServiceError> {
        tx.capture_mutation(&self.app)?;
        let policy = tx.policy(&self.app)?;
        Ok(AuthorizedTask {
            inspection: self,
            policy,
        })
    }
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
            || self.run.integer("frontier_revision")? != self.task.frontier_revision
            || self.run.text("id")? != self.task.run_id
            || self.app.as_str() != self.task.app_id
        {
            return Err(stale_lease());
        }
        Ok(())
    }

    pub(super) async fn update_task(
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

    pub(super) async fn update_run(
        &self,
        tx: &Transaction,
        patch: Value,
    ) -> Result<(), WorkflowServiceError> {
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
    inspect_task(tx, worker, task_id, token)
        .await?
        .authorize(tx)
}

#[expect(
    clippy::future_not_send,
    reason = "task inspection uses its creator transaction thread"
)]
pub(super) async fn inspect_task(
    tx: &mut Transaction,
    worker: &WorkerIdentity,
    task_id: &str,
    token: &TaskToken,
) -> Result<TaskInspection, WorkflowServiceError> {
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
        .filter(
            source
                .column(models::tasks::app_id)
                .in_values(
                    tx.host_app_ids()?
                        .into_iter()
                        .map(|app| app.as_str().to_owned()),
                )?
                .and(source.column(models::tasks::id).eq(task_id)?)
                .and(source.column(models::tasks::worker).eq(worker.as_str())?)
                .and(source.column(models::tasks::token_hash).eq(token.hash())?),
        )
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
    lock_app_state(tx, &app).await?;
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
    Ok(TaskInspection {
        app,
        task,
        run,
        now,
    })
}

/// Shared exact-run executor admission; the caller owns the app/run locks.
pub(super) enum ReadyClaim {
    Task(Box<TaskAssignment>),
    Unavailable,
    Busy,
}

pub(super) async fn assign(
    tx: &mut Transaction,
    app: &AppId,
    run: &Row,
    policy: &AppPolicy,
    worker: &WorkerIdentity,
    now: i64,
    lease_ms: i64,
) -> Result<ReadyClaim, WorkflowServiceError> {
    let id = run.text("id")?;
    let task_rows = tx
        .database()
        .collection(models::tasks::Entity::COLLECTION)?;
    let run_rows = tx.database().collection(models::runs::Entity::COLLECTION)?;
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
        return Ok(ReadyClaim::Unavailable);
    }
    if !policy.admission || !policy.dispatch || policy.max_running == 0 {
        return Ok(ReadyClaim::Busy);
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
        return Ok(ReadyClaim::Busy);
    }
    let run = lock_run(tx, app, &id).await?;
    let invocation = frontier::invocation(tx, app, &run).await?;
    let token = TaskToken::mint();
    let task_id = typed_id::new_workflow_dispatch_id();
    let generation = run.integer("generation")?;
    let epoch = run.integer("lease_epoch")?.checked_add(1).ok_or_else(|| {
        WorkflowServiceError::ResourceExhausted("workflow lease epoch exhausted".into())
    })?;
    let lease_ms = lease_ms.min(policy.lease_ms);
    let expires = deadline(now, lease_ms)?;
    task_rows.insert(value!({
        "id":task_id.clone(), "app_id":app.as_str(), "run_id":id.clone(), "generation":generation,
        "worker":worker.as_str(), "epoch":epoch, "token_hash":token.hash(), "deadline":expires,
        "state":"leased", "created_at":now, "frontier_revision":run.integer("frontier_revision")?,
        "journal_revision":run.integer("journal_revision")?,
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
    Ok(ReadyClaim::Task(Box::new(TaskAssignment {
        id: task_id,
        token,
        generation,
        epoch,
        deadline: expires,
        lease_ms,
        invocation,
    })))
}

pub(super) async fn complete_in(
    tx: &mut Transaction,
    claim: &AuthorizedTask,
    execution: WorkflowExecution,
    body_digest: &str,
) -> Result<CompletionReceipt, WorkflowServiceError> {
    let state = frontier::apply(
        tx,
        &claim.app,
        &claim.run,
        &claim.policy,
        execution,
        claim.now,
    )
    .await?;
    super::publication::advance(tx, &claim.app, &claim.run.text("id")?, claim.now).await?;
    claim.validate_at(tx.now().await?)?;
    let receipt = CompletionReceipt {
        task_id: claim.task.id.clone(),
        app_id: claim.app.clone(),
        run_id: claim.run.text("id")?,
        generation: claim.task.generation,
        state,
        committed_at: claim.now,
    };
    claim
        .update_task(
            tx,
            value!({"state":"completed", "completion_digest":body_digest,
        "receipt":encode(&receipt)?, "finished_at":claim.now}),
        )
        .await?;
    claim.validate_at(tx.now().await?)?;
    Ok(receipt)
}
