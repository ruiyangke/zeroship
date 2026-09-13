//! Durable reconciliation responsibility, independent of worker liveness.
#![expect(
    clippy::future_not_send,
    reason = "recovery shares the manager's owning compio runtime"
)]

use crate::{models::recovery_scopes, queue, Error, Queue};
use std::time::Duration;
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::Revision,
    workflow_jobs::{DeploymentId, JobId, JobOperation, JobSpec},
};
use zeroship_data_orm::{
    orm::{Database, Entity, FindOptions, FromRow, Operation, Output},
    value,
};

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub interval: Duration,
    pub page_size: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(30),
            page_size: 128,
        }
    }
}

/// The trusted platform host registers responsibility before enabling ingress.
///
/// Worker registration, placement and heartbeat operations cannot postpone it.
/// There is no age-based retirement or inferred drain operation.
#[derive(Debug, Clone)]
pub struct Recovery {
    queue: Queue,
    interval_ms: i64,
    page_size: u32,
}

#[derive(FromRow)]
#[orm(entity = recovery_scopes)]
struct Stored {
    deployment_id: String,
    activation_revision: i64,
    next_due_at: i64,
    pending_job_id: Option<String>,
}

#[derive(FromRow)]
#[orm(entity = recovery_scopes)]
struct Scope {
    id: String,
}

impl Recovery {
    /// # Errors
    /// Rejects unrepresentable intervals and pages outside the ORM row bound.
    pub fn new(queue: Queue, options: Options) -> Result<Self, Error> {
        let interval_ms = i64::try_from(options.interval.as_millis())
            .ok()
            .filter(|value| *value > 0)
            .ok_or(Error::Invalid)?;
        if options.page_size == 0
            || i64::from(options.page_size) > zeroship_data_orm::sql::MAX_ROW_LIMIT
        {
            return Err(Error::Invalid);
        }
        Ok(Self {
            queue,
            interval_ms,
            page_size: options.page_size,
        })
    }

    /// Persist responsibility using a platform-authorized deployment activation.
    /// Repeated registration preserves the deadline. A newer activation changes
    /// only the deployment for future jobs; a pending job keeps its original pin.
    ///
    /// # Errors
    /// Rejects stale or conflicting activations and unavailable platform storage.
    pub async fn ensure(
        &self,
        app: &AppId,
        deployment: &DeploymentId,
        activation_revision: Revision,
    ) -> Result<(), Error> {
        self.queue.transact(|tx| async move {
            queue::register_scope_in(&tx, app).await?;
            queue::lock_scope(&tx, app).await?;
            let scopes = tx.collection(recovery_scopes::Entity::COLLECTION)?;
            if let Some(stored) = load(&tx, app).await? {
                let revision = Revision::try_from(stored.activation_revision).map_err(|_| Error::Storage)?;
                if activation_revision < revision
                    || (activation_revision == revision && stored.deployment_id != deployment.as_str()) {
                    return Err(Error::Conflict);
                }
                if activation_revision > revision {
                    scopes.update(value!({"id":app.as_str()}), value!({
                        "deployment_id":deployment.as_str(), "activation_revision":activation_revision.get(),
                    })).await?;
                }
            } else {
                scopes.insert(value!({
                    "id":app.as_str(), "deployment_id":deployment.as_str(),
                    "activation_revision":activation_revision.get(), "next_due_at":self.queue.clock.now().await?,
                })).await?;
            }
            Ok(())
        }).await
    }

    /// Page due obligations by app identity, including apps with healthy owners.
    /// Continue after the last returned app and restart a sweep from the beginning
    /// after an empty page. Work that becomes due behind the cursor joins that sweep.
    ///
    /// # Errors
    /// Reports unavailable or malformed platform storage.
    pub async fn due(&self, after: Option<&AppId>) -> Result<Vec<AppId>, Error> {
        self.queue
            .transact(|tx| async move {
                let source = tx.entity::<recovery_scopes::Entity>()?.alias("r")?;
                let mut filter = source
                    .column(recovery_scopes::next_due_at)
                    .lte(self.queue.clock.now().await?)?;
                if let Some(after) = after {
                    filter = zeroship_data_orm::sql::Predicate::And(vec![
                        filter,
                        source.column(recovery_scopes::id).gt(after.as_str())?,
                    ]);
                }
                tx.from(&source)
                    .filter(filter)
                    .order_by(source.column(recovery_scopes::id).asc())
                    .select(source.row::<Scope>())?
                    .limit(i64::from(self.page_size))?
                    .all()
                    .await?
                    .into_iter()
                    .map(|row| AppId::parse(&row.id).map_err(|_| Error::Storage))
                    .collect()
            })
            .await
    }

    /// Atomically publish due reconciliation and record its deadline and identity.
    /// An unsettled job is returned unchanged, including after manager restart or
    /// loss of all workers. Failed publication leaves responsibility due.
    ///
    /// # Errors
    /// Refuses an unregistered scope, exhausted time range and storage failures.
    pub async fn dispatch(&self, app: &AppId) -> Result<Option<JobSpec>, Error> {
        self.queue.transact(|tx| async move {
            queue::lock_scope(&tx, app).await?;
            let stored = load(&tx, app).await?.ok_or(Error::Denied)?;
            if let Some(pending) = &stored.pending_job_id {
                let job = queue::load(&tx, app, pending).await?.ok_or(Error::Storage)?;
                match job.state.as_str() {
                    "ready" | "leased" => return Ok(Some(job.spec()?)),
                    "settled" => {},
                    _ => return Err(Error::Storage),
                }
            }
            let now = self.queue.clock.now().await?;
            if stored.next_due_at > now { return Ok(None); }
            let next_due_at = now.checked_add(self.interval_ms).ok_or(Error::Capacity)?;
            let spec = JobSpec {
                id: JobId::mint(), app_id: app.clone(),
                deployment_id: DeploymentId::parse(&stored.deployment_id).map_err(|_| Error::Storage)?,
                operation: JobOperation::Reconcile {},
                available_at: now.try_into().map_err(|_| Error::Storage)?,
            };
            self.queue.insert(&tx, &spec, now).await?;
            let updated = tx.collection(recovery_scopes::Entity::COLLECTION)?.execute(Operation::Update {
                filter: value!({"id":app.as_str()}),
                patch: value!({"next_due_at":next_due_at,"pending_job_id":spec.id.as_str()}), many: true,
            }).await?;
            if !matches!(updated, Output::Count(1)) { return Err(Error::Storage); }
            Ok(Some(spec))
        }).await
    }
}

async fn load(tx: &Database, app: &AppId) -> Result<Option<Stored>, Error> {
    Ok(tx
        .entity::<recovery_scopes::Entity>()?
        .find::<Stored>(
            recovery_scopes::id.eq(app.as_str())?,
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .next())
}
