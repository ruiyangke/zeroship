use super::{Error, WorkerCoordinator};
use std::time::{Duration, Instant};
use zeroship_core::{
    service_identity::endpoints,
    workflow_coordination::{AssignedScope, FailureCode},
    workflow_jobs::{
        Delivery, DeliveryLease, JobLease, JobOperation, JobSpec, Settlement, SettlementReceipt,
        SubmitJob,
    },
};

/// A validated delivery with authority measured on this process's monotonic clock.
/// Cloning preserves its expiration; the wire deadline is diagnostic metadata.
#[derive(Clone, Debug)]
pub struct LeasedJob {
    delivery: Delivery,
    expires: Instant,
}

impl JobLease for LeasedJob {
    fn delivery(&self) -> &Delivery {
        &self.delivery
    }

    fn remaining(&self) -> Option<Duration> {
        Self::remaining(self).ok()
    }
}

impl LeasedJob {
    #[must_use]
    pub const fn delivery(&self) -> &Delivery {
        &self.delivery
    }

    /// Remaining delivery authority, independent of the worker's wall clock.
    /// The executor must additionally enforce its original execution budget.
    ///
    /// # Errors
    /// Returns `Timeout` after this grant expires.
    pub fn remaining(&self) -> Result<Duration, Error> {
        self.expires
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(Error::Timeout)
    }

    fn received(lease: DeliveryLease, request_started: Instant) -> Result<Self, Error> {
        // Manager database timestamps cannot express a larger grant. Validate
        // before conversion rather than accepting an effectively unbounded lease.
        i64::try_from(lease.remaining_ms.get()).map_err(|_| Error::InvalidResponse)?;
        let expires = request_started
            .checked_add(Duration::from_millis(lease.remaining_ms.get()))
            .ok_or(Error::InvalidResponse)?;
        let job = Self {
            delivery: lease.delivery,
            expires,
        };
        job.remaining()?;
        Ok(job)
    }
}

impl WorkerCoordinator {
    /// Publish an app's durable intent under its current assignment.
    ///
    /// # Errors
    /// Refuses foreign scope, manager-owned operations, failed exchanges and
    /// receipts that change the submitted specification.
    pub async fn submit_job(&self, request: &SubmitJob) -> Result<JobSpec, Error> {
        if request.scope.app_id != request.job.app_id {
            return Err(denied());
        }
        worker_publication(&request.job)?;
        let submitted: JobSpec = self
            .transport
            .post(endpoints::WORKFLOW_JOB_SUBMIT, request)
            .await?;
        if submitted != request.job {
            return Err(Error::InvalidResponse);
        }
        Ok(submitted)
    }

    /// Claim an eligible job and capture its remaining delivery authority.
    ///
    /// # Errors
    /// Refuses failed exchanges, substituted scope/worker/revision and grants
    /// exhausted by transport delay or outside the representable clock range.
    pub async fn claim_job(&self, scope: &AssignedScope) -> Result<Option<LeasedJob>, Error> {
        let started = Instant::now();
        let lease: Option<DeliveryLease> = self
            .transport
            .post(endpoints::WORKFLOW_JOB_CLAIM, scope)
            .await?;
        lease
            .map(|lease| {
                if lease.delivery.worker_id != self.worker_id
                    || lease.delivery.job.app_id != scope.app_id
                    || lease.delivery.assignment_revision != scope.assignment_revision
                {
                    return Err(Error::InvalidResponse);
                }
                LeasedJob::received(lease, started)
            })
            .transpose()
    }

    /// Renew a delivery while its previously confirmed local authority is live.
    /// A late reply cannot reactivate an expired execution.
    ///
    /// # Errors
    /// Refuses expired grants, another worker's delivery, failed exchanges and
    /// responses that substitute the immutable delivery identity.
    pub async fn heartbeat_job(&self, job: &LeasedJob) -> Result<LeasedJob, Error> {
        if job.delivery.worker_id != self.worker_id {
            return Err(denied());
        }
        job.remaining()?;
        let started = Instant::now();
        let lease: DeliveryLease = self
            .transport
            .post(endpoints::WORKFLOW_JOB_HEARTBEAT, &job.delivery)
            .await?;
        job.remaining()?;
        let observed = &lease.delivery;
        if observed.job != job.delivery.job
            || observed.worker_id != job.delivery.worker_id
            || observed.assignment_revision != job.delivery.assignment_revision
            || observed.attempt != job.delivery.attempt
        {
            return Err(Error::InvalidResponse);
        }
        LeasedJob::received(lease, started)
    }

    /// Settle only a creator-committed outcome, preserving its successor IDs.
    /// An exact settled receipt may be retried after delivery expiry.
    ///
    /// # Errors
    /// Refuses foreign worker/app identities, manager-owned successors, failed
    /// exchanges and receipts that substitute job, attempt or outcome.
    pub async fn settle_job(&self, request: &Settlement) -> Result<SettlementReceipt, Error> {
        if request.delivery.worker_id != self.worker_id {
            return Err(denied());
        }
        for successor in &request.successors {
            if successor.app_id != request.delivery.job.app_id {
                return Err(denied());
            }
            worker_publication(successor)?;
        }
        let receipt: SettlementReceipt = self
            .transport
            .post(endpoints::WORKFLOW_JOB_SETTLE, request)
            .await?;
        if receipt.app_id != request.delivery.job.app_id
            || receipt.job_id != request.delivery.job.id
            || receipt.attempt != request.delivery.attempt
            || receipt.outcome != request.outcome
        {
            return Err(Error::InvalidResponse);
        }
        Ok(receipt)
    }
}

const fn worker_publication(job: &JobSpec) -> Result<(), Error> {
    if matches!(
        job.operation,
        JobOperation::Cron { .. } | JobOperation::Management { .. }
    ) {
        Err(denied())
    } else {
        Ok(())
    }
}

const fn denied() -> Error {
    Error::Refused(FailureCode::Denied)
}
