//! Platform placement and management share the queue's ORM transaction and app lock.
#![expect(
    clippy::future_not_send,
    reason = "manager ORM handles remain on their owning compio thread"
)]

mod jobs;
mod management;
mod placement;

use crate::{
    models::{
        assignments, management as management_records, placement_receipts, queue_scopes, workers,
        Scope, Worker,
    },
    queue::{lock_scope, register_scope_in, Budget},
    Error, Queue,
};
use serde::de::DeserializeOwned;
use std::time::Duration;
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{
        RegisterWorker, RegisteredWorker, Revision, UnixMillis, WorkerId, WorkerState,
    },
};
use zeroship_data_orm::{
    orm::{Database, Entity, FromRow, Operation, Output},
    value, Value,
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
            || i64::try_from(self.batch_limit).is_err()
            || i64::try_from(self.max_pending_management).is_err()
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}

/// Host-authenticated coordination over the same physical namespace as job delivery.
#[derive(Debug, Clone)]
pub struct Coordinator {
    queue: Queue,
    options: Options,
}

impl Coordinator {
    /// Use an already provisioned native queue; construction grants no database authority.
    ///
    /// # Errors
    /// Rejects invalid policy and incompatible generated model metadata.
    pub fn new(queue: Queue, options: Options) -> Result<Self, Error> {
        options.validate()?;
        queue.database.entity::<workers::Entity>()?;
        queue.database.entity::<assignments::Entity>()?;
        queue.database.entity::<placement_receipts::Entity>()?;
        queue.database.entity::<management_records::Entity>()?;
        Ok(Self { queue, options })
    }

    /// Soft liveness never revives an expired placement.
    ///
    /// # Errors
    /// Rejects storage failures and invalid registration metadata.
    pub async fn register(
        &self,
        worker: &WorkerId,
        request: &RegisterWorker,
    ) -> Result<RegisteredWorker, Error> {
        self.queue.transact(|tx| async move {
            // Upsert preserves the existing id, including concurrent first registration.
            tx.collection(workers::Entity::COLLECTION)?.execute(Operation::Upsert {
                document: value!({
                    "id": worker.as_str(),
                    "capacity": i64::from(request.capacity.get()),
                    "state": match request.state { WorkerState::Ready => "ready", WorkerState::Draining => "draining" },
                    "expires_at": 0
                }),
                conflict_fields: value!(["id"]),
            }).await?;
            // The upsert holds the worker row. Sample after that wait.
            let expires = deadline(self.queue.clock.now().await?, self.options.worker_ttl)?;
            update::<workers::Entity>(&tx, value!({"id":worker.as_str()}), value!({"expires_at":expires})).await?;
            let stored = one::<workers::Entity, Worker>(&tx, value!({"id":worker.as_str()})).await?.ok_or(Error::Storage)?;
            registered(&stored)
        }).await
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
                let mut filter = value!({"state":"ready","expires_at":{"$gt":now}});
                if let Some(after) = after {
                    filter["id"] = value!({"$gt":after.as_str()});
                }
                rows::<workers::Entity, Worker>(
                    &tx,
                    filter,
                    value!({"id":1}),
                    self.options.batch_limit,
                )
                .await?
                .iter()
                .map(registered)
                .collect()
            })
            .await
    }

    /// Missing owners require recovery even when no wake hint was published.
    ///
    /// # Errors
    /// Rejects unavailable or malformed placement metadata.
    pub async fn recovery_scopes(&self, after: Option<&AppId>) -> Result<Vec<AppId>, Error> {
        self.queue
            .transact(|tx| async move {
                let mut cursor = after.map(|app| app.as_str().to_owned());
                let mut recovered = Vec::new();
                loop {
                    let filter = cursor
                        .as_ref()
                        .map_or_else(|| value!({}), |after| value!({"id":{"$gt":after}}));
                    let scopes = rows::<queue_scopes::Entity, Scope>(
                        &tx,
                        filter,
                        value!({"id":1}),
                        self.options.batch_limit,
                    )
                    .await?;
                    if scopes.is_empty() {
                        return Ok(recovered);
                    }
                    for row in scopes {
                        cursor = Some(row.id.clone());
                        let app = AppId::parse(&row.id).map_err(|_| Error::Storage)?;
                        if !self.has_owner(&tx, &app, None, false).await? {
                            recovered.push(app);
                            if recovered.len() == self.options.batch_limit {
                                return Ok(recovered);
                            }
                        }
                    }
                    // Filter ownership before ending the result page; a page of
                    // owned scopes must not hide later recoverable applications.
                }
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

async fn rows<E: Entity, R: FromRow<E> + DeserializeOwned>(
    tx: &Database,
    filter: Value,
    order: Value,
    limit: usize,
) -> Result<Vec<R>, Error> {
    let Output::Rows { rows, .. } = tx
        .collection(E::COLLECTION)?
        .find(
            filter,
            value!({"select":R::COLUMNS,"orderBy":order,"limit":limit}),
        )
        .await?
    else {
        return Err(Error::Storage);
    };
    rows.into_iter()
        .map(|row| zeroship_data_orm::value::from_value(row).map_err(|_| Error::Storage))
        .collect()
}

async fn one<E: Entity, R: FromRow<E> + DeserializeOwned>(
    tx: &Database,
    filter: Value,
) -> Result<Option<R>, Error> {
    Ok(rows::<E, R>(tx, filter, value!({}), 1)
        .await?
        .into_iter()
        .next())
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

async fn count<E: Entity>(tx: &Database, filter: Value) -> Result<i64, Error> {
    match tx
        .collection(E::COLLECTION)?
        .count(filter, value!({}))
        .await?
    {
        Output::Count(n) if n >= 0 => Ok(n),
        _ => Err(Error::Storage),
    }
}
