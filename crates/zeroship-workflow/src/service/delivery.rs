//! Exact-run acceptance of manager-delivered work in the creator journal.

#![expect(
    clippy::future_not_send,
    reason = "creator transactions run on their owning compio thread"
)]

use super::{
    app::{decode, encode, lock_app, lock_run, parse_state},
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
    orm::{Entity, FindOptions, FromRow, Operation, Output},
    value,
};

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
        Ok(Settlement {
            delivery: lease.delivery().clone(),
            outcome: self.outcome,
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
        self.expires
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or(WorkflowServiceError::Timeout)
    }
}

pub(super) struct CapturedLease {
    delivery: Delivery,
    expires: Instant,
    policy: PolicyAuthority,
}
impl CapturedLease {
    pub(super) fn capture(
        scope: &AppWorkflows,
        lease: &impl JobLease,
    ) -> Result<Self, WorkflowServiceError> {
        let started = Instant::now();
        let duration = remaining(lease)?;
        let policy = scope.service.policies.authority(&scope.app)?;
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
        remaining(self)?;
        self.policy.check(&scope.service.policies, &scope.app)
    }
}
impl JobLease for CapturedLease {
    fn delivery(&self) -> &Delivery {
        &self.delivery
    }
    fn remaining(&self) -> Option<Duration> {
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
    pub(super) reconciliation: Option<String>,
    pub(super) reconciliation_next: Option<i64>,
}

impl Record {
    pub(super) fn receipt(
        &self,
        job: &JobSpec,
    ) -> Result<Option<JobReceipt>, WorkflowServiceError> {
        if decode::<JobSpec>(&self.specification)? != *job {
            return Err(conflict());
        }
        let valid = match &job.operation {
            JobOperation::Activate { .. } => {
                self.run_id.is_none()
                    && self.reconciliation.is_none()
                    && self.reconciliation_next.is_none()
            }
            JobOperation::Advance { run_id, .. } => {
                self.run_id.as_deref() == Some(run_id.as_str())
                    && self.reconciliation.is_none()
                    && self.reconciliation_next.is_none()
            }
            JobOperation::Cron { run_id, .. } => {
                (self.run_id.is_none() || self.run_id.as_deref() == Some(run_id.as_str()))
                    && self.reconciliation.is_none()
                    && self.reconciliation_next.is_none()
            }
            JobOperation::Reconcile {} => {
                self.run_id.is_none()
                    && self.reconciliation.is_some()
                    && self.reconciliation_next.is_some()
            }
            _ => false,
        };
        if !valid {
            return Err(invalid());
        }
        match (&self.outcome, self.completed_at) {
            (Some(outcome), Some(_)) => {
                let outcome = decode(outcome)?;
                if let JobOperation::Cron { run_id, .. } = &job.operation {
                    let valid = match outcome {
                        JobOutcome::Completed => self.run_id.as_deref() == Some(run_id.as_str()),
                        JobOutcome::Rejected => self.run_id.is_none(),
                        _ => false,
                    };
                    if !valid { return Err(invalid()); }
                }
                Ok(Some(JobReceipt { job: job.clone(), outcome }))
            }
            (None, None) => Ok(None),
            _ => Err(invalid()),
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
        compio::time::timeout(budget, Box::pin(self.accept_captured(delivery, captured)))
            .await
            .map_err(|_| WorkflowServiceError::Timeout)?
    }

    async fn accept_captured(
        &self,
        delivery: Delivery,
        captured: Result<CapturedLease, WorkflowServiceError>,
    ) -> Result<JobAcceptance, WorkflowServiceError> {
        let job = &delivery.job;
        let JobOperation::Advance {
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
        lock_app(&mut tx, &self.app).await?;
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
                run.integer("generation")? != i64::from(*generation)
                    || run.integer("frontier_revision")? != revision.get()
                    || run.text("deploy_id")? != job.deployment_id.as_str()
                    || parse_state(&run.text("state")?)?.is_terminal()
            }
        };
        if stale {
            let receipt = finish(&tx, job, JobOutcome::Rejected, now).await?;
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
        if !frontier::prepare(&mut tx, &self.app, &run, now).await? {
            publication::advance(&tx, &self.app, run_id.as_str(), now).await?;
            let current = lock_run(&mut tx, &self.app, run_id.as_str()).await?;
            let outcome = if parse_state(&current.text("state")?)?.is_terminal() {
                JobOutcome::Completed
            } else {
                JobOutcome::Waiting
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
    ///
    /// # Errors
    /// Refuses stale task/delivery identity, expired authority and storage errors.
    pub async fn heartbeat_job(
        &self,
        task: &DeliveredTask,
        grant: &impl JobLease,
    ) -> Result<(DeliveredTask, ControlIntent), WorkflowServiceError> {
        let delivery = self.task_delivery(task, grant)?;
        task.remaining()?;
        let lease = CapturedLease::capture(self, grant)?;
        let budget = attempt_budget(Some(&lease), Some(task));
        compio::time::timeout(budget, async {
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
            let control = ControlIntent::parse(&claim.run.text("control")?)?;
            if !lease.policy.policy.admission || !lease.policy.policy.dispatch {
                tx.commit().await?;
                return Ok((
                    task.clone(),
                    if control == ControlIntent::None {
                        ControlIntent::Pause
                    } else {
                        control
                    },
                ));
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
            let mut renewed = task.clone();
            renewed.delivery = delivery;
            renewed.expires = expires;
            renewed.assignment.deadline = deadline;
            renewed.assignment.lease_ms = lease_ms;
            renewed.remaining()?;
            Ok((renewed, control))
        })
        .await
        .map_err(|_| WorkflowServiceError::Timeout)?
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
        compio::time::timeout(
            budget,
            Box::pin(async {
                let worker = WorkerIdentity::new(delivery.worker_id.as_str().into())?;
                let digest = super::types::digest(&execution)?;
                let mut tx = self.service.begin().await?;
                let claim = tasks::authorized_task(
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
                let completion = tasks::complete_in(&mut tx, &claim, execution, &digest).await?;
                let outcome = if completion.state.is_terminal() {
                    JobOutcome::Completed
                } else {
                    JobOutcome::Waiting
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
        .map_err(|_| WorkflowServiceError::Timeout)?
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
        compio::time::timeout(budget, async {
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
            if claim.task.state == "released" {
                tx.commit().await?;
                return Ok(());
            }
            let lease = captured?;
            lease.check(self)?;
            task.remaining()?;
            claim.validate_live()?;
            claim
                .update_task(&tx, value!({"state":"released", "finished_at":claim.now}))
                .await?;
            claim.update_run(&tx, value!({"due_at":claim.now})).await?;
            claim.validate_at(tx.now().await?)?;
            lease.check(self)?;
            task.remaining()?;
            tx.commit().await
        })
        .await
        .map_err(|_| WorkflowServiceError::Timeout)?
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
        self.service.policies.resolve(&self.app)?;
        let tx = self.service.begin().await?;
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

fn authorize_task(
    claim: &tasks::AuthorizedTask,
    delivery: &Delivery,
) -> Result<(), WorkflowServiceError> {
    let JobOperation::Advance {
        run_id,
        generation,
        revision,
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

// Fresh attempts consume their captured authority even while BEGIN, locks or
// COMMIT wait. Expired attempts can only reach the bounded receipt branches.
pub(super) fn attempt_budget(
    lease: Option<&CapturedLease>,
    task: Option<&DeliveredTask>,
) -> Duration {
    let io_limit = Duration::from_secs(5);
    let Some(lease) = lease else {
        return io_limit;
    };
    let remaining = lease.expires.saturating_duration_since(Instant::now());
    task.map_or_else(
        || io_limit.min(remaining),
        |task| {
            task.remaining()
                .map_or(io_limit, |task| io_limit.min(remaining).min(task))
        },
    )
}

async fn creator_deadline(
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

fn conflict() -> WorkflowServiceError {
    WorkflowServiceError::Conflict("workflow job identity or outcome conflicts".into())
}
fn invalid() -> WorkflowServiceError {
    WorkflowServiceError::Internal("invalid workflow job receipt journal".into())
}
