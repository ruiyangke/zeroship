use super::{
    journal::{ClaimedDelivery, JobJournal, RenewDelivery, RenewedDelivery, SettleDelivery, SettledDelivery},
    Error, WorkerCoordinator,
};
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

    /// Claim an eligible job, capture its remaining delivery authority, and
    /// take the journal acceptance that came with it.
    ///
    /// ONE EXCHANGE, TWO HALVES. The acceptance is present exactly for the
    /// operation the journal accepts execution for, which this client decides
    /// from the delivery it was handed rather than from the reply's own shape:
    /// an acceptance beside a maintenance operation, or a missing one beside an
    /// advance, is a peer this contract does not describe.
    ///
    /// # Errors
    /// Refuses failed exchanges, substituted scope/worker/revision, a journal
    /// half that does not belong to the operation claimed, and grants exhausted
    /// by transport delay or outside the representable clock range.
    pub async fn claim_job<J: JobJournal>(
        &self,
        scope: &AssignedScope,
    ) -> Result<Option<ClaimedJob<J::Acceptance>>, Error> {
        let started = Instant::now();
        let claimed: Option<ClaimedDelivery<J::Acceptance>> = self
            .transport
            .post(endpoints::WORKFLOW_JOB_CLAIM, scope)
            .await?;
        claimed
            .map(|claimed| {
                let delivery = &claimed.lease.delivery;
                if delivery.worker_id != self.worker_id
                    || delivery.job.app_id != scope.app_id
                    || delivery.assignment_revision != scope.assignment_revision
                    || claimed.accepted.is_some() != executes(&delivery.job.operation)
                {
                    return Err(Error::InvalidResponse);
                }
                Ok(ClaimedJob {
                    lease: LeasedJob::received(claimed.lease, started)?,
                    accepted: claimed.accepted,
                })
            })
            .transpose()
    }

    /// Renew a delivery, and the journal task held under it, while the caller's
    /// previously confirmed local authority is live. A late reply cannot
    /// reactivate an expired execution.
    ///
    /// WHAT THE SPLIT PATH CHECKED HERE AND THIS ONE CANNOT. When these were two
    /// calls, the caller re-read its grant's remaining authority BETWEEN them,
    /// so a manager reply that arrived after the caller's own view of the grant
    /// had lapsed was never followed by a journal write. Merged, the journal
    /// half has already committed at the far end before this function resumes,
    /// so that window is admitted rather than refused. What still refuses it,
    /// one clock removed, is the manager's own `live` check on the stored lease
    /// deadline and the journal's capture of the grant it was handed: a lapsed
    /// delivery renews nothing at either end. The check below therefore reports
    /// to the caller rather than guarding the write.
    ///
    /// # Errors
    /// Refuses expired grants, another worker's delivery, failed exchanges, a
    /// renewal answered for a task the request did not name, and responses that
    /// substitute the immutable delivery identity.
    pub async fn heartbeat_job<J: JobJournal>(
        &self,
        job: &LeasedJob,
        task: Option<&J::Claim>,
    ) -> Result<RenewedJob<J::Renewal>, Error> {
        if job.delivery.worker_id != self.worker_id {
            return Err(denied());
        }
        job.remaining()?;
        let started = Instant::now();
        let renewed: RenewedDelivery<J::Renewal> = self
            .transport
            .post(
                endpoints::WORKFLOW_JOB_HEARTBEAT,
                &RenewDelivery {
                    delivery: job.delivery.clone(),
                    task,
                },
            )
            .await?;
        job.remaining()?;
        let observed = &renewed.lease.delivery;
        if observed.job != job.delivery.job
            || observed.worker_id != job.delivery.worker_id
            || observed.assignment_revision != job.delivery.assignment_revision
            || observed.attempt != job.delivery.attempt
            || renewed.renewal.is_some() != task.is_some()
        {
            return Err(Error::InvalidResponse);
        }
        Ok(RenewedJob {
            lease: LeasedJob::received(renewed.lease, started)?,
            renewal: renewed.renewal,
        })
    }

    /// Settle only a creator-committed outcome, preserving its successor IDs.
    /// An exact settled receipt may be retried after delivery expiry.
    ///
    /// # Errors
    /// Refuses foreign worker/app identities, incompatible outcome families,
    /// manager-owned successors, failed exchanges and substituted receipts.
    pub async fn settle_job(&self, request: &Settlement) -> Result<SettlementReceipt, Error> {
        if !request.outcome.valid_for(&request.delivery.job.operation) {
            return Err(Error::Refused(FailureCode::Invalid));
        }
        for successor in &request.successors {
            if successor.app_id != request.delivery.job.app_id {
                return Err(denied());
            }
            worker_publication(successor)?;
        }
        let settled: SettledDelivery<()> = self
            .settle::<(), ()>(SettleDelivery {
                delivery: request.delivery.clone(),
                outcome: Some(request.outcome.clone()),
                successors: request.successors.clone(),
                execution: None,
            })
            .await?;
        if settled.receipt.is_some() || settled.settlement.outcome != request.outcome {
            return Err(Error::InvalidResponse);
        }
        self.settled(&request.delivery, settled.settlement)
    }

    /// Commit an execution into the journal and settle the delivery with the
    /// outcome it produced, in one exchange.
    ///
    /// The outcome is NOT an argument: it is what the journal decides when it
    /// commits the batch, and it comes back on the settlement receipt. A caller
    /// that already holds a committed outcome settles it with
    /// [`Self::settle_job`] instead.
    ///
    /// # Errors
    /// Refuses another worker's delivery, failed exchanges, a settlement whose
    /// receipt does not match the delivery sent, an outcome family the operation
    /// does not admit, and a reply carrying no journal receipt.
    pub async fn settle_execution<J: JobJournal>(
        &self,
        delivery: &Delivery,
        execution: &J::Execution,
    ) -> Result<SettledJob<J::Receipt>, Error> {
        let settled: SettledDelivery<J::Receipt> = self
            .settle(SettleDelivery {
                delivery: delivery.clone(),
                outcome: None,
                successors: Vec::new(),
                execution: Some(execution),
            })
            .await?;
        let Some(receipt) = settled.receipt else {
            return Err(Error::InvalidResponse);
        };
        if !settled
            .settlement
            .outcome
            .valid_for(&delivery.job.operation)
        {
            return Err(Error::InvalidResponse);
        }
        Ok(SettledJob {
            settlement: self.settled(delivery, settled.settlement)?,
            receipt,
        })
    }

    /// The one bound-and-post both settlement shapes share.
    ///
    /// A settle body carries a journal quantity rather than a creator input, so
    /// it answers to `max_journal_request_bytes`. Every other exchange this
    /// client makes carries metadata or one creator input and answers to
    /// `max_request_bytes`.
    async fn settle<C: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        request: SettleDelivery<C>,
    ) -> Result<SettledDelivery<R>, Error> {
        if request.delivery.worker_id != self.worker_id {
            return Err(denied());
        }
        self.transport
            .post_journal(endpoints::WORKFLOW_JOB_SETTLE, &request)
            .await
    }

    fn settled(
        &self,
        delivery: &Delivery,
        receipt: SettlementReceipt,
    ) -> Result<SettlementReceipt, Error> {
        if receipt.app_id != delivery.job.app_id
            || receipt.job_id != delivery.job.id
            || receipt.attempt != delivery.attempt
        {
            return Err(Error::InvalidResponse);
        }
        Ok(receipt)
    }
}

/// A claimed delivery and the journal acceptance that rode with it.
#[derive(Clone, Debug)]
pub struct ClaimedJob<A> {
    pub lease: LeasedJob,
    pub accepted: Option<A>,
}

/// A renewed delivery and the journal renewal that rode with it.
#[derive(Clone, Debug)]
pub struct RenewedJob<R> {
    pub lease: LeasedJob,
    pub renewal: Option<R>,
}

/// A settled delivery and the journal receipt its execution committed.
#[derive(Clone, Debug)]
pub struct SettledJob<R> {
    pub settlement: SettlementReceipt,
    pub receipt: R,
}

/// Whether the journal accepts execution for an operation, and so whether a
/// claim of it carries an acceptance.
///
/// Every other kind is maintenance: the journal settles those itself, from the
/// lease alone, with no task and no executor.
const fn executes(operation: &JobOperation) -> bool {
    matches!(operation, JobOperation::Advance { .. })
}

/// Workers publish only creator intents. Activation, calendar, management,
/// closure and maintenance jobs are manager-origin.
const fn worker_publication(job: &JobSpec) -> Result<(), Error> {
    match job.operation {
        // Retention decisions are the manager's; a worker never asks for a
        // deployment to be given back.
        JobOperation::Activate { .. }
        | JobOperation::Cron { .. }
        | JobOperation::ReleaseHold { .. }
        | JobOperation::Management { .. }
        | JobOperation::Close { .. }
        | JobOperation::Reconcile {}
        | JobOperation::Collect {} => Err(denied()),
        JobOperation::Advance { .. }
        | JobOperation::Fanout { .. }
        | JobOperation::Propagate { .. } => Ok(()),
    }
}

const fn denied() -> Error {
    Error::Refused(FailureCode::Denied)
}
