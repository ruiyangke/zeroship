#![expect(
    clippy::future_not_send,
    reason = "native ORM transactions stay on their owning compio thread"
)]

use crate::{
    clock::{Clock, Sample, RESOLUTION_MILLIS},
    error::Error,
    models::{self, jobs, queue_scopes, Job},
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    collections::BTreeMap,
    future::{poll_fn, ready, Future},
    io::Write,
    num::{NonZeroU64, NonZeroUsize},
    rc::Rc,
    task::Poll,
    time::{Duration, Instant},
};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{Assignment, VerifyAssignment, WorkerId},
    workflow_jobs::{Delivery, DeliveryLease, JobSpec, Settlement, SettlementReceipt},
};
use zeroship_data_orm::{
    binding::DbBinding,
    encryption::ProjectKeySource,
    error::DbError,
    orm::{Database, Entity, FindOptions, Operation, Output},
    value, ConnectOptions, Value,
};

/// Bounds leases, transaction waits and successor metadata.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub max_connections: NonZeroUsize,
    pub lease: Duration,
    pub transaction_timeout: Duration,
    pub max_successors: usize,
    pub max_metadata_bytes: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_connections: NonZeroUsize::new(8).unwrap(),
            lease: Duration::from_secs(30),
            transaction_timeout: Duration::from_secs(5),
            max_successors: 256,
            max_metadata_bytes: 256 * 1024,
        }
    }
}

/// Durable platform metadata with app-scoped delivery fences.
///
/// Hosts authenticate workers and obtain their current coordinator assignment
/// before calling delivery operations. The authorized variants recheck placement
/// after locks and before commit. Journal authority remains in the creator zone.
/// A timeout does not retract a dispatched commit; retry the same settlement to
/// recover its durable receipt.
#[derive(Clone, Debug)]
pub struct Queue {
    pub(crate) database: Database,
    pub(crate) clock: Clock,
    pub(crate) options: Options,
}

/// A committed delivery with the manager's original monotonic lease budget.
/// Response construction must consume only the authority still remaining.
#[derive(Debug, Clone)]
pub struct DeliveryGrant {
    pub delivery: Delivery,
    expires_at: Instant,
}

impl DeliveryGrant {
    fn new(delivery: Delivery, sample: Sample, assignment_expires: Instant) -> Result<Self, Error> {
        let expires_at = local_deadline(sample, delivery.deadline.get())?.min(assignment_expires);
        if expires_at <= Instant::now() {
            return Err(Error::Timeout);
        }
        Ok(Self {
            delivery,
            expires_at,
        })
    }

    fn cap(&mut self, sample: Sample) -> Result<(), Error> {
        self.expires_at = self
            .expires_at
            .min(local_deadline(sample, self.delivery.deadline.get())?);
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
        Ok(DeliveryLease {
            delivery: self.delivery.clone(),
            remaining_ms,
        })
    }
}

impl Queue {
    /// Bind provisioned platform storage without creating schemas or roles.
    ///
    /// # Errors
    /// Refuses invalid bounds and unavailable or incompatible storage.
    pub async fn connect(binding: DbBinding, url: &str, options: Options) -> Result<Self, Error> {
        if options.transaction_timeout.is_zero()
            || Instant::now()
                .checked_add(options.transaction_timeout)
                .is_none()
            || options.max_successors == 0
            || options.max_metadata_bytes == 0
            || options.lease.as_millis() == 0
            || i64::try_from(options.lease.as_millis()).is_err()
        {
            return Err(Error::Invalid);
        }
        let database = Database::connect(
            binding.clone(),
            ConnectOptions::new(url, ProjectKeySource::unavailable())
                .max_connections(options.max_connections)
                .connection_authority(),
            models::collections()?,
        )
        .await?;
        for collection in [jobs::Entity::COLLECTION, queue_scopes::Entity::COLLECTION] {
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
        })
    }

    /// Register an app selected by trusted platform configuration.
    ///
    /// # Errors
    /// Refuses failed transactions; repeated registration preserves queue state.
    pub async fn register_scope(&self, app: &AppId) -> Result<(), Error> {
        self.transact(|tx| async move { register_scope_in(&tx, app).await })
            .await
    }

    /// Submit immutable metadata from a trusted manager operation.
    /// Worker publication uses [`Self::submit_authorized`] to fence its outbox retry.
    ///
    /// # Errors
    /// Refuses unknown apps, reused identities with different content and storage failures.
    pub async fn submit(&self, job: &JobSpec) -> Result<JobSpec, Error> {
        self.encode(job)?;
        self.transact(|tx| async move {
            lock_scope(&tx, &job.app_id).await?;
            self.insert(&tx, job, self.clock.now().await?).await?;
            Ok(job.clone())
        })
        .await
    }

    /// Publish an app's immutable job under its current host-authenticated assignment.
    /// Invoke the host's enrollment and placement checks after the app lock and
    /// before commit, including exact publication retries. The callback receives the active
    /// transaction so placement reads share the queue's serialization boundary.
    /// The host must separately authorize the requested operation's provenance.
    ///
    /// # Errors
    /// Refuses foreign apps, revoked authority, changed job identities and failed transactions.
    pub async fn submit_authorized<F, Fut>(
        &self,
        assignment: &VerifyAssignment,
        job: &JobSpec,
        mut authorize: F,
    ) -> Result<JobSpec, Error>
    where
        F: FnMut(Database) -> Fut,
        Fut: Future<Output = Result<Assignment, Error>>,
    {
        if job.app_id != assignment.app_id {
            return Err(Error::Denied);
        }
        self.encode(job)?;
        let budget = Budget::new(self.options.transaction_timeout);
        self.transact_for(budget.clone(), |tx| async move {
            lock_scope(&tx, &assignment.app_id).await?;
            let observed = authorize(tx.clone()).await?;
            let sample = self.clock.sample().await?;
            let authority = current(assignment, observed, sample.millis)?;
            budget.cap(sample, authority.expires_at.get())?;
            self.insert(&tx, job, sample.millis).await?;
            let observed = authorize(tx.clone()).await?;
            let sample = self.clock.sample().await?;
            let authority = current(assignment, observed, sample.millis)?;
            budget.cap(sample, authority.expires_at.get())?;
            Ok(job.clone())
        })
        .await
    }

    /// Claim under a current assignment authenticated by the native host.
    ///
    /// # Errors
    /// Refuses expired authority and failed transactions.
    pub async fn claim(&self, assignment: &Assignment) -> Result<Option<DeliveryGrant>, Error> {
        self.claim_authorized(&assignment.into(), |_| ready(Ok(assignment.clone())))
            .await
    }

    /// Revalidate placement after acquiring the app lock and before commit.
    /// Each authorization callback receives the active transaction for scoped reads.
    ///
    /// # Errors
    /// Refuses revoked assignments, exhausted attempts and failed transactions.
    pub async fn claim_authorized<F, Fut>(
        &self,
        assignment: &VerifyAssignment,
        mut authorize: F,
    ) -> Result<Option<DeliveryGrant>, Error>
    where
        F: FnMut(Database) -> Fut,
        Fut: Future<Output = Result<Assignment, Error>>,
    {
        let budget = Budget::new(self.options.transaction_timeout);
        self.transact_for(budget.clone(), |tx| async move {
            lock_scope(&tx, &assignment.app_id).await?;
            let observed = authorize(tx.clone()).await?;
            let sample = self.clock.sample().await?;
            let authority = current(assignment, observed, sample.millis)?;
            budget.cap(sample, authority.expires_at.get())?;
            let now = sample.millis;
            let assignment_expires = local_deadline(sample, authority.expires_at.get())?;
            let due = value!({"app_id":assignment.app_id.as_str(), "$or":[
                {"state":"ready", "available_at":{"$lte":now}},
                {"state":"leased", "lease_deadline":{"$lte":now}}
            ]});
            let Output::Rows { rows, .. } = tx.collection(jobs::Entity::COLLECTION)?.find(
                due, value!({"limit":1,"select":["id"],"orderBy":{"available_at":1,"id":1}})
            ).await? else { return Err(Error::Storage); };
            let Some(row) = rows.first() else {
                let observed = authorize(tx.clone()).await?;
                let sample = self.clock.sample().await?;
                let authority = current(assignment, observed, sample.millis)?;
                budget.cap(sample, authority.expires_at.get())?;
                return Ok(None);
            };
            let id = row["id"].as_str().ok_or(Error::Storage)?;
            let job = load(&tx, &assignment.app_id, id).await?.ok_or(Error::Storage)?;
            let attempt = job.attempt.checked_add(1).filter(|value| *value > 0)
                .ok_or(Error::Capacity)?;
            let sample = self.clock.sample().await?;
            let deadline = self.deadline(&authority, sample.millis)?;
            budget.cap(sample, deadline)?;
            let delivery = Delivery {
                job: job.spec()?, worker_id: assignment.worker_id.clone(),
                assignment_revision: assignment.assignment_revision,
                attempt: attempt.try_into().map_err(|_| Error::Storage)?,
                deadline: deadline.try_into().map_err(|_| Error::Storage)?,
            };
            let mut grant = DeliveryGrant::new(delivery, sample, assignment_expires)?;
            update(&tx,
                value!({"id":id,"app_id":assignment.app_id.as_str(),"state":job.state,"attempt":job.attempt}),
                value!({"state":"leased","attempt":attempt,"worker_id":assignment.worker_id.as_str(),
                    "assignment_revision":assignment.assignment_revision.get(),"lease_deadline":deadline})
            ).await?;
            let observed = authorize(tx.clone()).await?;
            let sample = self.clock.sample().await?;
            let authority = current(assignment, observed, sample.millis)?;
            if deadline > authority.expires_at.get() || sample.millis >= deadline {
                return Err(Error::Denied);
            }
            budget.cap(sample, deadline.min(authority.expires_at.get()))?;
            grant.cap(sample)?;
            Ok(Some(grant))
        }).await
    }

    /// Extend the current delivery under host-authenticated placement.
    ///
    /// # Errors
    /// Refuses expired deliveries and any changed worker, app, revision or attempt.
    pub async fn heartbeat(
        &self,
        assignment: &Assignment,
        delivery: &Delivery,
    ) -> Result<DeliveryGrant, Error> {
        self.heartbeat_authorized(&assignment.into(), delivery, |_| {
            ready(Ok(assignment.clone()))
        })
        .await
    }

    /// Revalidate placement while extending the stored delivery lease.
    /// Each authorization callback receives the active transaction for scoped reads.
    ///
    /// # Errors
    /// Refuses stale delivery identity, revoked placement and failed transactions.
    pub async fn heartbeat_authorized<F, Fut>(
        &self,
        assignment: &VerifyAssignment,
        delivery: &Delivery,
        mut authorize: F,
    ) -> Result<DeliveryGrant, Error>
    where
        F: FnMut(Database) -> Fut,
        Fut: Future<Output = Result<Assignment, Error>>,
    {
        bound(assignment, delivery)?;
        let budget = Budget::new(self.options.transaction_timeout);
        self.transact_for(budget.clone(), |tx| async move {
            lock_scope(&tx, &assignment.app_id).await?;
            let observed = authorize(tx.clone()).await?;
            let sample = self.clock.sample().await?;
            let authority = current(assignment, observed, sample.millis)?;
            budget.cap(sample, authority.expires_at.get())?;
            let assignment_expires = local_deadline(sample, authority.expires_at.get())?;
            let job = load(&tx, &assignment.app_id, delivery.job.id.as_str())
                .await?
                .ok_or(Error::Conflict)?;
            matches_delivery(&job, delivery)?;
            let sample = self.clock.sample().await?;
            live(&job, sample.millis)?;
            budget.cap(
                sample,
                authority
                    .expires_at
                    .get()
                    .min(job.lease_deadline.ok_or(Error::Storage)?),
            )?;
            let deadline = self.deadline(&authority, sample.millis)?;
            let mut grant = DeliveryGrant::new(
                Delivery {
                    deadline: deadline.try_into().map_err(|_| Error::Storage)?,
                    ..delivery.clone()
                },
                sample,
                assignment_expires,
            )?;
            update(&tx, fence(delivery), value!({"lease_deadline":deadline})).await?;
            let observed = authorize(tx.clone()).await?;
            let sample = self.clock.sample().await?;
            let authority = current(assignment, observed, sample.millis)?;
            if deadline > authority.expires_at.get() || sample.millis >= deadline {
                return Err(Error::Denied);
            }
            live(&job, sample.millis)?;
            budget.cap(
                sample,
                authority
                    .expires_at
                    .get()
                    .min(job.lease_deadline.ok_or(Error::Storage)?),
            )?;
            grant.cap(sample)?;
            Ok(grant)
        })
        .await
    }

    /// Atomically persist an outcome and its immutable successor jobs.
    /// Retried settled deliveries return the stored receipt after lease expiry;
    /// the host must still authenticate the original worker identity.
    ///
    /// # Errors
    /// Refuses changed settlements, foreign successors and stale delivery fences.
    pub async fn settle(
        &self,
        assignment: &Assignment,
        settlement: &Settlement,
    ) -> Result<SettlementReceipt, Error> {
        self.settle_authorized(
            &assignment.into(),
            settlement,
            |_| ready(Ok(assignment.clone())),
            |_| ready(Ok(settlement.delivery.worker_id.clone())),
        )
        .await
    }

    /// Revalidate active placement around an atomic settlement. Receipt replay
    /// checks current enrollment of the original worker through `authorize_replay`;
    /// expired or replaced placement does not erase its immutable receipt.
    /// Replay neither renews placement nor admits successor writes.
    /// Both callbacks receive the active transaction for scoped metadata reads.
    ///
    /// # Errors
    /// Refuses revoked active delivery, conflicting successors and failed transactions.
    pub async fn settle_authorized<F, Fut, R, Replay>(
        &self,
        assignment: &VerifyAssignment,
        settlement: &Settlement,
        mut authorize: F,
        mut authorize_replay: R,
    ) -> Result<SettlementReceipt, Error>
    where
        F: FnMut(Database) -> Fut,
        Fut: Future<Output = Result<Assignment, Error>>,
        R: FnMut(Database) -> Replay,
        Replay: Future<Output = Result<WorkerId, Error>>,
    {
        let delivery = &settlement.delivery;
        bound(assignment, delivery)?;
        if settlement.successors.len() > self.options.max_successors {
            return Err(Error::Capacity);
        }
        self.encode(settlement)?;
        let mut successors = BTreeMap::new();
        for successor in &settlement.successors {
            if successor.app_id != assignment.app_id {
                return Err(Error::Denied);
            }
            if successor.id == delivery.job.id {
                return Err(Error::Conflict);
            }
            if successors
                .insert(successor.id.as_str(), successor)
                .is_some_and(|previous| previous != successor)
            {
                return Err(Error::Conflict);
            }
        }
        let digest = digest(&self.encode(&(
            &delivery.job,
            &delivery.worker_id,
            delivery.assignment_revision,
            delivery.attempt,
            settlement.outcome,
            successors.values().collect::<Vec<_>>(),
        ))?);
        let budget = Budget::new(self.options.transaction_timeout);
        self.transact_for(budget.clone(), |tx| async move {
            lock_scope(&tx, &assignment.app_id).await?;
            let job = load(&tx, &assignment.app_id, delivery.job.id.as_str())
                .await?
                .ok_or(Error::Conflict)?;
            matches_delivery(&job, delivery)?;
            let outcome = serde_json::to_string(&settlement.outcome).map_err(|_| Error::Invalid)?;
            let receipt = SettlementReceipt {
                job_id: delivery.job.id.clone(),
                app_id: assignment.app_id.clone(),
                attempt: delivery.attempt,
                outcome: settlement.outcome,
            };
            if job.state == "settled" {
                if job.settlement_digest.as_deref() != Some(&digest)
                    || job.outcome.as_deref() != Some(&outcome)
                {
                    return Err(Error::Conflict);
                }
                if authorize_replay(tx.clone()).await? != delivery.worker_id {
                    return Err(Error::Denied);
                }
                return Ok(receipt);
            }
            let observed = authorize(tx.clone()).await?;
            let sample = self.clock.sample().await?;
            let authority = current(assignment, observed, sample.millis)?;
            live(&job, sample.millis)?;
            budget.cap(
                sample,
                authority
                    .expires_at
                    .get()
                    .min(job.lease_deadline.ok_or(Error::Storage)?),
            )?;
            for successor in successors.values() {
                self.insert(&tx, successor, sample.millis).await?;
            }
            update(
                &tx,
                fence(delivery),
                value!({
                    "state":"settled","outcome":outcome,"settlement_digest":digest
                }),
            )
            .await?;
            let observed = authorize(tx.clone()).await?;
            let sample = self.clock.sample().await?;
            let authority = current(assignment, observed, sample.millis)?;
            live(&job, sample.millis)?;
            budget.cap(
                sample,
                authority
                    .expires_at
                    .get()
                    .min(job.lease_deadline.ok_or(Error::Storage)?),
            )?;
            Ok(receipt)
        })
        .await
    }

    fn deadline(&self, assignment: &Assignment, now: i64) -> Result<i64, Error> {
        let lease = i64::try_from(self.options.lease.as_millis()).map_err(|_| Error::Invalid)?;
        let deadline = now
            .checked_add(lease)
            .ok_or(Error::Capacity)?
            .min(assignment.expires_at.get());
        if deadline <= now {
            return Err(Error::Denied);
        }
        Ok(deadline)
    }

    fn encode(&self, value: &impl Serialize) -> Result<Vec<u8>, Error> {
        let mut output = Metadata {
            bytes: Vec::new(),
            bound: self.options.max_metadata_bytes,
        };
        serde_json::to_writer(&mut output, value).map_err(|_| Error::Capacity)?;
        Ok(output.bytes)
    }

    async fn insert(&self, tx: &Database, spec: &JobSpec, now: i64) -> Result<(), Error> {
        let digest = digest(&self.encode(spec)?);
        if let Some(job) = load(tx, &spec.app_id, spec.id.as_str()).await? {
            return if job.spec_digest == digest && job.spec()? == *spec {
                Ok(())
            } else {
                Err(Error::Conflict)
            };
        }
        tx.collection(jobs::Entity::COLLECTION)?.insert(value!({
            "id":spec.id.as_str(),"app_id":spec.app_id.as_str(),"deployment_id":spec.deployment_id.as_str(),
            "operation":serde_json::to_string(&spec.operation).map_err(|_| Error::Invalid)?,
            "spec_digest":digest,"available_at":spec.available_at.get(),"state":"ready", "attempt":0,
            "created_at":now
        })).await?;
        Ok(())
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

pub async fn register_scope_in(tx: &Database, app: &AppId) -> Result<(), Error> {
    tx.collection(queue_scopes::Entity::COLLECTION)?
        .execute(Operation::Upsert {
            document: value!({"id":app.as_str()}),
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

async fn load(tx: &Database, app: &AppId, id: &str) -> Result<Option<Job>, Error> {
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

fn current(
    original: &VerifyAssignment,
    observed: Assignment,
    now: i64,
) -> Result<Assignment, Error> {
    if original != &VerifyAssignment::from(&observed) || observed.expires_at.get() <= now
    {
        return Err(Error::Denied);
    }
    Ok(observed)
}

fn bound(assignment: &VerifyAssignment, delivery: &Delivery) -> Result<(), Error> {
    if assignment.app_id != delivery.job.app_id
        || assignment.worker_id != delivery.worker_id
        || assignment.assignment_revision != delivery.assignment_revision
    {
        return Err(Error::Denied);
    }
    Ok(())
}

fn matches_delivery(job: &Job, delivery: &Delivery) -> Result<(), Error> {
    if job.spec()? != delivery.job
        || job.worker_id.as_deref() != Some(delivery.worker_id.as_str())
        || job.assignment_revision != Some(delivery.assignment_revision.get())
        || job.attempt != delivery.attempt.get()
    {
        return Err(Error::Conflict);
    }
    Ok(())
}

fn live(job: &Job, now: i64) -> Result<(), Error> {
    if job.state != "leased" || job.lease_deadline.is_none_or(|deadline| deadline <= now) {
        return Err(Error::Conflict);
    }
    Ok(())
}

fn fence(delivery: &Delivery) -> Value {
    value!({"id":delivery.job.id.as_str(),"app_id":delivery.job.app_id.as_str(),"state":"leased",
        "worker_id":delivery.worker_id.as_str(),"assignment_revision":delivery.assignment_revision.get(),
        "attempt":delivery.attempt.get()})
}

fn digest(bytes: &[u8]) -> String {
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

    pub(crate) fn cap(&self, sample: Sample, deadline: i64) -> Result<(), Error> {
        let deadline = local_deadline(sample, deadline)?;
        self.0.set(self.0.get().min(deadline));
        if self.0.get() <= Instant::now() {
            return Err(Error::Timeout);
        }
        Ok(())
    }
}

fn local_deadline(sample: Sample, deadline: i64) -> Result<Instant, Error> {
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

async fn bounded<T>(budget: Budget, future: impl Future<Output = T>) -> Result<T, Error> {
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
    use zeroship_core::workflow_jobs::{DeploymentId, JobId, JobOperation};

    fn delivery(deadline: i64) -> Delivery {
        Delivery {
            job: JobSpec {
                id: JobId::mint(),
                app_id: AppId::mint(),
                deployment_id: DeploymentId::mint(),
                operation: JobOperation::Reconcile {},
                available_at: 0.try_into().unwrap(),
            },
            worker_id: WorkerId::mint(),
            assignment_revision: 1.try_into().unwrap(),
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
            started + Duration::from_secs(60),
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
    fn original_assignment_sample_caps_a_grant_created_after_clock_regression() {
        let started = Instant::now();
        let assignment_expires = local_deadline(
            Sample {
                millis: 100_000,
                started,
            },
            120_000,
        )
        .unwrap();
        let grant = DeliveryGrant::new(
            delivery(120_000),
            Sample {
                millis: 0,
                started: started + Duration::from_secs(1),
            },
            assignment_expires,
        )
        .unwrap();
        assert_eq!(grant.expires_at, assignment_expires);
        assert!(grant.expires_at < started + Duration::from_secs(120));
    }

    #[test]
    fn an_expired_grant_cannot_serialize_positive_authority() {
        let grant = DeliveryGrant {
            delivery: delivery(i64::MAX),
            expires_at: Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
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
