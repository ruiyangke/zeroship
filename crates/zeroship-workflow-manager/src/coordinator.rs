//! Platform placement and management share the queue's ORM transaction and app lock.
#![expect(
    clippy::future_not_send,
    reason = "manager ORM handles remain on their owning compio thread"
)]

mod jobs;
mod management;
mod placement;
mod policy;

pub use placement::Placed;

use crate::{
    eligibility::{EligibilitySource, ZoneId},
    models::{
        assignments, management as management_records, placement_receipts, workers, Worker,
    },
    queue::{lock_scope, register_scope_in, Budget},
    Error, Queue,
};
use std::{rc::Rc, time::Duration};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{
        RegisterWorker, RegisteredWorker, Revision, UnixMillis, WorkerId, WorkerState,
    },
};
use zeroship_data_orm::{
    orm::{
        ConflictTarget, Database, Entity, FieldOrder, Filter, FromRow, Insertable, Operation,
        Output,
    },
    Value,
};

/// Native placement policy; connection and authentication configuration belongs to the host.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub worker_ttl: Duration,
    pub assignment_ttl: Duration,
    pub batch_limit: usize,
    pub max_pending_management: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            worker_ttl: Duration::from_secs(30),
            assignment_ttl: Duration::from_secs(30),
            batch_limit: 128,
            max_pending_management: 1024,
        }
    }
}

impl Options {
    /// # Errors
    /// Rejects empty limits and durations that cannot be stored.
    pub fn validate(&self) -> Result<(), Error> {
        duration_ms(self.worker_ttl)?;
        duration_ms(self.assignment_ttl)?;
        if self.batch_limit == 0
            || self.max_pending_management == 0
            || self.max_pending_management > crate::management::MAX_PENDING_COMMANDS
            || i64::try_from(self.batch_limit).is_err()
            || i64::try_from(self.max_pending_management).is_err()
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}

/// Host-authenticated coordination over the same physical namespace as job delivery.
///
/// Placement eligibility comes from the injected [`EligibilitySource`]: the
/// app's execution zone and deletion, and each instance's enroller zone and
/// enrollment. Registration, placement, renewal and ownership read it under
/// their locks; no worker request supplies a zone.
#[derive(Debug, Clone)]
pub struct Coordinator {
    queue: Queue,
    options: Options,
    eligibility: Rc<dyn EligibilitySource>,
}

#[derive(Insertable)]
#[orm(entity = workers)]
struct WorkerIdentity {
    id: String,
}

impl Coordinator {
    /// Use an already provisioned native queue; construction grants no database authority.
    ///
    /// # Errors
    /// Rejects invalid policy and incompatible generated model metadata.
    pub fn new(
        queue: Queue,
        options: Options,
        eligibility: Rc<dyn EligibilitySource>,
    ) -> Result<Self, Error> {
        options.validate()?;
        queue.database.entity::<workers::Entity>()?;
        queue.database.entity::<assignments::Entity>()?;
        queue.database.entity::<placement_receipts::Entity>()?;
        queue.database.entity::<management_records::Entity>()?;
        queue.database.entity::<crate::models::management_scopes::Entity>()?;
        Ok(Self {
            queue,
            options,
            eligibility,
        })
    }

    /// The platform queue this coordinator places work from.
    #[must_use]
    pub const fn queue(&self) -> &Queue {
        &self.queue
    }

    /// The trusted source of zone and enrollment facts.
    #[must_use]
    pub fn eligibility(&self) -> &dyn EligibilitySource {
        self.eligibility.as_ref()
    }

    /// Soft liveness never revives an expired placement or a draining instance.
    /// The registration records the zone of the enroller Control verified for
    /// this instance, read after the worker row lock and again before commit;
    /// an instance whose enrollment is no longer active cannot renew.
    ///
    /// # Errors
    /// Rejects storage failures, inactive or unknown enrollment and attempts to
    /// make a draining instance ready.
    pub async fn register(
        &self,
        worker: &WorkerId,
        request: &RegisterWorker,
    ) -> Result<RegisteredWorker, Error> {
        self.queue
            .transact(|tx| async move {
                // Preserve existing metadata while serializing first registration
                // and renewal. A new identity starts expired until this transaction
                // accepts its heartbeat; schema defaults never grant liveness.
                let workers = tx.entity::<workers::Entity>()?;
                let previous: Worker = workers
                    .upsert(
                        WorkerIdentity {
                            id: worker.as_str().into(),
                        },
                        ConflictTarget::new(workers::id),
                    )
                    .await?;
                if registered(&previous)?.state == WorkerState::Draining
                    && request.state == WorkerState::Ready
                {
                    return Err(Error::Conflict);
                }
                let zone = self.enrolled_zone(worker).await?;
                // An instance's zone is frozen with its enrollment; a change
                // means the stored registration no longer describes it.
                if previous
                    .execution_zone_id
                    .as_deref()
                    .is_some_and(|stored| stored != zone.as_str())
                {
                    return Err(Error::Denied);
                }
                // Draining is terminal for this enrolled process. A delayed ready
                // request cannot restore it, even after its heartbeat expires.
                let expires = deadline(self.queue.clock.now().await?, self.options.worker_ttl)?;
                let changes = workers::capacity
                    .set(i64::from(request.capacity.get()))?
                    .and(workers::state.set(match request.state {
                        WorkerState::Ready => "ready",
                        WorkerState::Draining => "draining",
                    })?)?
                    .and(workers::expires_at.set(expires)?)?
                    .and(workers::execution_zone_id.set(Some(zone.as_str()))?)?;
                let stored: Worker = workers
                    .update(workers::id.eq(worker.as_str())?, changes)
                    .await?
                    .ok_or(Error::Storage)?;
                if self.enrolled_zone(worker).await? != zone {
                    return Err(Error::Denied);
                }
                registered(&stored)
            })
            .await
    }

    /// The zone of an instance whose enrollment is active.
    async fn enrolled_zone(&self, worker: &WorkerId) -> Result<ZoneId, Error> {
        match self.eligibility.worker(worker).await? {
            Some(facts) if facts.active => Ok(facts.zone),
            _ => Err(Error::Denied),
        }
    }

    /// # Errors
    /// Rejects unavailable or malformed worker metadata.
    pub async fn ready_workers(
        &self,
        after: Option<&WorkerId>,
    ) -> Result<Vec<RegisteredWorker>, Error> {
        self.queue
            .transact(|tx| async move {
                let now = self.queue.clock.now().await?;
                let mut filter = workers::state
                    .eq("ready")?
                    .and(workers::expires_at.gt(now)?);
                if let Some(after) = after {
                    filter = filter.and(workers::id.gt(after.as_str())?);
                }
                rows::<workers::Entity, Worker>(
                    &tx,
                    filter,
                    [workers::id.asc()],
                    self.options.batch_limit,
                )
                .await?
                .iter()
                .map(registered)
                .collect()
            })
            .await
    }

    async fn scope(&self, tx: &Database, app: &AppId, create: bool) -> Result<(), Error> {
        if create {
            register_scope_in(tx, app).await?;
        }
        lock_scope(tx, app).await
    }

    fn budget(&self) -> Budget {
        Budget::new(self.queue.options.transaction_timeout)
    }
}

fn duration_ms(value: Duration) -> Result<i64, Error> {
    i64::try_from(value.as_millis())
        .ok()
        .filter(|value| *value > 0)
        .ok_or(Error::Invalid)
}
fn deadline(now: i64, duration: Duration) -> Result<i64, Error> {
    now.checked_add(duration_ms(duration)?)
        .ok_or(Error::Storage)
}
fn revision(value: i64) -> Result<Revision, Error> {
    value.try_into().map_err(|_| Error::Storage)
}
fn timestamp(value: i64) -> Result<UnixMillis, Error> {
    value.try_into().map_err(|_| Error::Storage)
}

fn registered(row: &Worker) -> Result<RegisteredWorker, Error> {
    Ok(RegisteredWorker {
        worker_id: WorkerId::parse(&row.id).map_err(|_| Error::Storage)?,
        capacity: u32::try_from(row.capacity)
            .ok()
            .and_then(std::num::NonZeroU32::new)
            .ok_or(Error::Storage)?,
        state: match row.state.as_str() {
            "ready" => WorkerState::Ready,
            "draining" => WorkerState::Draining,
            _ => return Err(Error::Storage),
        },
        expires_at: timestamp(row.expires_at)?,
    })
}

async fn rows<E: Entity, R: FromRow<E>>(
    tx: &Database,
    filter: Filter<E>,
    order: impl IntoIterator<Item = FieldOrder<E>>,
    limit: usize,
) -> Result<Vec<R>, Error> {
    let mut query = tx.entity::<E>()?.query().filter(filter);
    for field in order {
        query = query.order_by(field);
    }
    Ok(query
        .limit(i64::try_from(limit).map_err(|_| Error::Storage)?)?
        .all::<R>()
        .await?)
}

async fn one<E: Entity, R: FromRow<E>>(
    tx: &Database,
    filter: Filter<E>,
) -> Result<Option<R>, Error> {
    Ok(tx
        .entity::<E>()?
        .query()
        .filter(filter)
        .first::<R>()
        .await?)
}

async fn update<E: Entity>(tx: &Database, filter: Value, patch: Value) -> Result<(), Error> {
    match tx
        .collection(E::COLLECTION)?
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

async fn count<E: Entity>(tx: &Database, filter: Filter<E>) -> Result<i64, Error> {
    match tx.entity::<E>()?.count(filter).await? {
        n if n >= 0 => Ok(n),
        _ => Err(Error::Storage),
    }
}
