use super::{
    journal::{
        JobJournal, JobReceiptQuery, ReleaseDelivery, RenewDelivery, RenewedDelivery,
        SettleDelivery,
    },
    Error, WorkerCoordinator,
};
use std::time::{Duration, Instant};
use zeroship_core::{
    service_identity::endpoints,
    typed_id,
    workflow_coordination::{
        FailureCode, PayloadLocation, PayloadReservation, PinnedDeployment, ReadTaskPayload,
        ReservePayload, ResolveTaskExecutable,
    },
    workflow_jobs::{
        ClaimJobs, ClaimedJobs, Delivery, DeliveryLease, JobLease, JobSpec, SettlementReceipt,
    },
};

/// A validated delivery with authority measured on this process's monotonic clock.
/// Cloning preserves its expiration; the wire deadline is diagnostic metadata.
#[derive(Clone, Debug)]
pub struct LeasedJob {
    delivery: Delivery,
    expires: Instant,
    attempt_expires: Instant,
}

impl JobLease for LeasedJob {
    fn delivery(&self) -> &Delivery {
        &self.delivery
    }

    fn remaining(&self) -> Option<Duration> {
        Self::remaining(self).ok()
    }

    fn attempt_remaining(&self) -> Option<Duration> {
        Self::attempt_remaining(self).ok()
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

    /// Remaining authority for the whole attempt, across heartbeats.
    pub fn attempt_remaining(&self) -> Result<Duration, Error> {
        self.attempt_expires
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(Error::Timeout)
    }

    fn received(lease: DeliveryLease, request_started: Instant) -> Result<Self, Error> {
        // Manager database timestamps cannot express a larger grant. Validate
        // before conversion rather than accepting an effectively unbounded lease.
        i64::try_from(lease.remaining_ms.get()).map_err(|_| Error::InvalidResponse)?;
        i64::try_from(lease.attempt_remaining_ms.get()).map_err(|_| Error::InvalidResponse)?;
        let expires = request_started
            .checked_add(Duration::from_millis(lease.remaining_ms.get()))
            .ok_or(Error::InvalidResponse)?;
        let attempt_expires = request_started
            .checked_add(Duration::from_millis(lease.attempt_remaining_ms.get()))
            .ok_or(Error::InvalidResponse)?;
        let job = Self {
            delivery: lease.delivery,
            expires,
            attempt_expires,
        };
        job.remaining()?;
        job.attempt_remaining()?;
        Ok(job)
    }
}

impl WorkerCoordinator {
    /// Claim a batch of jobs across this worker's zone, capture each delivery's
    /// remaining authority, and take the journal acceptance that came with it.
    ///
    /// ONE EXCHANGE, TWO HALVES. The acceptance is present exactly for the
    /// operation the journal accepts execution for, which this client decides
    /// from the delivery it was handed rather than from the reply's own shape:
    /// an acceptance beside a maintenance operation, or a missing one beside an
    /// advance, is a peer this contract does not describe.
    ///
    /// THE EXCHANGE LASTS EXACTLY THE REQUEST'S WAIT. The service works for a
    /// claim until a deadline it derives from `wait_ms` and replies inside it,
    /// so this side waits `wait_ms` and not its generic exchange bound: giving
    /// up earlier would strand every delivery the service committed.
    ///
    /// # Errors
    /// Refuses failed exchanges, a reply holding more deliveries than were
    /// asked for or a delivery to another worker, a journal half that does not
    /// belong to the operation claimed, and grants outside the representable
    /// clock range. A grant exhausted by transport delay is that delivery's
    /// alone, reported in [`ClaimedJobBatch::lapsed`].
    pub async fn claim_jobs<J: JobJournal>(
        &self,
        request: &ClaimJobs,
    ) -> Result<ClaimedJobBatch<J::Acceptance>, Error> {
        let started = Instant::now();
        let claimed: ClaimedJobs<J::Acceptance> = self
            .transport
            .post_within(
                endpoints::WORKFLOW_JOB_CLAIM,
                request,
                Duration::from_millis(request.wait_ms.get()),
            )
            .await?;
        if claimed.deliveries.len() > usize::try_from(request.max.get()).unwrap_or(usize::MAX) {
            return Err(Error::InvalidResponse);
        }
        let mut deliveries = Vec::with_capacity(claimed.deliveries.len());
        let mut lapsed = Vec::new();
        for claimed in claimed.deliveries {
            let delivery = &claimed.lease.delivery;
            if delivery.worker_id != self.worker_id
                || claimed.accepted.is_some() != delivery.job.operation.accepts_execution()
            {
                return Err(Error::InvalidResponse);
            }
            // A lease spent in transit is that delivery's loss alone: the
            // others in the reply are live, and refusing the whole reply would
            // strand every one of them.
            let delivery = delivery.clone();
            match LeasedJob::received(claimed.lease, started) {
                Ok(lease) => deliveries.push(ClaimedJob {
                    lease,
                    accepted: claimed.accepted,
                    started,
                }),
                Err(Error::Timeout) => lapsed.push(delivery),
                Err(error) => return Err(error),
            }
        }
        Ok(ClaimedJobBatch {
            deliveries,
            lapsed,
            after: claimed.after,
            lap_complete: claimed.lap_complete,
        })
    }

    /// Renew a delivery, and the journal task held under it, while the caller's
    /// previously confirmed local authority is live. A late reply cannot
    /// reactivate an expired execution.
    ///
    /// THE LOCAL AUTHORITY CHECK REPORTS; IT DOES NOT GATE. Both halves commit at
    /// the far end before this function resumes, so a reply that arrives after
    /// this process's own view of the grant has lapsed finds the journal already
    /// renewed. What refuses a lapsed delivery is one clock removed: the
    /// manager's `live` check against the stored lease deadline, and the
    /// journal's capture of the grant it is handed, which refuses a grant with no
    /// remaining authority before it opens a transaction. This side's reads bound
    /// what it will ASK for.
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
            || observed.attempt != job.delivery.attempt
            || renewed.renewal.is_some() != task.is_some()
        {
            return Err(Error::InvalidResponse);
        }
        Ok(RenewedJob {
            lease: LeasedJob::received(renewed.lease, started)?,
            renewal: renewed.renewal,
            started,
        })
    }

    /// Settle a delivery whose job the journal has already committed, with the
    /// outcome the journal holds for it. An exact settled receipt may be retried
    /// after delivery expiry.
    ///
    /// NOTHING ABOUT THE OUTCOME IS SENT. The service reads the receipt from its
    /// own journal and settles the queue with that, so this is the recovery for a
    /// holder whose settlement reply was lost and for one whose claim found the
    /// job already committed. A job the journal holds no receipt for is refused
    /// as `Conflict`.
    ///
    /// # Errors
    /// Refuses another worker's delivery, failed exchanges, a receipt that does
    /// not match the delivery sent, and an outcome family the operation does not
    /// admit.
    pub async fn settle_committed(&self, delivery: &Delivery) -> Result<SettlementReceipt, Error> {
        self.settle::<()>(delivery, None).await
    }

    /// Commit an execution into the journal and settle the delivery with the
    /// outcome it produced, in one exchange.
    ///
    /// The outcome is NOT an argument: it is what the journal decides when it
    /// commits the batch, and it comes back on the settlement receipt. A holder
    /// whose execution already committed settles it with
    /// [`Self::settle_committed`] instead.
    ///
    /// # Errors
    /// Refuses another worker's delivery, failed exchanges, a receipt that does
    /// not match the delivery sent, and an outcome family the operation does not
    /// admit.
    pub async fn settle_execution<J: JobJournal>(
        &self,
        delivery: &Delivery,
        execution: &J::Execution,
    ) -> Result<SettlementReceipt, Error> {
        self.settle(delivery, Some(execution)).await
    }

    /// The one bound, post and reply check both settlement shapes share.
    ///
    /// A settle body carries a journal quantity rather than a creator input, so
    /// it answers to `max_journal_request_bytes`. Every other exchange this
    /// client makes carries metadata or one creator input and answers to
    /// `max_request_bytes`.
    ///
    /// The outcome arrives rather than being sent, in both shapes, so it is
    /// checked against the operation it answers for: the caller has no value of
    /// its own to compare it with.
    async fn settle<C: serde::Serialize>(
        &self,
        delivery: &Delivery,
        execution: Option<&C>,
    ) -> Result<SettlementReceipt, Error> {
        if delivery.worker_id != self.worker_id {
            return Err(denied());
        }
        let receipt: SettlementReceipt = self
            .transport
            .post_journal(
                endpoints::WORKFLOW_JOB_SETTLE,
                &SettleDelivery {
                    delivery: delivery.clone(),
                    execution,
                },
            )
            .await?;
        if !receipt.outcome.valid_for(&delivery.job.operation) {
            return Err(Error::InvalidResponse);
        }
        self.settled(delivery, receipt)
    }

    /// Locate the object one replay edge of a live dispatch names.
    ///
    /// TWO PHASES, and this is the first. The bytes cannot cross -- a payload
    /// answers to its own ceiling and a reply to the smaller journal one -- so
    /// what comes back is the key the journal proved this task owns, plus the
    /// descriptor those bytes must satisfy. The caller opens the object from the
    /// store it binds under the same namespace.
    ///
    /// THE WORKER IS NOT IN THE REQUEST. The credential that signs it is what
    /// the service substitutes into the task lookup, so a caller cannot read
    /// another worker's dispatch by naming it.
    ///
    /// # Errors
    /// Refuses failed exchanges, the journal's own refusals, and a reply whose
    /// key is not a workflow payload id.
    pub async fn read_task_payload(
        &self,
        request: &ReadTaskPayload,
    ) -> Result<PayloadLocation, Error> {
        let located: PayloadLocation = self
            .transport
            .post(endpoints::WORKFLOW_TASK_PAYLOAD, request)
            .await?;
        // A key addresses an object store, so it is parsed against the one prefix
        // a payload may carry before any caller uses it as one. The descriptor is
        // NOT checked here: `read_verified` compares it against the one this call
        // asked for, which is a stronger check than a well-formedness one and is
        // the caller's to make against its own request.
        typed_id::parse_with_prefix(&located.payload_id, typed_id::WORKFLOW_PAYLOAD_PREFIX)
            .map_err(|_| Error::InvalidResponse)?;
        Ok(located)
    }

    /// Resolve which deployment a live dispatch replays against.
    ///
    /// TWO PHASES, like [`Self::read_task_payload`], and for a harder reason: an
    /// executable's module source answers to a budget twice the journal reply
    /// ceiling, so the artifact cannot cross at all. This asks WHICH deployment,
    /// and the caller loads it from the object store it already holds.
    ///
    /// The two fence values in the reply are not for the caller to read. They
    /// are echoed back with the execution it reports, so the journal can refuse a
    /// settlement produced against a deployment that was parked or re-admitted
    /// while creator code ran.
    ///
    /// # Errors
    /// Refuses failed exchanges and the journal's own refusals.
    pub async fn resolve_task_executable(
        &self,
        request: &ResolveTaskExecutable,
    ) -> Result<PinnedDeployment, Error> {
        self.transport
            .post(endpoints::WORKFLOW_TASK_EXECUTABLE, request)
            .await
    }

    /// Hand a claimed journal task back without settling its delivery.
    ///
    /// A release gives up creator work this holder cannot finish. The delivery
    /// stays unsettled on purpose: the journal reopens the run and the service
    /// returns the queue row in the same request, as `reason` says - at once with
    /// the attempt counted for an interrupted attempt, after a back-off for an
    /// app that could not be prepared. So there is no receipt to check and
    /// nothing to compare -- the reply is the acknowledgement.
    ///
    /// # Errors
    /// Refuses another worker's delivery, an expired grant and failed exchanges.
    pub async fn release_job<J: JobJournal>(
        &self,
        job: &LeasedJob,
        task: &J::Claim,
        reason: crate::GiveBackReason,
    ) -> Result<(), Error> {
        if job.delivery.worker_id != self.worker_id {
            return Err(denied());
        }
        job.remaining()?;
        self.transport
            .post_journal(
                endpoints::WORKFLOW_JOB_RELEASE,
                &ReleaseDelivery {
                    delivery: job.delivery.clone(),
                    task: Some(task),
                    reason,
                },
            )
            .await
    }

    /// Return a delivery whose app could not be prepared before journal acceptance.
    pub async fn give_back_job<J: JobJournal>(
        &self,
        job: &LeasedJob,
        reason: crate::GiveBackReason,
    ) -> Result<(), Error> {
        if job.delivery.worker_id != self.worker_id {
            return Err(denied());
        }
        job.remaining()?;
        self.transport
            .post_journal::<_, ()>(
                endpoints::WORKFLOW_JOB_RELEASE,
                &ReleaseDelivery::<J::Claim> {
                    delivery: job.delivery.clone(),
                    task: None,
                    reason,
                },
            )
            .await
    }

    /// Read the committed outcome of one logical job, if any attempt committed.
    ///
    /// THE RECOVERY READ, and the reason it exists is that a settlement whose
    /// reply was lost may have committed. A holder that retried blindly could
    /// commit twice; one that gives up could abandon work already done. This
    /// answers which happened, so the retry can settle the queue half alone.
    ///
    /// Absent means no attempt has committed one, which is a fact rather than a
    /// refusal -- so it is `Ok(None)` and not an error.
    ///
    /// # Errors
    /// Refuses failed exchanges and the journal's own refusals.
    pub async fn job_receipt<J: JobJournal>(
        &self,
        job: &JobSpec,
    ) -> Result<Option<J::Receipt>, Error> {
        self.transport
            .post_journal(
                endpoints::WORKFLOW_JOB_RECEIPT,
                &JobReceiptQuery { job: job.clone() },
            )
            .await
    }

    /// Reserve the row an upload will be keyed by.
    ///
    /// THE BYTES DO NOT CROSS. This asks which payload id to write under; the
    /// caller then writes the object to the store it already binds and confirms
    /// afterwards. The split exists because staging in one process holds a lock
    /// across the object write so collection cannot race a live writer, and no
    /// request boundary can hold that lock.
    ///
    /// A RETRY MUST SEND THE SAME `request_id`. The service deduplicates on it,
    /// and a caller minting a fresh one per attempt reserves a new object each
    /// time without anything failing.
    ///
    /// # Errors
    /// Refuses failed exchanges, the journal's own refusals, and a reservation
    /// whose key is not a workflow payload id.
    pub async fn reserve_task_payload(
        &self,
        request: &ReservePayload,
    ) -> Result<PayloadReservation, Error> {
        let reserved: PayloadReservation = self
            .transport
            .post(endpoints::WORKFLOW_TASK_PAYLOAD_RESERVE, request)
            .await?;
        // A reservation is used as an object-store key, so it is parsed against
        // the one prefix a payload may carry before any caller writes under it.
        let payload_id = match &reserved {
            PayloadReservation::Reserved { payload_id, .. }
            | PayloadReservation::Staged { payload_id } => payload_id,
        };
        typed_id::parse_with_prefix(payload_id, typed_id::WORKFLOW_PAYLOAD_PREFIX)
            .map_err(|_| Error::InvalidResponse)?;
        Ok(reserved)
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
    /// When this exchange began, which is the anchor the lease's own expiration
    /// was measured from.
    ///
    /// The journal half carries a remaining duration too, and it has to be
    /// anchored HERE rather than on arrival: anchoring later would add the
    /// transport delay to the authority instead of charging it, and anchoring it
    /// somewhere else than the lease would leave one half of a single grant
    /// outliving the other.
    pub started: Instant,
}

/// A validated zone claim with its continuation cursor.
#[derive(Clone, Debug)]
pub struct ClaimedJobBatch<A> {
    pub deliveries: Vec<ClaimedJob<A>>,
    /// Deliveries whose lease was spent by the time the reply arrived. Nothing
    /// can renew or give them back; each lapses and is redelivered.
    pub lapsed: Vec<Delivery>,
    pub after: Option<zeroship_core::app_id::AppId>,
    pub lap_complete: bool,
}

/// A renewed delivery and the journal renewal that rode with it.
#[derive(Clone, Debug)]
pub struct RenewedJob<R> {
    pub lease: LeasedJob,
    pub renewal: Option<R>,
    /// When this exchange began; see [`ClaimedJob::started`].
    pub started: Instant,
}

const fn denied() -> Error {
    Error::Refused(FailureCode::Denied)
}
