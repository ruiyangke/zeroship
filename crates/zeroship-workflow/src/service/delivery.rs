//! Exact-run acceptance of manager-delivered work in the creator journal.

#![expect(
    clippy::future_not_send,
    reason = "creator transactions run on their owning compio thread"
)]

use super::{
    app::{decode, encode, lock_app_state, lock_run, parse_state},
    frontier,
    models::{self, job_receipts},
    policy::PolicyAuthority,
    publication,
    store::{Row, Transaction},
    tasks::{self, ReadyClaim},
    AppWorkflows, ControlIntent, TaskAssignment, WorkerIdentity,
};
use crate::{WorkflowExecution, WorkflowServiceError};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use zeroship_core::{
    app_id::AppId,
    workflow_jobs::{Delivery, JobLease, JobOperation, JobOutcome, JobSpec, Settlement},
};
use zeroship_data_orm::{
    orm::{Entity, EntityAlias, FindOptions, FromRow, Operation, Output, ReadPredicate},
    value,
};

/// Keep the original attempt bound while interrupting invalidated policy I/O.
/// Missing initial authority permits only the caller's immutable receipt path.
pub(super) fn run_attempt<'a, T: 'a>(
    cancelled: Option<futures::future::Shared<futures::future::BoxFuture<'static, ()>>>,
    budget: Duration,
    operation: impl std::future::Future<Output = Result<T, WorkflowServiceError>> + 'a,
) -> futures::future::LocalBoxFuture<'a, Result<T, WorkflowServiceError>> {
    use futures::FutureExt;
    Box::pin(async move {
        let cancelled = async move {
            match cancelled {
                Some(cancelled) => cancelled.await,
                None => futures::future::pending::<()>().await,
            }
        }
        .fuse();
        let operation = compio::time::timeout(budget, operation).fuse();
        futures::pin_mut!(cancelled, operation);
        futures::select_biased! {
            () = cancelled => Err(WorkflowServiceError::Unavailable("workflow host policy unavailable".into())),
            result = operation => result.map_err(|_| WorkflowServiceError::Timeout)?,
        }
    })
}

/// A semantic result belongs to the logical job, not its delivery attempt.
/// Successor publication remains independently durable in the creator outbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobReceipt {
    pub job: JobSpec,
    pub outcome: JobOutcome,
}

impl JobReceipt {
    /// Bind a persisted outcome to the current attempt for queue settlement.
    ///
    /// # Errors
    /// Rejects an attempt for a different immutable logical job.
    pub fn settlement(&self, lease: &impl JobLease) -> Result<Settlement, WorkflowServiceError> {
        if lease.delivery().job != self.job {
            return Err(conflict());
        }
        if !valid_outcome(&self.job.operation, &self.outcome) {
            return Err(invalid());
        }
        Ok(Settlement {
            delivery: lease.delivery().clone(),
            outcome: self.outcome.clone(),
            successors: Vec::new(),
        })
    }
}

#[derive(Debug)]
pub enum JobAcceptance {
    Execute(Box<DeliveredTask>),
    Settled(JobReceipt),
    /// The job remains unsettled: code, policy, a live task or creator time
    /// prevents execution. This is not a durable rejection or an ACK.
    Deferred,
}

/// Creator execution authority with its original monotonic expiration.
/// The executor also enforces its separate hard execution budget.
#[derive(Debug, Clone)]
pub struct DeliveredTask {
    assignment: TaskAssignment,
    delivery: Delivery,
    expires: Instant,
}

impl DeliveredTask {
    #[must_use]
    pub const fn assignment(&self) -> &TaskAssignment {
        &self.assignment
    }

    /// # Errors
    /// Refuses execution or renewal after the confirmed creator lease expires.
    pub fn remaining(&self) -> Result<Duration, WorkflowServiceError> {
        expires_in(self.expires)
    }

    /// Adopt a renewal whole. The creator deadline, the granted lease and the
    /// monotonic expiration derived from them take effect together, so no
    /// caller can extend the stored deadline without the expiration that
    /// bounds execution under it.
    pub fn renew(&mut self, renewal: TaskRenewal) {
        self.delivery = renewal.delivery;
        self.expires = renewal.expires;
        self.assignment.deadline = renewal.deadline;
        self.assignment.lease_ms = renewal.lease_ms;
    }
}

/// Everything one renewal changes about a live delivered task, with the run
/// control intent read in the same transaction.
///
/// The renewal answers a task its holder already has, so it carries no
/// [`TaskAssignment`]: the invocation and its journal stay on the holder's copy
/// instead of crossing the reply on every heartbeat of a long step.
#[derive(Debug, Clone)]
pub struct TaskRenewal {
    delivery: Delivery,
    expires: Instant,
    deadline: i64,
    lease_ms: i64,
    control: ControlIntent,
}

impl TaskRenewal {
    /// The run's effective control intent when this renewal committed.
    #[must_use]
    pub const fn control(&self) -> ControlIntent {
        self.control
    }
}

/// Creator authority left on the monotonic clock.
///
/// # Errors
/// Refuses execution or renewal at or after the confirmed expiration.
fn expires_in(expires: Instant) -> Result<Duration, WorkflowServiceError> {
    expires
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or(WorkflowServiceError::Timeout)
}

pub(super) struct CapturedLease {
    delivery: Delivery,
    expires: Instant,
    policy: PolicyAuthority,
}
impl CapturedLease {
    pub(super) fn bind(&self, scope: &AppWorkflows) -> Result<AppWorkflows, WorkflowServiceError> {
        self.check(scope)?;
        scope.clone().with_authority(self.policy.clone())
    }

    pub(super) fn capture(
        scope: &AppWorkflows,
        lease: &impl JobLease,
    ) -> Result<Self, WorkflowServiceError> {
        let started = Instant::now();
        let duration = remaining(lease)?;
        let policy = scope.capture_policy().authority()?.clone();
        let duration = duration.min(Duration::from_millis(
            u64::try_from(policy.policy.lease_ms).map_err(|_| WorkflowServiceError::Timeout)?,
        ));
        let mut expires = started
            .checked_add(duration)
            .ok_or(WorkflowServiceError::Timeout)?;
        if let Some(deadline) = policy.deadline {
            expires = expires.min(deadline);
        }
        Ok(Self {
            delivery: lease.delivery().clone(),
            expires,
            policy,
        })
    }
    pub(super) fn check(&self, scope: &AppWorkflows) -> Result<(), WorkflowServiceError> {
        if !self.policy.belongs_to(&scope.binding) {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        self.policy.check()?;
        remaining(self).map(|_| ())
    }
}
impl CapturedLease {
    pub(super) fn cancelled(
        &self,
    ) -> futures::future::Shared<futures::future::BoxFuture<'static, ()>> {
        self.policy.cancelled()
    }
    pub(super) const fn policy(&self) -> &super::AppPolicy {
        &self.policy.policy
    }
}
impl JobLease for CapturedLease {
    fn delivery(&self) -> &Delivery {
        &self.delivery
    }
    fn remaining(&self) -> Option<Duration> {
        self.policy.check().ok()?;
        self.expires
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
    }
}

#[derive(FromRow)]
#[orm(entity = job_receipts)]
pub(super) struct Record {
    run_id: Option<String>,
    specification: String,
    outcome: Option<String>,
    completed_at: Option<i64>,
}

impl Record {
    pub(super) fn receipt(
        &self,
        job: &JobSpec,
    ) -> Result<Option<JobReceipt>, WorkflowServiceError> {
        if decode::<JobSpec>(&self.specification)? != *job {
            return Err(conflict());
        }
        // `run_id` is the receipt's one kind-blind column: the scan that finds
        // runs with no outstanding receipt joins on it without reading the
        // specification, so every operation declares whether its receipt names
        // a run. Whatever else a kind stores lives in that kind's own table.
        let valid = match &job.operation {
            JobOperation::Advance { run_id, .. } => self.run_id.as_deref() == Some(run_id.as_str()),
            JobOperation::Cron { run_id, .. } => {
                self.run_id.is_none() || self.run_id.as_deref() == Some(run_id.as_str())
            }
            JobOperation::Activate { .. }
            | JobOperation::Management { .. }
            | JobOperation::Collect {}
            | JobOperation::Close { .. }
            | JobOperation::Fanout { .. }
            | JobOperation::Propagate { .. }
            | JobOperation::Reconcile {}
            | JobOperation::ReleaseHold { .. } => self.run_id.is_none(),
        };
        if !valid {
            return Err(invalid());
        }
        match (&self.outcome, self.completed_at) {
            (Some(outcome), Some(_)) => {
                let outcome = decode(outcome)?;
                self.check_outcome(job, &outcome)?;
                Ok(Some(JobReceipt {
                    job: job.clone(),
                    outcome,
                }))
            }
            (None, None) => Ok(None),
            _ => Err(invalid()),
        }
    }

    fn check_outcome(
        &self,
        job: &JobSpec,
        outcome: &JobOutcome,
    ) -> Result<(), WorkflowServiceError> {
        if !valid_outcome(&job.operation, outcome) {
            return Err(invalid());
        }
        if let JobOperation::Cron { run_id, .. } = &job.operation {
            let valid = match outcome {
                JobOutcome::Completed {} => self.run_id.as_deref() == Some(run_id.as_str()),
                JobOutcome::Rejected {} => self.run_id.is_none(),
                JobOutcome::Waiting {} | JobOutcome::Management { .. } | JobOutcome::Closed { .. } => {
                    false
                }
            };
            if !valid {
                return Err(invalid());
            }
        }
        Ok(())
    }
}

const fn valid_outcome(operation: &JobOperation, outcome: &JobOutcome) -> bool {
    if !outcome.valid_for(operation) {
        return false;
    }
    match operation {
        JobOperation::Activate { .. } => matches!(outcome, JobOutcome::Completed {}),
        JobOperation::Advance { .. } => true,
        JobOperation::Cron { .. } => {
            matches!(outcome, JobOutcome::Completed {} | JobOutcome::Rejected {})
        }
        JobOperation::Reconcile {} => {
            matches!(outcome, JobOutcome::Completed {} | JobOutcome::Waiting {})
        }
        JobOperation::Management { .. } => matches!(outcome, JobOutcome::Management { .. }),
        JobOperation::Close { .. } => matches!(outcome, JobOutcome::Closed { .. }),
        JobOperation::Collect {}
        | JobOperation::Fanout { .. }
        | JobOperation::Propagate { .. }
        // A refused release is Waiting, never Rejected: the deployment is still
        // needed, and the journal holder keeps it until a later job succeeds.
        | JobOperation::ReleaseHold { .. } => {
            matches!(outcome, JobOutcome::Completed {} | JobOutcome::Waiting {})
        }
    }
}

impl AppWorkflows {
    /// Accept only the run, generation, deployment and frontier named by a job.
    /// Duplicate jobs replay their semantic receipt; delivery attempts acquire
    /// distinct task tokens and creator fences. A wire deadline grants no time.
    ///
    /// # Errors
    /// Rejects foreign scopes, changed job identities, unsupported operations,
    /// expired delivery authority and creator storage failures.
    pub async fn accept_job(
        &self,
        lease: &impl JobLease,
    ) -> Result<JobAcceptance, WorkflowServiceError> {
        let delivery = lease.delivery().clone();
        let job = &delivery.job;
        check_scope(&self.app, job)?;
        let captured = CapturedLease::capture(self, lease);
        let budget = attempt_budget(captured.as_ref().ok(), None);
        run_attempt(
            captured.as_ref().ok().map(CapturedLease::cancelled),
            budget,
            Box::pin(self.accept_captured(delivery, captured)),
        )
        .await
    }

    async fn accept_captured(
        &self,
        delivery: Delivery,
        captured: Result<CapturedLease, WorkflowServiceError>,
    ) -> Result<JobAcceptance, WorkflowServiceError> {
        let job = &delivery.job;
        let JobOperation::Advance {
            deployment_id,
            run_id,
            generation,
            revision,
        } = &job.operation
        else {
            return Err(WorkflowServiceError::InvalidRequest(
                "unsupported workflow delivery operation".into(),
            ));
        };
        let mut tx = self.service.begin().await?;
        lock_app_state(&mut tx, &self.app).await?;
        let existing = read(&tx, job).await?;
        if let Some(existing) = &existing {
            if let Some(receipt) = existing.receipt(job)? {
                tx.commit().await?;
                return Ok(JobAcceptance::Settled(receipt));
            }
        }
        let lease = &captured?;
        lease.check(self)?;
        let policy = &lease.policy.policy;
        let now = tx.now().await?;
        if existing.is_none() {
            tx.database().collection(job_receipts::Entity::COLLECTION)?.insert(value!({
                "id":job.id.as_str(), "app_id":self.app.as_str(), "run_id":run_id.as_str(), "specification":encode(job)?, "created_at":now,
            })).await?;
        }
        let run = match lock_run(&mut tx, &self.app, run_id.as_str()).await {
            Ok(run) => Some(run),
            Err(WorkflowServiceError::NotFound(_)) => None,
            Err(error) => return Err(error),
        };
        let stale = match &run {
            None => true,
            Some(run) => {
                !current_frontier(run, deployment_id.as_str(), *generation, revision.get())?
            }
        };
        if stale {
            let receipt = finish(&tx, job, JobOutcome::Rejected {}, now).await?;
            lease.check(self)?;
            tx.commit().await?;
            return Ok(JobAcceptance::Settled(receipt));
        }
        let Some(run) = reclaim(&mut tx, &self.app, run.ok_or_else(invalid)?, now).await? else {
            lease.check(self)?;
            tx.commit().await?;
            return Ok(JobAcceptance::Deferred);
        };
        if run.optional_integer("due_at")?.is_none_or(|due| due > now) {
            lease.check(self)?;
            tx.commit().await?;
            return Ok(JobAcceptance::Deferred);
        }
        if !frontier::prepare(&mut tx, &self.app, &run, policy, now).await? {
            publication::advance(&tx, &self.app, run_id.as_str(), now).await?;
            let current = lock_run(&mut tx, &self.app, run_id.as_str()).await?;
            let outcome = if parse_state(&current.text("state")?)?.is_terminal() {
                JobOutcome::Completed {}
            } else {
                JobOutcome::Waiting {}
            };
            let receipt = finish(&tx, job, outcome, now).await?;
            lease.check(self)?;
            tx.commit().await?;
            return Ok(JobAcceptance::Settled(receipt));
        }
        let worker = WorkerIdentity::new(delivery.worker_id.as_str().into())?;
        let lease_ms = remaining_millis(lease)?;
        let result =
            match tasks::assign(&mut tx, &self.app, &run, policy, &worker, now, lease_ms).await? {
                ReadyClaim::Task(task) => {
                    tx.database().collection(models::tasks::Entity::COLLECTION)?.update(
                    value!({"app_id":self.app.as_str(), "id":task.id.clone()}),
                    value!({"job_id":job.id.as_str(), "delivery_attempt":delivery.attempt.get(),
                        "assignment_revision":delivery.assignment_revision.get()}),
                ).await?;
                    let expires = creator_deadline(&mut tx, task.deadline, lease.expires).await?;
                    JobAcceptance::Execute(Box::new(DeliveredTask {
                        assignment: *task,
                        delivery: delivery.clone(),
                        expires,
                    }))
                }
                ReadyClaim::Unavailable | ReadyClaim::Busy => JobAcceptance::Deferred,
            };
        lease.check(self)?;
        tx.commit().await?;
        // COMMIT may have completed after cancellation. Its claim is durable,
        // but exhausted authority cannot authorize starting an executor.
        lease.check(self)?;
        if let JobAcceptance::Execute(task) = &result {
            task.remaining()?;
        }
        Ok(result)
    }

    /// Renew a creator task only under an identity-matching manager grant.
    /// The executor's original hard budget remains independent of this renewal.
    /// The reply carries only what the renewal changed, which the caller applies
    /// to the task it already holds.
    ///
    /// # Errors
    /// Refuses stale task/delivery identity, expired authority and storage errors.
    pub async fn heartbeat_job(
        &self,
        task: &DeliveredTask,
        grant: &impl JobLease,
    ) -> Result<TaskRenewal, WorkflowServiceError> {
        let delivery = self.task_delivery(task, grant)?;
        task.remaining()?;
        let lease = CapturedLease::capture(self, grant)?;
        let budget = attempt_budget(Some(&lease), Some(task));
        run_attempt(Some(lease.cancelled()), budget, async {
            let worker = WorkerIdentity::new(delivery.worker_id.as_str().into())?;
            let mut tx = self.service.begin().await?;
            let claim = tasks::authorized_task(
                &mut tx,
                &worker,
                &task.assignment.id,
                &task.assignment.token,
            )
            .await?;
            authorize_task(&claim, &delivery)?;
            claim.validate_live()?;
            lease.check(self)?;
            task.remaining()?;
            let control =
                super::propagation::effective_control(&tx, &claim.app, &claim.run).await?;
            if !lease.policy.policy.admission || !lease.policy.policy.dispatch {
                tx.commit().await?;
                return Ok(TaskRenewal {
                    delivery: task.delivery.clone(),
                    expires: task.expires,
                    deadline: task.assignment.deadline,
                    lease_ms: task.assignment.lease_ms,
                    control: if control == ControlIntent::None {
                        ControlIntent::Pause
                    } else {
                        control
                    },
                });
            }
            let lease_ms = remaining_millis(&lease)?;
            let deadline = super::app::deadline(claim.now, lease_ms)?;
            claim
                .update_task(&tx, value!({"deadline":deadline}))
                .await?;
            claim.update_run(&tx, value!({"due_at":deadline})).await?;
            let expires = creator_deadline(&mut tx, deadline, lease.expires).await?;
            claim.validate_at(tx.now().await?)?;
            lease.check(self)?;
            task.remaining()?;
            tx.commit().await?;
            lease.check(self)?;
            task.remaining()?;
            expires_in(expires)?;
            Ok(TaskRenewal {
                delivery,
                expires,
                deadline,
                lease_ms,
                control,
            })
        })
        .await
    }

    /// Commit a stopped executor's checkpoint, semantic job receipt and intents.
    /// An exact task completion retry can read its receipt after lease expiry.
    ///
    /// # Errors
    /// Refuses changed delivery/results, stale creator fences, lost authority,
    /// invalid checkpoints and creator storage failures.
    pub async fn complete_job(
        &self,
        task: &DeliveredTask,
        grant: &impl JobLease,
        execution: WorkflowExecution,
    ) -> Result<JobReceipt, WorkflowServiceError> {
        let delivery = self.task_delivery(task, grant)?;
        let captured = CapturedLease::capture(self, grant);
        let budget = attempt_budget(captured.as_ref().ok(), Some(task));
        run_attempt(
            captured.as_ref().ok().map(CapturedLease::cancelled),
            budget,
            Box::pin(async {
                let worker = WorkerIdentity::new(delivery.worker_id.as_str().into())?;
                let digest = super::types::digest(&execution)?;
                let mut tx = self.service.begin().await?;
                let claim = tasks::inspect_task(
                    &mut tx,
                    &worker,
                    &task.assignment.id,
                    &task.assignment.token,
                )
                .await?;
                authorize_task(&claim, &delivery)?;
                if claim.task.state == "completed" {
                    if claim.task.completion_digest.as_deref() != Some(&digest) {
                        return Err(conflict());
                    }
                    let receipt = read(&tx, &delivery.job)
                        .await?
                        .ok_or_else(invalid)?
                        .receipt(&delivery.job)?
                        .ok_or_else(invalid)?;
                    tx.commit().await?;
                    return Ok(receipt);
                }
                let lease = captured?;
                lease.check(self)?;
                task.remaining()?;
                claim.validate_live()?;
                let claim = claim.authorize(&mut tx)?;
                let completion = tasks::complete_in(&mut tx, &claim, execution, &digest).await?;
                let outcome = if completion.state.is_terminal() {
                    JobOutcome::Completed {}
                } else {
                    JobOutcome::Waiting {}
                };
                let receipt = finish(&tx, &delivery.job, outcome, claim.now).await?;
                claim.validate_at(tx.now().await?)?;
                lease.check(self)?;
                task.remaining()?;
                tx.commit().await?;
                Ok(receipt)
            }),
        )
        .await
    }

    /// Release only after execution and its native operations have stopped.
    /// Keep the old task identity on the run until authorized delivery reclaims it.
    ///
    /// # Errors
    /// Refuses changed delivery, expired authority, stale tasks and storage errors.
    pub async fn release_job(
        &self,
        task: &DeliveredTask,
        grant: &impl JobLease,
    ) -> Result<(), WorkflowServiceError> {
        let delivery = self.task_delivery(task, grant)?;
        let captured = CapturedLease::capture(self, grant);
        let budget = attempt_budget(captured.as_ref().ok(), Some(task));
        run_attempt(
            captured.as_ref().ok().map(CapturedLease::cancelled),
            budget,
            async {
                let worker = WorkerIdentity::new(delivery.worker_id.as_str().into())?;
                let mut tx = self.service.begin().await?;
                let claim = tasks::inspect_task(
                    &mut tx,
                    &worker,
                    &task.assignment.id,
                    &task.assignment.token,
                )
                .await?;
                authorize_task(&claim, &delivery)?;
                if claim.task.state == "released" {
                    tx.commit().await?;
                    return Ok(());
                }
                let lease = captured?;
                lease.check(self)?;
                task.remaining()?;
                claim.validate_live()?;
                let claim = claim.authorize(&mut tx)?;
                claim
                    .update_task(&tx, value!({"state":"released", "finished_at":claim.now}))
                    .await?;
                claim.update_run(&tx, value!({"due_at":claim.now})).await?;
                claim.validate_at(tx.now().await?)?;
                lease.check(self)?;
                task.remaining()?;
                tx.commit().await
            },
        )
        .await
    }

    fn task_delivery(
        &self,
        task: &DeliveredTask,
        lease: &impl JobLease,
    ) -> Result<Delivery, WorkflowServiceError> {
        let delivery = lease.delivery();
        check_scope(&self.app, &delivery.job)?;
        if delivery.job != task.delivery.job
            || delivery.worker_id != task.delivery.worker_id
            || delivery.assignment_revision != task.delivery.assignment_revision
            || delivery.attempt != task.delivery.attempt
        {
            return Err(conflict());
        }
        Ok(delivery.clone())
    }

    /// Read the original semantic result without requiring a live delivery.
    /// This does not authorize new work or an unsettled manager ACK.
    ///
    /// # Errors
    /// Rejects foreign scope, changed immutable metadata and storage failures.
    pub async fn job_receipt(
        &self,
        job: &JobSpec,
    ) -> Result<Option<JobReceipt>, WorkflowServiceError> {
        check_scope(&self.app, job)?;
        let mut tx = self.service.begin_history().await?;
        if matches!(job.operation, JobOperation::Fanout { .. }) {
            lock_app_state(&mut tx, &self.app).await?;
            let receipt = super::fanout::receipt(&tx, job).await?;
            tx.commit().await?;
            return Ok(receipt);
        }
        if matches!(job.operation, JobOperation::Propagate { .. }) {
            lock_app_state(&mut tx, &self.app).await?;
            let receipt = super::propagation::receipt(&tx, job).await?;
            tx.commit().await?;
            return Ok(receipt);
        }
        if matches!(job.operation, JobOperation::Management { .. }) {
            lock_app_state(&mut tx, &self.app).await?;
            let receipt = super::management::receipt(&tx, job).await?;
            tx.commit().await?;
            return Ok(receipt);
        }
        if matches!(job.operation, JobOperation::Collect {}) {
            lock_app_state(&mut tx, &self.app).await?;
            let receipt = super::collection::receipt(&tx, job).await?;
            tx.commit().await?;
            return Ok(receipt);
        }
        if matches!(job.operation, JobOperation::Reconcile {}) {
            lock_app_state(&mut tx, &self.app).await?;
            let receipt = super::reconciliation::receipt(&tx, job).await?;
            tx.commit().await?;
            return Ok(receipt);
        }
        if matches!(job.operation, JobOperation::Close { .. }) {
            lock_app_state(&mut tx, &self.app).await?;
            let receipt = super::closure::receipt(&tx, job).await?;
            tx.commit().await?;
            return Ok(receipt);
        }
        if matches!(job.operation, JobOperation::Cron { .. }) {
            let receipt = super::cron::receipt(&tx, job).await?;
            tx.commit().await?;
            return Ok(receipt);
        }
        let receipt = read(&tx, job)
            .await?
            .map(|record| record.receipt(job))
            .transpose()?
            .flatten();
        tx.commit().await?;
        Ok(receipt)
    }
}

// Replacing a task is serialized with its completion by the app lock. A live
// creator claim remains authoritative even if another manager job was delivered.
async fn reclaim(
    tx: &mut Transaction,
    app: &AppId,
    mut run: Row,
    now: i64,
) -> Result<Option<Row>, WorkflowServiceError> {
    if let Some(task) = run.optional_text("task_id")? {
        let id = run.text("id")?;
        let changed = tx
            .database()
            .collection(models::tasks::Entity::COLLECTION)?
            .execute(Operation::Update {
                filter: value!({"app_id":app.as_str(), "id":task, "run_id":id.clone(),
                    "generation":run.integer("generation")?, "epoch":run.integer("lease_epoch")?,
                    "$or":[{"state":"leased", "deadline":{"$lte":now}}, {"state":"released"}]}),
                patch: value!({"state":"expired", "finished_at":now}),
                many: true,
            })
            .await?;
        if !matches!(changed, Output::Count(1)) {
            return Ok(None);
        }
        tx.database()
            .collection(models::runs::Entity::COLLECTION)?
            .update(
                value!({"app_id":app.as_str(), "id":id.clone()}),
                value!({"task_id":null, "due_at":now}),
            )
            .await?;
        run = lock_run(tx, app, &id).await?;
    }
    Ok(Some(run))
}

fn current_frontier(
    run: &Row,
    deployment: &str,
    generation: u32,
    revision: i64,
) -> Result<bool, WorkflowServiceError> {
    Ok(run.integer("generation")? == i64::from(generation)
        && run.integer("frontier_revision")? == revision
        && run.text("deploy_id")? == deployment
        && !parse_state(&run.text("state")?)?.is_terminal())
}

fn authorize_task(
    claim: &tasks::TaskInspection,
    delivery: &Delivery,
) -> Result<(), WorkflowServiceError> {
    let JobOperation::Advance {
        run_id,
        generation,
        revision,
        ..
    } = &delivery.job.operation
    else {
        return Err(conflict());
    };
    if claim.app != delivery.job.app_id
        || claim.task.job_id.as_deref() != Some(delivery.job.id.as_str())
        || claim.task.delivery_attempt != Some(delivery.attempt.get())
        || claim.task.assignment_revision != Some(delivery.assignment_revision.get())
        || claim.task.run_id != run_id.as_str()
        || claim.task.generation != i64::from(*generation)
        || claim.task.frontier_revision != revision.get()
    {
        return Err(conflict());
    }
    Ok(())
}

/// How long one attempt may wait on the creator journal.
///
/// An attempt holds a journal connection from BEGIN through COMMIT and takes
/// the app-state row lock that every other attempt for the same app also
/// takes, so storage that stops answering pins that connection and queues the
/// app's other attempts behind it. Ending the attempt here gives the
/// connection and the worker's slot back while the grant is still live,
/// rather than holding both until the grant lapses, and a caller that can
/// retry does so under the authority it already holds.
///
/// An attempt that reaches the platform between transactions opens more than
/// one, and each of them takes the same app-state row lock, so this bounds the
/// whole sequence rather than a single BEGIN through COMMIT. Work outside the
/// journal transaction does not put an attempt outside this ceiling.
///
/// This is an absolute duration rather than a fraction of the lease on
/// purpose. The lease answers how long this worker may act, which
/// [`attempt_budget`] applies as its own separate term; this answers how long
/// the journal may take to answer at all, which is a fact about storage.
/// Deriving it from the lease would put admission policy in charge of how long
/// a journal connection can be pinned.
///
/// Reaching it is cheap. The manager counts an attempt against its delivery
/// ceiling only on that attempt's first renewal, so an acceptance that ends
/// here leaves the ceiling where it was and the job stays deliverable.
pub(super) const ATTEMPT_IO_CEILING: Duration = Duration::from_secs(5);

// Fresh attempts consume their captured authority even while BEGIN, locks or
// COMMIT wait. Expired attempts can only reach the bounded receipt branches.
pub(super) fn attempt_budget(
    lease: Option<&CapturedLease>,
    task: Option<&DeliveredTask>,
) -> Duration {
    let Some(lease) = lease else {
        return ATTEMPT_IO_CEILING;
    };
    let remaining = lease.expires.saturating_duration_since(Instant::now());
    task.map_or_else(
        || ATTEMPT_IO_CEILING.min(remaining),
        |task| {
            task.remaining().map_or(ATTEMPT_IO_CEILING, |task| {
                ATTEMPT_IO_CEILING.min(remaining).min(task)
            })
        },
    )
}

/// Re-anchor a creator-clock deadline onto the monotonic clock, never past the
/// captured authority that authorized the task. The cap is the binding term
/// only when the creator clock disagrees with the monotonic one: on both call
/// paths below the deadline is itself derived from the captured lease, and the
/// clock read converting it back is taken after the read it was built from, so
/// an agreeing clock always lands inside the cap on its own.
pub(super) async fn creator_deadline(
    tx: &mut Transaction,
    deadline: i64,
    cap: Instant,
) -> Result<Instant, WorkflowServiceError> {
    let started = Instant::now();
    let remaining = deadline
        .checked_sub(tx.now().await?)
        .and_then(|value| value.checked_sub(1))
        .filter(|value| *value > 0)
        .ok_or(WorkflowServiceError::Timeout)?;
    started
        .checked_add(Duration::from_millis(
            u64::try_from(remaining).map_err(|_| WorkflowServiceError::Timeout)?,
        ))
        .map(|expires| expires.min(cap))
        .ok_or(WorkflowServiceError::Timeout)
}

pub(super) fn remaining(lease: &impl JobLease) -> Result<Duration, WorkflowServiceError> {
    lease
        .remaining()
        .filter(|remaining| !remaining.is_zero())
        .ok_or(WorkflowServiceError::Timeout)
}

pub(super) fn remaining_millis(lease: &impl JobLease) -> Result<i64, WorkflowServiceError> {
    // Subtract the database clock's quantization margin before translating the
    // remaining monotonic grant into a creator-clock deadline.
    i64::try_from(remaining(lease)?.as_millis())
        .ok()
        .and_then(|millis| millis.checked_sub(1))
        .filter(|millis| *millis > 0)
        .ok_or(WorkflowServiceError::Timeout)
}

pub(super) async fn finish(
    tx: &Transaction,
    job: &JobSpec,
    outcome: JobOutcome,
    now: i64,
) -> Result<JobReceipt, WorkflowServiceError> {
    let record = read(tx, job).await?.ok_or_else(invalid)?;
    if record.receipt(job)?.is_some() {
        return Err(conflict());
    }
    record.check_outcome(job, &outcome)?;
    let changed = tx.database().collection(job_receipts::Entity::COLLECTION)?.execute(Operation::Update {
        filter:value!({"app_id":job.app_id.as_str(), "id":job.id.as_str(), "outcome":null, "completed_at":null}),
        patch:value!({"outcome":encode(&outcome)?, "completed_at":now}), many:true,
    }).await?;
    if !matches!(changed, Output::Count(1)) {
        return Err(conflict());
    }
    Ok(JobReceipt {
        job: job.clone(),
        outcome,
    })
}

pub(super) fn check_scope(app: &AppId, job: &JobSpec) -> Result<(), WorkflowServiceError> {
    if app != &job.app_id {
        return Err(WorkflowServiceError::PermissionDenied);
    }
    Ok(())
}

pub(super) async fn read(
    tx: &Transaction,
    job: &JobSpec,
) -> Result<Option<Record>, WorkflowServiceError> {
    Ok(tx
        .database()
        .entity::<job_receipts::Entity>()?
        .find::<Record>(
            job_receipts::app_id
                .eq(job.app_id.as_str())?
                .and(job_receipts::id.eq(job.id.as_str())?),
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .next())
}

/// A job kind's receipt extension: a table keyed by the job id, scoped by the
/// app, whose foreign key onto `job_receipts(app_id, id)` is what forbids an
/// extension without its receipt. The scope is also the join below, so a kind
/// reaches its own extension and no other's.
pub(super) trait ReceiptExtension: Entity + Sized {
    fn scope(
        extension: &EntityAlias<Self>,
        receipts: &EntityAlias<job_receipts::Entity>,
    ) -> Result<ReadPredicate, WorkflowServiceError>;
}

macro_rules! receipt_extension {
    ($($module:ident),+ $(,)?) => {$(
        impl ReceiptExtension for models::$module::Entity {
            fn scope(
                extension: &EntityAlias<Self>,
                receipts: &EntityAlias<job_receipts::Entity>,
            ) -> Result<ReadPredicate, WorkflowServiceError> {
                Ok(extension
                    .column(models::$module::app_id)
                    .eq(receipts.column(job_receipts::app_id))?
                    .and(
                        extension
                            .column(models::$module::id)
                            .eq(receipts.column(job_receipts::id))?,
                    ))
            }
        }
    )+};
}
receipt_extension!(
    collection_pages,
    fanout_pages,
    propagation_pages,
    reconciliation_pages,
);

/// Read a receipt together with its kind's extension.
///
/// A kind writes both rows in one transaction and the extension's foreign key
/// makes an orphan extension impossible, so the pair is present or absent
/// together. A receipt whose extension is missing is a damaged journal, not an
/// absent job, and the outer join reports it as exactly that.
pub(super) async fn read_extended<E: ReceiptExtension, P: FromRow<E>>(
    tx: &Transaction,
    job: &JobSpec,
    invalid: fn() -> WorkflowServiceError,
) -> Result<Option<(Record, P)>, WorkflowServiceError> {
    let receipts = tx.database().entity::<job_receipts::Entity>()?.alias("r")?;
    let extension = tx.database().entity::<E>()?.alias("x")?;
    let scope = E::scope(&extension, &receipts)?;
    let row = tx
        .database()
        .from(&receipts)
        .left_join(&extension, scope)?
        .filter(
            receipts
                .column(job_receipts::app_id)
                .eq(job.app_id.as_str())?
                .and(receipts.column(job_receipts::id).eq(job.id.as_str())?),
        )
        .select((receipts.row::<Record>(), extension.optional_row::<P>()))?
        .limit(1)?
        .all()
        .await?
        .into_iter()
        .next();
    match row {
        None => Ok(None),
        Some((record, Some(extension))) => Ok(Some((record, extension))),
        Some((_, None)) => Err(invalid()),
    }
}

/// The extension a paged sweep stores: the plan its delivered page committed to
/// and the index of the next item it reserves. Both sweeps store that shape, so
/// one reservation advances either.
pub(super) trait PageCursor: ReceiptExtension {}
impl PageCursor for models::collection_pages::Entity {}
impl PageCursor for models::reconciliation_pages::Entity {}

/// Reserve the item a paged sweep is about to visit by advancing its stored
/// cursor past it. Matching the index the page read is what makes a lost update
/// impossible: a concurrent reservation moved it, so the filter matches no row
/// and the zero-row result fails the attempt.
pub(super) async fn reserve<E: PageCursor>(
    tx: &Transaction,
    job: &JobSpec,
    next: usize,
    invalid: fn() -> WorkflowServiceError,
) -> Result<(), WorkflowServiceError> {
    let next = i64::try_from(next).map_err(|_| invalid())?;
    let changed = tx
        .database()
        .collection(E::COLLECTION)?
        .execute(Operation::Update {
            filter: value!({"id":job.id.as_str(), "app_id":job.app_id.as_str(), "next_index":next}),
            patch: value!({"next_index":next.checked_add(1).ok_or_else(invalid)?}),
            many: true,
        })
        .await?;
    if matches!(changed, Output::Count(1)) {
        Ok(())
    } else {
        Err(invalid())
    }
}

fn conflict() -> WorkflowServiceError {
    WorkflowServiceError::Conflict("workflow job identity or outcome conflicts".into())
}
fn invalid() -> WorkflowServiceError {
    WorkflowServiceError::Internal("invalid workflow job receipt journal".into())
}
