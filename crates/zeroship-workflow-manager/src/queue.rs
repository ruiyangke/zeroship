#![expect(
    clippy::future_not_send,
    reason = "native ORM transactions stay on their owning compio thread"
)]

use crate::{
    clock::{Clock, Sample, RESOLUTION_MILLIS},
    error::Error,
    models::{self, jobs, queue_scopes, Claimant, Job, Scope},
    retention::{self, Retention},
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    future::{poll_fn, Future},
    io::Write,
    num::{NonZeroU64, NonZeroUsize},
    rc::Rc,
    task::Poll,
    time::{Duration, Instant},
};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::WorkerId,
    workflow_jobs::{
        valid_outcome, Delivery, DeliveryLease, DeploymentId, JobLease, JobSpec,
        JournalSettlement, SettlementReceipt,
    },
    zone_id::ZoneId,
};
use zeroship_data_orm::{
    binding::DbBinding,
    encryption::ProjectKeySource,
    error::DbError,
    orm::{Database, Entity, FindOptions, Operation, Output},
    value, ConnectOptions, Value,
};

/// Bounds leases and transaction waits.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub max_connections: NonZeroUsize,
    pub lease: Duration,
    pub max_attempt: Duration,
    pub transaction_timeout: Duration,
    pub max_metadata_bytes: usize,
    pub defer_backoff: Duration,
    pub defer_backoff_max: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_connections: NonZeroUsize::new(8).unwrap(),
            lease: Duration::from_secs(30),
            max_attempt: Duration::from_secs(300),
            transaction_timeout: Duration::from_secs(5),
            max_metadata_bytes: 256 * 1024,
            defer_backoff: Duration::from_millis(100),
            defer_backoff_max: Duration::from_secs(60),
        }
    }
}

impl Options {
    /// The bounds [`Queue::connect`] refuses, checked without opening anything,
    /// so a host's configuration check refuses what its startup would.
    ///
    /// One attempt spans at least one lease: `max_attempt` caps how far
    /// heartbeats may extend a delivery, so a cap shorter than the lease would
    /// cut every attempt before its first renewal was due.
    ///
    /// # Errors
    /// Refuses empty or unrepresentable durations, an attempt cap shorter than
    /// the lease and a back-off ceiling below its base.
    pub fn validate(&self) -> Result<(), Error> {
        if self.transaction_timeout.is_zero()
            || Instant::now()
                .checked_add(self.transaction_timeout)
                .is_none()
            || self.max_metadata_bytes == 0
            || self.lease.as_millis() == 0
            || self.max_attempt < self.lease
            || self.max_attempt.as_millis() == 0
            || self.defer_backoff.is_zero()
            || self.defer_backoff_max < self.defer_backoff
            || i64::try_from(self.lease.as_millis()).is_err()
            || i64::try_from(self.max_attempt.as_millis()).is_err()
            || i64::try_from(self.defer_backoff_max.as_millis()).is_err()
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}

/// Durable platform metadata with app-scoped delivery fences.
///
/// Hosts authenticate workers before calling delivery operations. The
/// authorized variants recheck enrollment after locks and before commit.
/// A timeout does not retract a dispatched commit; retry the same settlement to
/// recover its durable receipt.
#[derive(Clone, Debug)]
pub struct Queue {
    pub(crate) database: Database,
    pub(crate) clock: Clock,
    pub(crate) options: Options,
    pub(crate) holds: Rc<dyn crate::retention::HoldClient>,
}

/// How a delivery returns to the ready queue without settling.
///
/// Every variant clears the holder and returns the row to `ready`. The deferral
/// variants keep it unclaimable for a while and count no attempt: nothing
/// executed, so a job that is deferred or unpreparable forever never exhausts its
/// delivery budget. An interrupted attempt is the opposite case, claimable at
/// once and counted; an unsent delivery is claimable at once and counts nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GiveBack {
    /// Unclaimable until exactly this instant on the queue's database clock.
    Exact(i64),
    /// Unclaimable for this long from the give-back.
    After(Duration),
    /// Unclaimable for a pause that doubles with each consecutive back-off,
    /// from [`Options::defer_backoff`] up to [`Options::defer_backoff_max`].
    /// The only give-back that lengthens the next one.
    Backoff,
    /// An attempt that began executing and stopped without a receipt. The row
    /// is claimable at once and the attempt counts toward the delivery budget,
    /// exactly once: an attempt its first renewal already counted is not
    /// counted again. Counting it is what stops an execution that fails before
    /// its first renewal from being redelivered without end.
    Interrupted,
    /// A delivery that reached no execution through no fault of the job: the
    /// reply it would have joined was full, its task reached the holder with
    /// no time left, or its holder stopped before starting it. The row is
    /// claimable at once, counts no attempt and leaves its back-off as it was.
    Unsent,
}

/// How an operation takes an app's queue lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScopeLock {
    /// Wait for it, within the operation's budget.
    Wait,
    /// Refuse at once as [`Error::Contended`] when another transaction holds
    /// it, on PostgreSQL. SQLite has no row locks, so there this waits like
    /// [`Self::Wait`] and ends as `Unavailable` or `Timeout`.
    Skip,
}

/// What a claim transaction found under the app's lock.
#[derive(Debug)]
pub(crate) enum Claimed {
    /// A delivery committed for the caller. Boxed: a grant is several times
    /// the size of the other answers, which carry nothing.
    Granted(Box<DeliveryGrant>),
    /// No row the caller may take.
    Empty,
    /// As many live deliveries as the caller's concurrency cap allows,
    /// counted under the lock: another claim leased the last slot after this
    /// caller's lock-free count.
    AtCap,
}

struct PreparedSettlement {
    digest: String,
}

/// A committed delivery with the manager's original monotonic lease budget.
/// Response construction must consume only the authority still remaining.
#[derive(Debug, Clone)]
pub struct DeliveryGrant {
    delivery: Delivery,
    expires_at: Instant,
    attempt_expires_at: Instant,
}

impl JobLease for DeliveryGrant {
    fn delivery(&self) -> &Delivery {
        &self.delivery
    }

    fn remaining(&self) -> Option<Duration> {
        self.expires_at
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
    }

    fn attempt_remaining(&self) -> Option<Duration> {
        self.attempt_expires_at
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
    }
}

impl DeliveryGrant {
    /// Read the immutable identity covered by this grant's lease authority.
    #[must_use]
    pub const fn delivery(&self) -> &Delivery {
        &self.delivery
    }

    fn new(delivery: Delivery, sample: Sample, attempt_deadline: i64) -> Result<Self, Error> {
        let expires_at = local_deadline(sample, delivery.deadline.get())?;
        let attempt_expires_at = local_deadline(sample, attempt_deadline)?;
        if expires_at <= Instant::now() {
            return Err(Error::Timeout);
        }
        Ok(Self {
            delivery,
            expires_at,
            attempt_expires_at,
        })
    }

    fn cap(&mut self, sample: Sample) -> Result<(), Error> {
        self.expires_at = self
            .expires_at
            .min(local_deadline(sample, self.delivery.deadline.get())?)
            .min(self.attempt_expires_at);
        if self.expires_at <= Instant::now() {
            return Err(Error::Timeout);
        }
        Ok(())
    }

    /// Convert to wire authority after commit, charging all intervening waits.
    ///
    /// # Errors
    /// Refuses exhausted or unrepresentable remaining authority.
    pub fn lease(&self) -> Result<DeliveryLease, Error> {
        let remaining = self.expires_at.saturating_duration_since(Instant::now());
        let remaining_ms = u64::try_from(remaining.as_millis())
            .ok()
            .and_then(NonZeroU64::new)
            .ok_or(Error::Timeout)?;
        let attempt_remaining = self
            .attempt_expires_at
            .saturating_duration_since(Instant::now());
        let attempt_remaining_ms = u64::try_from(attempt_remaining.as_millis())
            .ok()
            .and_then(NonZeroU64::new)
            .ok_or(Error::Timeout)?;
        Ok(DeliveryLease {
            delivery: self.delivery.clone(),
            remaining_ms,
            attempt_remaining_ms,
        })
    }
}

impl Queue {
    /// Bind provisioned platform storage without creating schemas or roles.
    ///
    /// # Errors
    /// Refuses invalid bounds and unavailable or incompatible storage.
    pub async fn connect(
        binding: DbBinding,
        url: &str,
        options: Options,
        holds: Rc<dyn crate::retention::HoldClient>,
    ) -> Result<Self, Error> {
        options.validate()?;
        let database = Database::connect(
            binding.clone(),
            ConnectOptions::new(url, ProjectKeySource::unavailable())
                .max_connections(options.max_connections)
                .connection_authority(),
            models::collections()?,
        )
        .await?;
        for collection in [
            jobs::Entity::COLLECTION,
            queue_scopes::Entity::COLLECTION,
            models::schema::deployment_holds::Entity::COLLECTION,
        ] {
            database
                .collection(collection)?
                .find(value!({}), value!({"limit":1}))
                .await?;
        }
        let clock = Clock::connect(binding, url, options.transaction_timeout).await?;
        Ok(Self {
            database,
            clock,
            options,
            holds,
        })
    }

    /// The queue's database clock, which every stored deadline is stated in.
    ///
    /// # Errors
    /// Reports an unreachable or unreadable clock connection.
    pub async fn now(&self) -> Result<i64, Error> {
        self.clock.now().await
    }

    /// The lease window this queue grants a claimed delivery.
    #[must_use]
    pub const fn lease(&self) -> Duration {
        self.options.lease
    }

    /// The apps whose queue holds a row `claimant` may take, in app id order
    /// after `after` and up to `upper`, and how many rows the scan fetched.
    /// Passing `descending` with a limit of one reads a sweep's upper bound.
    ///
    /// `limit` bounds the ROWS, so the answer holds at most that many apps and
    /// commonly fewer: one app's claimable rows are adjacent in this order and
    /// collapse to one entry. A fetch that reached the limit is the caller's
    /// signal that the scan has more to page through.
    ///
    /// The enumeration a host runs BEFORE claiming, so it reads only what needs
    /// no app lock: row state, availability and the kinds this claimant
    /// refuses. Occurrence gating, management barriers and the delivery ceiling
    /// stay inside [`Self::claim_authorized`], which is where a claim is
    /// decided; an app listed here can still have nothing that claimant takes,
    /// and asking is the only way to find out.
    ///
    /// # Errors
    /// Reports an unreachable clock, a stored app id this crate did not mint,
    /// and failed transactions.
    pub async fn claimable_apps(
        &self,
        claimant: Claimant,
        after: Option<&str>,
        upper: Option<&str>,
        descending: bool,
        limit: u32,
    ) -> Result<(Vec<AppId>, usize), Error> {
        self.transact(|tx| async move {
            let now = self.clock.now().await?;
            let rows = crate::scheduling::claimable_apps_in(
                &tx, now, claimant, after, upper, descending, limit,
            )
            .await?;
            let fetched = rows.len();
            let mut apps: Vec<AppId> = Vec::with_capacity(fetched);
            for row in rows {
                let app = AppId::parse(&row).map_err(|_| Error::Storage)?;
                if apps.last() != Some(&app) {
                    apps.push(app);
                }
            }
            Ok((apps, fetched))
        })
        .await
    }

    /// Register an app selected by trusted platform configuration.
    ///
    /// # Errors
    /// Refuses failed transactions; repeated registration preserves queue state.
    pub async fn register_scope(&self, app: &AppId, zone: &ZoneId) -> Result<(), Error> {
        self.transact(|tx| async move { register_scope_in(&tx, app, zone).await })
            .await
    }

    /// Submit immutable metadata from a trusted manager operation. A creator
    /// journal's committed intents reach the queue through this path, with the
    /// publication seam asserting the app before it calls here.
    ///
    /// # Errors
    /// Refuses unknown apps, reused identities with different content and storage failures.
    pub async fn submit(&self, job: &JobSpec) -> Result<JobSpec, Error> {
        if matches!(
            job.operation,
            zeroship_core::workflow_jobs::JobOperation::Management { .. }
                | zeroship_core::workflow_jobs::JobOperation::Close { .. }
        ) {
            return Err(Error::Invalid);
        }
        self.encode(job)?;
        let budget = Budget::new(self.options.transaction_timeout);
        loop {
            let result = self
                .transact_for(budget.clone(), |tx| async move {
                    lock_scope(&tx, &job.app_id).await?;
                    if !self.existing(&tx, job).await? {
                        if let Some(deployment) = job.deployment_id() {
                            if !retention::prepared(&tx, &job.app_id, deployment).await? {
                                return Ok(Retention::Acquire(deployment.clone()));
                            }
                        }
                    }
                    self.insert(&tx, job, self.clock.now().await?).await?;
                    Ok(Retention::Ready(job.clone()))
                })
                .await?;
            match result {
                Retention::Ready(job) => return Ok(job),
                Retention::Acquire(deployment) => {
                    self.ensure_deployment_for(&job.app_id, &deployment, budget.clone())
                        .await?;
                }
            }
        }
    }

    /// Revalidate enrollment after acquiring the app lock and before commit.
    /// Each authorization callback receives the active transaction for scoped reads.
    ///
    /// A job whose counted executions have reached `max_delivery_attempts` is no
    /// longer a candidate, so a body that never completes stops being redelivered.
    /// The exhausted row keeps its attempt history; nothing settles it, because
    /// no executor produced an outcome for it.
    ///
    /// The ceiling arrives as its caller's already-settled result, and is read
    /// only once the app's lock and the caller's enrollment hold. A policy
    /// authority that cannot answer for an app therefore never preempts a
    /// refusal of the caller itself, and a caller that is refused is told so
    /// rather than told to retry.
    ///
    /// `claimant` names the host asking, and the kinds it refuses are excluded
    /// from candidate selection rather than from the claimed row, so a refused
    /// row at the front of the dispatch order does not hide the rows behind it.
    ///
    /// The app's lock is WAITED FOR, within the transaction timeout: the
    /// maintenance lane and management commands claim one app at a time, and
    /// one that found the app busy would only have to ask again. The worker's
    /// zone claim is the one caller that passes a busy app instead.
    ///
    /// # Errors
    /// Refuses revoked enrollment, unreadable or invalid ceilings, exhausted
    /// attempt numbering and failed transactions.
    pub async fn claim_authorized<F, Fut>(
        &self,
        app: &AppId,
        worker: &WorkerId,
        claimant: Claimant,
        max_delivery_attempts: Result<i64, Error>,
        authorize: F,
    ) -> Result<Option<DeliveryGrant>, Error>
    where
        F: FnMut(Database) -> Fut,
        Fut: Future<Output = Result<WorkerId, Error>>,
    {
        let claimed = self
            .claim_within(
                Budget::new(self.options.transaction_timeout),
                ScopeLock::Wait,
                app,
                worker,
                claimant,
                max_delivery_attempts,
                None,
                authorize,
            )
            .await?;
        Ok(match claimed {
            Claimed::Granted(grant) => Some(*grant),
            Claimed::Empty | Claimed::AtCap => None,
        })
    }

    /// [`Self::claim_authorized`] under a caller's budget and lock, which a
    /// zone claim caps at its batch deadline and takes without waiting.
    ///
    /// `max_running`, when given, is counted again under the lock against the
    /// app's live creator deliveries: a lock-free count that admitted the app
    /// can be overtaken by another claim that leased its last slot meanwhile.
    #[expect(
        clippy::too_many_arguments,
        reason = "one claim's budget, lock, caller, ceilings and enrollment check"
    )]
    pub(crate) async fn claim_within<F, Fut>(
        &self,
        budget: Budget,
        lock: ScopeLock,
        app: &AppId,
        worker: &WorkerId,
        claimant: Claimant,
        max_delivery_attempts: Result<i64, Error>,
        max_running: Option<i64>,
        mut authorize: F,
    ) -> Result<Claimed, Error>
    where
        F: FnMut(Database) -> Fut,
        Fut: Future<Output = Result<WorkerId, Error>>,
    {
        self.transact_for(budget.clone(), |tx| async move {
            lock_scope_as(&tx, app, lock).await?;
            require_worker(worker, authorize(tx.clone()).await?)?;
            let sample = self.clock.sample().await?;
            let max_delivery_attempts = max_delivery_attempts?;
            if max_delivery_attempts <= 0 {
                return Err(Error::Invalid);
            }
            let now = sample.millis;
            crate::management::validate_pending(&tx, app).await?;
            if let Some(cap) = max_running {
                if crate::scheduling::live_advance_count(&tx, app, now).await? >= cap {
                    require_worker(worker, authorize(tx.clone()).await?)?;
                    return Ok(Claimed::AtCap);
                }
            }
            let Some(id) = Box::pin(crate::scheduling::candidate(
                &tx,
                app,
                now,
                claimant,
                Some(max_delivery_attempts),
            ))
            .await?
            else {
                require_worker(worker, authorize(tx.clone()).await?)?;
                return Ok(Claimed::Empty);
            };
            let job = load(&tx, app, &id).await?.ok_or(Error::Storage)?;
            if let Some(deployment) = job.spec()?.deployment_id() {
                retention::require_held(&tx, app, deployment).await?;
            }
            crate::scheduling::validate_delivery(&tx, &job).await?;
            crate::management::validate_job(&tx, &job.spec()?, true).await?;
            let attempt = job.attempt.checked_add(1).filter(|value| *value > 0)
                .ok_or(Error::Capacity)?;
            let sample = self.clock.sample().await?;
            let leased_at = sample.millis;
            let attempt_deadline = self.attempt_deadline(leased_at)?;
            let deadline = self.deadline(leased_at, attempt_deadline)?;
            budget.cap(sample, deadline)?;
            let delivery = Delivery {
                job: job.spec()?, worker_id: worker.clone(),
                attempt: attempt.try_into().map_err(|_| Error::Storage)?,
                deadline: deadline.try_into().map_err(|_| Error::Storage)?,
            };
            let mut grant = DeliveryGrant::new(delivery, sample, attempt_deadline)?;
            let dispatch_order = next_dispatch_order(&tx, app, Some(job.dispatch_order)).await?;
            update(&tx,
                value!({"id":id,"app_id":app.as_str(),"state":job.state,"attempt":job.attempt}),
                value!({"state":"leased","attempt":attempt,"worker_id":worker.as_str(),
                    "lease_deadline":deadline,"leased_at":leased_at,"deferred_until":null,
                    "dispatch_order":dispatch_order})
            ).await?;
            // Responsibility must exist before an intent-producing job executes.
            // The reopen shares this claim's app lock and rolls back with it.
            Box::pin(crate::recovery::claimed_in(&tx, &grant.delivery().job, now)).await?;
            require_worker(worker, authorize(tx.clone()).await?)?;
            let sample = self.clock.sample().await?;
            if sample.millis >= deadline {
                return Err(Error::Denied);
            }
            budget.cap(sample, deadline)?;
            grant.cap(sample)?;
            Ok(Claimed::Granted(Box::new(grant)))
        }).await
    }

    /// Revalidate enrollment while extending the stored delivery lease.
    /// Each authorization callback receives the active transaction for scoped reads.
    ///
    /// # Errors
    /// Refuses stale delivery identity, revoked enrollment and failed transactions.
    pub async fn heartbeat_authorized<F, Fut>(
        &self,
        worker: &WorkerId,
        delivery: &Delivery,
        mut authorize: F,
    ) -> Result<DeliveryGrant, Error>
    where
        F: FnMut(Database) -> Fut,
        Fut: Future<Output = Result<WorkerId, Error>>,
    {
        bound(worker, delivery)?;
        let budget = Budget::new(self.options.transaction_timeout);
        self.transact_for(budget.clone(), |tx| async move {
            lock_scope(&tx, &delivery.job.app_id).await?;
            require_worker(worker, authorize(tx.clone()).await?)?;
            let job = load(&tx, &delivery.job.app_id, delivery.job.id.as_str())
                .await?
                .ok_or(Error::Conflict)?;
            matches_delivery(&job, delivery)?;
            crate::management::validate_job(&tx, &delivery.job, true).await?;
            if let Some(deployment) = delivery.job.deployment_id() {
                retention::require_held(&tx, &delivery.job.app_id, deployment).await?;
            }
            let sample = self.clock.sample().await?;
            live(&job, sample.millis)?;
            budget.cap(
                sample,
                job.lease_deadline.ok_or(Error::Storage)?,
            )?;
            let attempt_deadline = attempt_deadline(&job, self.options.max_attempt)?;
            let deadline = self.deadline(sample.millis, attempt_deadline)?;
            let mut grant = DeliveryGrant::new(
                Delivery {
                    deadline: deadline.try_into().map_err(|_| Error::Storage)?,
                    ..delivery.clone()
                },
                sample,
                attempt_deadline,
            )?;
            update(&tx, fence(delivery), renewal(&job, delivery, deadline)?).await?;
            require_worker(worker, authorize(tx.clone()).await?)?;
            let sample = self.clock.sample().await?;
            if sample.millis >= deadline {
                return Err(Error::Denied);
            }
            live(&job, sample.millis)?;
            budget.cap(
                sample,
                job.lease_deadline.ok_or(Error::Storage)?,
            )?;
            grant.cap(sample)?;
            Ok(grant)
        })
        .await
    }

    /// Atomically persist the outcome the journal decided.
    /// Retried settled deliveries return the stored receipt after lease expiry;
    /// the host must still authenticate the original worker identity.
    ///
    /// A settlement publishes no successor. Successors belong to the creator
    /// journal's own frontier, which commits them independently of the queue.
    ///
    /// # Errors
    /// Refuses changed settlements, stale delivery fences and failed transactions.
    /// Revalidate active enrollment around an atomic settlement. Receipt replay
    /// checks current enrollment of the original worker through `authorize_replay`;
    /// an expired lease does not erase its immutable receipt.
    /// Replay neither renews the lease nor writes anything the caller supplies.
    /// Both callbacks receive the active transaction for scoped metadata reads.
    ///
    /// # Errors
    /// Refuses revoked active delivery, conflicting settlements and failed transactions.
    pub async fn settle_authorized<F, Fut, R, Replay>(
        &self,
        worker: &WorkerId,
        settlement: &JournalSettlement,
        mut authorize: F,
        mut authorize_replay: R,
    ) -> Result<SettlementReceipt, Error>
    where
        F: FnMut(Database) -> Fut,
        Fut: Future<Output = Result<WorkerId, Error>>,
        R: FnMut(Database) -> Replay,
        Replay: Future<Output = Result<WorkerId, Error>>,
    {
        let prepared = self.prepare_settlement(worker, settlement)?;
        let delivery = settlement.delivery();
        let outcome = settlement.outcome();
        let budget = Budget::new(self.options.transaction_timeout);
        loop {
            let result = self
                .transact_for(budget.clone(), |tx| {
                    let authorize = &mut authorize;
                    let authorize_replay = &mut authorize_replay;
                    let digest = &prepared.digest;
                    let budget = &budget;
                    async move {
                        lock_scope(&tx, &delivery.job.app_id).await?;
                        let job = load(&tx, &delivery.job.app_id, delivery.job.id.as_str())
                            .await?
                            .ok_or(Error::Conflict)?;
                        matches_delivery(&job, delivery)?;
                        let stored_outcome =
                            serde_json::to_string(outcome).map_err(|_| Error::Invalid)?;
                        let receipt = SettlementReceipt {
                            job_id: delivery.job.id.clone(),
                            app_id: delivery.job.app_id.clone(),
                            attempt: delivery.attempt,
                            outcome: outcome.clone(),
                        };
                        if job.state == "settled" {
                            if job.settlement_digest.as_deref() != Some(digest.as_str())
                                || job.outcome.as_deref() != Some(&stored_outcome)
                            {
                                return Err(Error::Conflict);
                            }
                            crate::management::settle(&tx, &delivery.job, outcome, true).await?;
                            if authorize_replay(tx.clone()).await? != delivery.worker_id {
                                return Err(Error::Denied);
                            }
                            return Ok(Retention::Ready(receipt));
                        }
                        require_worker(worker, authorize(tx.clone()).await?)?;
                        let sample = self.clock.sample().await?;
                        cap_live_delivery(budget, &job, sample)?;
                        crate::management::validate_job(&tx, &delivery.job, true).await?;
                        if let Some(deployment) = delivery.job.deployment_id() {
                            retention::require_held(&tx, &delivery.job.app_id, deployment).await?;
                        }
                        crate::management::settle(&tx, &delivery.job, outcome, false).await?;
                        update(
                            &tx,
                            fence(delivery),
                            value!({
                                "state":"settled","outcome":stored_outcome,"settlement_digest":digest
                            }),
                        )
                        .await?;
                        crate::recovery::settled_page(&tx, &delivery.job, outcome, sample.millis)
                            .await?;
                        Box::pin(crate::recovery::settled_close(
                            &tx,
                            &delivery.job,
                            outcome,
                            sample.millis,
                        ))
                        .await?;
                        require_worker(worker, authorize(tx.clone()).await?)?;
                        let sample = self.clock.sample().await?;
                        cap_live_delivery(budget, &job, sample)?;
                        Ok(Retention::Ready(receipt))
                    }
                })
                .await?;
            match result {
                Retention::Ready(receipt) => return Ok(receipt),
                Retention::Acquire(deployment) => {
                    self.ensure_deployment_for(&delivery.job.app_id, &deployment, budget.clone())
                        .await?;
                }
            }
        }
    }

    /// Refuse a delivery that is not the one this queue last handed out for its
    /// job: the job, worker and attempt the latest claim
    /// wrote.
    ///
    /// A FENCE WITHOUT A LIVENESS CHECK, because the caller has not decided yet
    /// whether it is settling or replaying. A settled job keeps the fence of the
    /// delivery that settled it, so an exact replay passes here after its lease
    /// has lapsed, and [`Self::settle_authorized`] then decides which branch the
    /// delivery takes. What this lets a host do is refuse a caller that does
    /// not hold the job BEFORE it reads anything a holder alone may read.
    ///
    /// # Errors
    /// Refuses with `Conflict` a delivery the queue never made or has since
    /// superseded, and reports failed transactions.
    pub async fn require_latest_delivery(&self, delivery: &Delivery) -> Result<(), Error> {
        self.transact(|tx| async move {
            let job = load(&tx, &delivery.job.app_id, delivery.job.id.as_str())
                .await?
                .ok_or(Error::Conflict)?;
            matches_delivery(&job, delivery)
        })
        .await
    }

    /// Refuse a task call for an app in which this worker holds no live
    /// delivery.
    ///
    /// A task read or reservation names an app and a task credential, never a
    /// delivery, so the queue cannot compare the exact attempt the way
    /// [`Self::require_latest_delivery`] does. What it proves instead is that
    /// the worker holds a live lease on some job of the app, which is what every
    /// task credential is issued under, and which must be true before the
    /// policy source is asked to observe the app. The journal then authorizes
    /// the task itself. Without this fence, naming another tenant's app would
    /// make the policy source observe it and answer differently by whether that
    /// app exists.
    ///
    /// # Errors
    /// Refuses with `Denied` an app in which the worker holds no live lease, and
    /// reports failed transactions.
    pub async fn require_live_holder(&self, worker: &WorkerId, app: &AppId) -> Result<(), Error> {
        self.transact(|tx| async move {
            let now = self.clock.now().await?;
            let held = tx
                .entity::<jobs::Entity>()?
                .count(
                    jobs::app_id
                        .eq(app.as_str())?
                        .and(jobs::worker_id.eq(Some(worker.as_str()))?)
                        .and(jobs::state.eq("leased")?)
                        .and(jobs::lease_deadline.gt(Some(now))?),
                )
                .await?;
            if held <= 0 {
                return Err(Error::Denied);
            }
            Ok(())
        })
        .await
    }

    /// Refuse a run call for an app that holds no queue scope in `zone`.
    ///
    /// A run call names an app and nothing else, and binding it observes the
    /// app's policy: a Control read and a ledger write whose answers differ by
    /// whether the app exists. The scope's frozen zone is this queue's own
    /// copy of the app's, written when Control's lifecycle publication first
    /// reached it, so this read is what admits the observation at all. An app
    /// this queue has never seen and an app of another zone are refused alike,
    /// with nothing observed and nothing written.
    ///
    /// # Errors
    /// Refuses with `Denied` an app with no scope in `zone`, and reports failed
    /// transactions.
    pub async fn require_scope_in_zone(&self, app: &AppId, zone: &ZoneId) -> Result<(), Error> {
        self.transact(|tx| async move {
            let held = tx
                .entity::<queue_scopes::Entity>()?
                .count(
                    queue_scopes::id
                        .eq(app.as_str())?
                        .and(queue_scopes::execution_zone_id.eq(zone.as_str())?),
                )
                .await?;
            if held <= 0 {
                return Err(Error::Denied);
            }
            Ok(())
        })
        .await
    }

    /// Return a live delivery to the ready queue without settling it, as
    /// `defer` says: until a deferral ends, or at once for an interrupted
    /// attempt.
    ///
    /// # Errors
    /// Refuses a stale or live-worker-mismatched delivery and invalid deadlines.
    pub async fn give_back<F, Fut>(
        &self,
        worker: &WorkerId,
        delivery: &Delivery,
        defer: GiveBack,
        authorize: F,
    ) -> Result<(), Error>
    where
        F: FnMut(Database) -> Fut,
        Fut: Future<Output = Result<WorkerId, Error>>,
    {
        self.give_back_within(
            Budget::new(self.options.transaction_timeout),
            ScopeLock::Wait,
            worker,
            delivery,
            defer,
            authorize,
        )
        .await
    }

    /// [`Self::give_back`] under a caller's budget and lock, which a zone
    /// claim bounds by the time its reply has left and takes without waiting.
    pub(crate) async fn give_back_within<F, Fut>(
        &self,
        budget: Budget,
        lock: ScopeLock,
        worker: &WorkerId,
        delivery: &Delivery,
        defer: GiveBack,
        mut authorize: F,
    ) -> Result<(), Error>
    where
        F: FnMut(Database) -> Fut,
        Fut: Future<Output = Result<WorkerId, Error>>,
    {
        bound(worker, delivery)?;
        self.transact_for(budget, |tx| async move {
            lock_scope_as(&tx, &delivery.job.app_id, lock).await?;
            require_worker(worker, authorize(tx.clone()).await?)?;
            let now = self.clock.now().await?;
            let job = load(&tx, &delivery.job.app_id, delivery.job.id.as_str())
                .await?
                .ok_or(Error::Conflict)?;
            matches_delivery(&job, delivery)?;
            live(&job, now)?;
            let patch = match defer {
                GiveBack::Interrupted => interrupted(&job, delivery)?,
                GiveBack::Unsent => value!({"state":"ready","worker_id":null,
                    "lease_deadline":null,"leased_at":null,"deferred_until":null}),
                GiveBack::Exact(_) | GiveBack::After(_) | GiveBack::Backoff => {
                    self.deferral(&job, defer, now)?
                }
            };
            update(&tx, fence(delivery), patch).await
        })
        .await
    }

    /// The row a deferral give-back leaves: ready, held by nobody, unclaimable
    /// until `deferred_until`, and no attempt counted.
    ///
    /// ONLY THE BACK-OFF COUNTS ITSELF. `deferrals` is what the back-off
    /// doubles by, so it moves for the give-backs that wait out a back-off and
    /// for no other: a pause whose length is already known - an occurrence not
    /// yet due, a policy observation not yet lapsed, a concurrency cap - says
    /// nothing about whether the app can be prepared or its deployment
    /// reached, and counting it would lengthen the next back-off for a fault
    /// that never happened.
    ///
    /// A DEFERRAL THAT HAS ALREADY ENDED LEAVES THE ROW CLAIMABLE NOW. The
    /// instant a host defers to can pass between its decision and this
    /// give-back - an occurrence that fell due meanwhile - and refusing that
    /// give-back would leave the row leased until its lease lapsed.
    fn deferral(&self, job: &Job, defer: GiveBack, now: i64) -> Result<Value, Error> {
        let (deferred_until, deferrals) = match defer {
            GiveBack::Exact(until) => (until, job.deferrals),
            GiveBack::After(delay) => (
                now.checked_add(i64::try_from(delay.as_millis()).map_err(|_| Error::Invalid)?)
                    .ok_or(Error::Capacity)?,
                job.deferrals,
            ),
            GiveBack::Backoff => {
                let shift = u32::try_from(job.deferrals).unwrap_or(u32::MAX).min(62);
                let multiplier = 1_u128.checked_shl(shift).unwrap_or(u128::MAX);
                let delay = self
                    .options
                    .defer_backoff
                    .as_millis()
                    .saturating_mul(multiplier)
                    .min(self.options.defer_backoff_max.as_millis());
                (
                    now.checked_add(i64::try_from(delay).map_err(|_| Error::Capacity)?)
                        .ok_or(Error::Capacity)?,
                    job.deferrals.checked_add(1).ok_or(Error::Capacity)?,
                )
            }
            GiveBack::Interrupted | GiveBack::Unsent => return Err(Error::Invalid),
        };
        let deferred_until = (deferred_until > now).then_some(deferred_until);
        Ok(value!({"state":"ready","worker_id":null,"lease_deadline":null,
            "leased_at":null,"deferred_until":deferred_until,"deferrals":deferrals}))
    }

    /// Refuse a receipt read for anyone but the worker this queue last delivered
    /// `job` to.
    ///
    /// THE LATEST HOLDER, NOT A LIVE ONE. A claim writes `worker_id` and a
    /// settlement leaves it, so a settled job still names the worker that
    /// settled it -- the holder a lost settlement reply leaves needing the job's
    /// receipt -- and the claim that superseded an earlier holder is what
    /// refuses that holder here. A give-back clears it: the row is nobody's
    /// until it is claimed again.
    ///
    /// The worker's live enrollment is the caller's check, made against the
    /// credential that verified the request. The job is compared whole, not by
    /// id, so a holder is answered about the job it was delivered and not about
    /// another operation under the same id.
    ///
    /// # Errors
    /// Refuses with `Conflict` a job this worker does not hold, including one
    /// the queue has never seen, and reports failed transactions.
    pub async fn require_latest_holder(
        &self,
        worker: &WorkerId,
        job: &JobSpec,
    ) -> Result<(), Error> {
        self.transact(|tx| async move {
            let stored = load(&tx, &job.app_id, job.id.as_str())
                .await?
                .ok_or(Error::Conflict)?;
            if stored.spec()? != *job || stored.worker_id.as_deref() != Some(worker.as_str()) {
                return Err(Error::Conflict);
            }
            Ok(())
        })
        .await
    }

    fn prepare_settlement(
        &self,
        worker: &WorkerId,
        settlement: &JournalSettlement,
    ) -> Result<PreparedSettlement, Error> {
        let delivery = settlement.delivery();
        bound(worker, delivery)?;
        if !valid_outcome(&delivery.job.operation, settlement.outcome()) {
            return Err(Error::Invalid);
        }
        self.encode(settlement)?;
        let digest = digest(&self.encode(&(
            &delivery.job,
            &delivery.worker_id,
            delivery.attempt,
            settlement.outcome(),
        ))?);
        Ok(PreparedSettlement { digest })
    }

    fn deadline(&self, now: i64, attempt_deadline: i64) -> Result<i64, Error> {
        let lease = i64::try_from(self.options.lease.as_millis()).map_err(|_| Error::Invalid)?;
        let deadline = now
            .checked_add(lease)
            .ok_or(Error::Capacity)?
            .min(attempt_deadline);
        if deadline <= now {
            return Err(Error::Denied);
        }
        Ok(deadline)
    }

    fn attempt_deadline(&self, leased_at: i64) -> Result<i64, Error> {
        let max_attempt =
            i64::try_from(self.options.max_attempt.as_millis()).map_err(|_| Error::Invalid)?;
        leased_at.checked_add(max_attempt).ok_or(Error::Capacity)
    }

    pub(crate) fn encode(&self, value: &impl Serialize) -> Result<Vec<u8>, Error> {
        let mut output = Metadata {
            bytes: Vec::new(),
            bound: self.options.max_metadata_bytes,
        };
        serde_json::to_writer(&mut output, value).map_err(|_| Error::Capacity)?;
        Ok(output.bytes)
    }

    pub(crate) async fn insert(
        &self,
        tx: &Database,
        spec: &JobSpec,
        now: i64,
    ) -> Result<(), Error> {
        if self.existing(tx, spec).await? {
            return Ok(());
        }
        if let Some(deployment) = spec.deployment_id() {
            retention::require_held(tx, &spec.app_id, deployment).await?;
        }
        let digest = digest(&self.encode(spec)?);
        let dispatch_order = next_dispatch_order(tx, &spec.app_id, None).await?;
        tx.collection(jobs::Entity::COLLECTION)?.insert(value!({
            "id":spec.id.as_str(),"app_id":spec.app_id.as_str(),"deployment_id":spec.deployment_id().map(DeploymentId::as_str),
            "operation":serde_json::to_string(&spec.operation).map_err(|_| Error::Invalid)?,
            "management_request_id":models::management_request(&spec.operation),
            "operation_kind":models::operation_kind(&spec.operation),"run_id":models::operation_run(&spec.operation),
            "spec_digest":digest,"available_at":spec.available_at.get(),"state":"ready", "attempt":0,
            "execution_attempts":0,
            "dispatch_order":dispatch_order,"created_at":now
        })).await?;
        Ok(())
    }

    pub(crate) async fn existing(&self, tx: &Database, spec: &JobSpec) -> Result<bool, Error> {
        let digest = digest(&self.encode(spec)?);
        if let Some(job) = load(tx, &spec.app_id, spec.id.as_str()).await? {
            if job.spec_digest != digest || job.spec()? != *spec {
                return Err(Error::Conflict);
            }
            match job.state.as_str() {
                "settled" => {}
                "ready" | "leased" => {
                    if let Some(deployment) = spec.deployment_id() {
                        retention::require_held(tx, &spec.app_id, deployment).await?;
                    }
                }
                _ => return Err(Error::Storage),
            }
            return Ok(true);
        }
        Ok(false)
    }

    pub(crate) async fn transact<T, F, Fut>(&self, body: F) -> Result<T, Error>
    where
        F: FnOnce(Database) -> Fut,
        Fut: Future<Output = Result<T, Error>>,
    {
        self.transact_for(Budget::new(self.options.transaction_timeout), body)
            .await
    }

    pub(crate) async fn transact_for<T, F, Fut>(&self, budget: Budget, body: F) -> Result<T, Error>
    where
        F: FnOnce(Database) -> Fut,
        Fut: Future<Output = Result<T, Error>>,
    {
        const CALLBACK_FAILED: &str = "workflow_manager_callback_failed";
        let mut failure = None;
        let saved = &mut failure;
        let result = bounded(
            budget,
            self.database.transaction(|tx| async move {
                match body(tx).await {
                    Ok(value) => Ok(value),
                    Err(error) => {
                        *saved = Some(error);
                        Err(DbError::validation(
                            CALLBACK_FAILED,
                            "queue transaction refused",
                        ))
                    }
                }
            }),
        )
        .await?;
        match result {
            Err(DbError::ValidationFailed {
                code: CALLBACK_FAILED,
                ..
            }) => Err(failure.unwrap_or(Error::Storage)),
            Err(error) => Err(error.into()),
            Ok(value) => Ok(value),
        }
    }
}

pub async fn register_scope_in(
    tx: &Database,
    app: &AppId,
    zone: &ZoneId,
) -> Result<(), Error> {
    // Two concurrent registrations of one app cannot both establish a row: the
    // insert leaves an existing row untouched, and the read below decides from
    // whichever row won. A conflict on the identity alone, with a stored zone
    // that differs, is the one refusal.
    tx.collection(queue_scopes::Entity::COLLECTION)?
        .execute(Operation::InsertOnConflict {
            document: value!({"id":app.as_str(),"execution_zone_id":zone.as_str()}),
            conflict_fields: value!(["id"]),
        })
        .await?;
    let scopes = tx.entity::<queue_scopes::Entity>()?;
    let scope = scopes
        .find::<Scope>(
            queue_scopes::id.eq(app.as_str())?,
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .next()
        .ok_or(Error::Storage)?;
    if scope.execution_zone_id != zone.as_str() {
        return Err(Error::Conflict);
    }
    tx.collection(models::capacity_targets::Entity::COLLECTION)?
        .execute(Operation::Upsert {
            document: value!({"id":zone.as_str()}),
            conflict_fields: value!(["id"]),
        })
        .await?;
    Ok(())
}

pub async fn lock_scope(tx: &Database, app: &AppId) -> Result<(), Error> {
    let result = tx
        .collection(queue_scopes::Entity::COLLECTION)?
        .execute(Operation::Update {
            filter: value!({"id":app.as_str()}),
            patch: value!({"$inc":{"lock_version":0}}),
            many: true,
        })
        .await?;
    match result {
        Output::Count(1) => Ok(()),
        Output::Count(0) => Err(Error::Denied),
        _ => Err(Error::Storage),
    }
}

/// Take the app's queue lock as `lock` says.
async fn lock_scope_as(tx: &Database, app: &AppId, lock: ScopeLock) -> Result<(), Error> {
    match lock {
        ScopeLock::Wait => lock_scope(tx, app).await,
        ScopeLock::Skip => lock_scope_without_waiting(tx, app).await,
    }
}

/// Take the app's queue lock without waiting for it.
///
/// On PostgreSQL the scope row is locked `FOR UPDATE NOWAIT`, and a row another
/// transaction holds is refused at once as [`Error::Contended`], so a zone claim
/// passes that app instead of spending its batch behind the holder. SQLite has
/// no row locks and one database-wide write lock, so it keeps the waiting
/// update-as-lock of [`lock_scope`], and a wait there ends as `Unavailable` or
/// `Timeout`, never `Contended`.
async fn lock_scope_without_waiting(tx: &Database, app: &AppId) -> Result<(), Error> {
    if tx.postgres().is_err() {
        return lock_scope(tx, app).await;
    }
    let locked = tx
        .entity::<queue_scopes::Entity>()?
        .query()
        .filter(queue_scopes::id.eq(app.as_str())?)
        .for_update_nowait()?
        .first::<Scope>()
        .await
        .map_err(|error| match error {
            DbError::LockContention { .. } => Error::Contended,
            other => Error::from(other),
        })?;
    if locked.is_some() {
        Ok(())
    } else {
        Err(Error::Denied)
    }
}

/// Allocate under the app lock in the transaction that publishes or claims a job.
/// A claim moves to the tail without changing its immutable specification.
async fn next_dispatch_order(
    tx: &Database,
    app: &AppId,
    previous: Option<i64>,
) -> Result<i64, Error> {
    let scopes = tx.entity::<queue_scopes::Entity>()?;
    let scope = scopes
        .find::<Scope>(
            queue_scopes::id.eq(app.as_str())?,
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .next()
        .ok_or(Error::Storage)?;
    if scope.dispatch_cursor < 0
        || previous.is_some_and(|order| order <= 0 || order > scope.dispatch_cursor)
    {
        return Err(Error::Storage);
    }
    let next = scope
        .dispatch_cursor
        .checked_add(1)
        .ok_or(Error::Capacity)?;
    if scopes
        .update_many(
            queue_scopes::id
                .eq(app.as_str())?
                .and(queue_scopes::dispatch_cursor.eq(scope.dispatch_cursor)?),
            queue_scopes::dispatch_cursor.set(next)?,
        )
        .await?
        != 1
    {
        return Err(Error::Storage);
    }
    Ok(next)
}

pub async fn load(tx: &Database, app: &AppId, id: &str) -> Result<Option<Job>, Error> {
    Ok(tx
        .entity::<jobs::Entity>()?
        .find::<Job>(
            jobs::id
                .eq(id.to_owned())?
                .and(jobs::app_id.eq(app.as_str().to_owned())?),
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .next())
}

async fn update(tx: &Database, filter: Value, patch: Value) -> Result<(), Error> {
    match tx
        .collection(jobs::Entity::COLLECTION)?
        .execute(Operation::Update {
            filter,
            patch,
            many: true,
        })
        .await?
    {
        Output::Count(1) => Ok(()),
        Output::Count(0) => Err(Error::Conflict),
        _ => Err(Error::Storage),
    }
}

fn require_worker(expected: &WorkerId, observed: WorkerId) -> Result<(), Error> {
    if expected != &observed {
        return Err(Error::Denied);
    }
    Ok(())
}

fn bound(worker: &WorkerId, delivery: &Delivery) -> Result<(), Error> {
    if worker != &delivery.worker_id {
        return Err(Error::Denied);
    }
    Ok(())
}

fn matches_delivery(job: &Job, delivery: &Delivery) -> Result<(), Error> {
    if job.spec()? != delivery.job
        || job.worker_id.as_deref() != Some(delivery.worker_id.as_str())
        || job.attempt != delivery.attempt.get()
    {
        return Err(Error::Conflict);
    }
    Ok(())
}

fn cap_live_delivery(
    budget: &Budget,
    job: &Job,
    sample: Sample,
) -> Result<(), Error> {
    live(job, sample.millis)?;
    budget.cap(sample, job.lease_deadline.ok_or(Error::Storage)?)
}

fn attempt_deadline(job: &Job, max_attempt: Duration) -> Result<i64, Error> {
    let leased_at = job.leased_at.ok_or(Error::Storage)?;
    let duration = i64::try_from(max_attempt.as_millis()).map_err(|_| Error::Invalid)?;
    leased_at.checked_add(duration).ok_or(Error::Capacity)
}

fn live(job: &Job, now: i64) -> Result<(), Error> {
    if job.state != "leased" || job.lease_deadline.is_none_or(|deadline| deadline <= now) {
        return Err(Error::Conflict);
    }
    Ok(())
}

/// Renewal is the manager's evidence that a delivery began executing: a claim
/// the journal defers never reaches this path, so its attempt stays uncounted
/// and capacity pressure cannot exhaust a job's budget. The first renewal of an
/// attempt counts it; later renewals of the same attempt extend only the lease.
fn renewal(job: &Job, delivery: &Delivery, deadline: i64) -> Result<Value, Error> {
    let mut patch = counted(job, delivery)?;
    patch["lease_deadline"] = value!(deadline);
    Ok(patch)
}

/// The row an interrupted attempt leaves: ready at once, held by nobody, and
/// the attempt counted by the same rule its renewal follows.
///
/// The holder's own report is the other evidence that an attempt executed: it
/// began, and stopped without a receipt. An attempt that fails before its
/// first renewal is counted here or nowhere, and an uncounted failure would be
/// redelivered without end.
fn interrupted(job: &Job, delivery: &Delivery) -> Result<Value, Error> {
    let mut patch = counted(job, delivery)?;
    for (field, cleared) in [
        ("state", value!("ready")),
        ("worker_id", Value::Null),
        ("lease_deadline", Value::Null),
        ("leased_at", Value::Null),
        ("deferred_until", Value::Null),
    ] {
        patch[field] = cleared;
    }
    Ok(patch)
}

/// Count `delivery`'s attempt toward the delivery budget once, and end the run
/// of consecutive back-offs: an attempt already counted is not counted again.
fn counted(job: &Job, delivery: &Delivery) -> Result<Value, Error> {
    if job.executed_attempt == Some(delivery.attempt.get()) {
        return Ok(value!({ "deferrals": 0 }));
    }
    let counted = job
        .execution_attempts
        .checked_add(1)
        .ok_or(Error::Capacity)?;
    Ok(value!({"execution_attempts":counted,"deferrals":0,
        "executed_attempt":delivery.attempt.get()}))
}

fn fence(delivery: &Delivery) -> Value {
    value!({"id":delivery.job.id.as_str(),"app_id":delivery.job.app_id.as_str(),"state":"leased",
        "worker_id":delivery.worker_id.as_str(),"attempt":delivery.attempt.get()})
}

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// The stored lease can shorten the caller's wait after its app lock is acquired.
/// A dispatched commit may finish after this wait; its receipt resolves retries.
#[derive(Clone)]
pub struct Budget(Rc<Cell<Instant>>);

impl Budget {
    pub(crate) fn new(timeout: Duration) -> Self {
        Self(Rc::new(Cell::new(Instant::now() + timeout)))
    }

    /// A budget that ends at `deadline` exactly.
    pub(crate) fn at(deadline: Instant) -> Self {
        Self(Rc::new(Cell::new(deadline)))
    }

    /// `timeout` from now, never past `deadline`: one transaction's share of a
    /// caller's larger budget.
    pub(crate) fn within(timeout: Duration, deadline: Instant) -> Self {
        Self::at((Instant::now() + timeout).min(deadline))
    }

    pub(crate) fn cap(&self, sample: Sample, deadline: i64) -> Result<(), Error> {
        self.cap_at(local_deadline(sample, deadline)?)
    }

    pub(crate) fn cap_at(&self, deadline: Instant) -> Result<(), Error> {
        self.0.set(self.0.get().min(deadline));
        if self.0.get() <= Instant::now() {
            return Err(Error::Timeout);
        }
        Ok(())
    }
}

pub fn local_deadline(sample: Sample, deadline: i64) -> Result<Instant, Error> {
    // The database sample is floored. Charge its resolution so the conversion
    // cannot retain the unobserved fraction of the final clock tick.
    let remaining = deadline
        .checked_sub(sample.millis)
        .and_then(|remaining| remaining.checked_sub(RESOLUTION_MILLIS))
        .filter(|remaining| *remaining > 0)
        .ok_or(Error::Denied)?;
    sample
        .started
        .checked_add(Duration::from_millis(
            u64::try_from(remaining).map_err(|_| Error::Invalid)?,
        ))
        .ok_or(Error::Invalid)
}

pub async fn bounded<T>(budget: Budget, future: impl Future<Output = T>) -> Result<T, Error> {
    let mut future = Box::pin(future);
    let mut deadline = budget.0.get();
    let mut timer = Box::pin(compio::time::sleep(
        deadline.saturating_duration_since(Instant::now()),
    ));
    poll_fn(move |context| {
        if Instant::now() >= budget.0.get() {
            return Poll::Ready(Err(Error::Timeout));
        }
        if deadline != budget.0.get() {
            deadline = budget.0.get();
            timer = Box::pin(compio::time::sleep(
                deadline.saturating_duration_since(Instant::now()),
            ));
        }
        if timer.as_mut().poll(context).is_ready() {
            return Poll::Ready(Err(Error::Timeout));
        }
        let result = future.as_mut().poll(context);
        if Instant::now() >= budget.0.get() {
            return Poll::Ready(Err(Error::Timeout));
        }
        if deadline != budget.0.get() {
            deadline = budget.0.get();
            timer = Box::pin(compio::time::sleep(
                deadline.saturating_duration_since(Instant::now()),
            ));
            if timer.as_mut().poll(context).is_ready() {
                return Poll::Ready(Err(Error::Timeout));
            }
        }
        result.map(Ok)
    })
    .await
}

struct Metadata {
    bytes: Vec<u8>,
    bound: usize,
}
impl Write for Metadata {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.bound.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other(
                "workflow queue metadata exceeds its bound",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroship_core::workflow_jobs::{JobId, JobOperation};

    fn delivery(deadline: i64) -> Delivery {
        Delivery {
            job: JobSpec {
                id: JobId::mint(),
                app_id: AppId::mint(),
                operation: JobOperation::Reconcile {},
                available_at: 0.try_into().unwrap(),
            },
            worker_id: WorkerId::mint(),
            attempt: 1.try_into().unwrap(),
            deadline: deadline.try_into().unwrap(),
        }
    }

    #[test]
    fn later_clock_samples_can_only_shorten_a_grant() {
        let started = Instant::now();
        let mut grant = DeliveryGrant::new(
            delivery(120_000),
            Sample {
                millis: 60_000,
                started,
            },
            120_000,
        )
        .unwrap();
        let original = grant.expires_at;
        grant
            .cap(Sample {
                millis: 0,
                started: started + Duration::from_secs(1),
            })
            .unwrap();
        assert_eq!(
            grant.expires_at, original,
            "a backwards clock sample cannot add authority"
        );
        grant
            .cap(Sample {
                millis: 100_000,
                started: started + Duration::from_secs(2),
            })
            .unwrap();
        let shortened = grant.expires_at;
        assert!(shortened < original);
        grant
            .cap(Sample {
                millis: 0,
                started: started + Duration::from_secs(3),
            })
            .unwrap();
        assert_eq!(
            grant.expires_at, shortened,
            "later observations cannot undo a shortened budget"
        );
    }

    #[test]
    fn original_attempt_sample_caps_a_grant_created_after_clock_regression() {
        let started = Instant::now();
        let grant = DeliveryGrant::new(
            delivery(120_000),
            Sample {
                millis: 0,
                started: started + Duration::from_secs(1),
            },
            100_000,
        )
        .unwrap();
        assert_eq!(
            grant.attempt_expires_at,
            started + Duration::from_secs(1) + Duration::from_millis(99_999)
        );
        assert!(grant.expires_at < started + Duration::from_secs(121));
        assert!(grant.attempt_expires_at < grant.expires_at);
    }

    #[test]
    fn an_expired_grant_cannot_serialize_positive_authority() {
        let grant = DeliveryGrant {
            delivery: delivery(i64::MAX),
            expires_at: Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
            attempt_expires_at: Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap(),
        };
        let cloned = grant.clone();
        assert_eq!(grant.lease(), Err(Error::Timeout));
        assert_eq!(cloned.lease(), Err(Error::Timeout));
    }

    #[test]
    fn quantized_clock_sample_cannot_grant_its_unobserved_final_fraction() {
        let sample = Sample {
            millis: 10_000,
            started: Instant::now(),
        };
        assert_eq!(
            local_deadline(sample, sample.millis + RESOLUTION_MILLIS),
            Err(Error::Denied)
        );
        assert_eq!(
            local_deadline(sample, sample.millis + RESOLUTION_MILLIS + 1),
            Ok(sample.started + Duration::from_millis(1))
        );
    }
}
