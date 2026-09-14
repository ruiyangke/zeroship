//! Durable reconciliation and collection duties, independent of worker liveness.
#![expect(
    clippy::future_not_send,
    reason = "recovery shares the manager's owning compio runtime"
)]

use crate::{
    models::{recovery_duties, recovery_scopes},
    queue, Error, Queue,
};

mod duties;
use duties::Duty;
use std::time::Duration;
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::Revision,
    workflow_jobs::{DeploymentId, JobId, JobOperation, JobOutcome, JobSpec},
};
use zeroship_data_orm::orm::{Database, FromRow, Insertable};

/// Independent periodic maintenance responsibilities for a registered app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DutyKind {
    Reconcile,
    Collect,
}

impl DutyKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reconcile => "reconcile",
            Self::Collect => "collect",
        }
    }

    const fn operation(self) -> JobOperation {
        match self {
            Self::Reconcile => JobOperation::Reconcile {},
            Self::Collect => JobOperation::Collect {},
        }
    }
}

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
}

#[derive(FromRow)]
#[orm(entity = recovery_scopes)]
struct Scope {
    id: String,
}

#[derive(Insertable)]
#[orm(entity = recovery_scopes)]
struct NewScope<'a> {
    id: &'a str,
    deployment_id: &'a str,
    activation_revision: i64,
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
    /// only activation provenance; pending duties keep their identities.
    ///
    /// # Errors
    /// Rejects stale or conflicting activations and unavailable platform storage.
    pub async fn ensure(
        &self,
        app: &AppId,
        deployment: &DeploymentId,
        activation_revision: Revision,
    ) -> Result<(), Error> {
        self.queue
            .transact(|tx| async move {
                queue::register_scope_in(&tx, app).await?;
                queue::lock_scope(&tx, app).await?;
                ensure_in(
                    &tx,
                    app,
                    deployment,
                    activation_revision,
                    self.queue.clock.now().await?,
                )
                .await
            })
            .await
    }

    /// Page due obligations by app identity, including apps with healthy owners.
    /// Continue after the last returned app and restart a sweep from the beginning
    /// after an empty page. Work that becomes due behind the cursor joins that sweep.
    ///
    /// # Errors
    /// Reports unavailable or malformed platform storage.
    pub async fn due(&self, kind: DutyKind, after: Option<&AppId>) -> Result<Vec<AppId>, Error> {
        self.queue
            .transact(|tx| async move {
                let mut filter = recovery_duties::kind
                    .eq(kind.as_str())?
                    .and(recovery_duties::next_due_at.lte(self.queue.clock.now().await?)?);
                if let Some(after) = after {
                    filter = filter.and(recovery_duties::app_id.gt(after.as_str())?);
                }
                tx.entity::<recovery_duties::Entity>()?
                    .query()
                    .filter(filter)
                    .order_by(recovery_duties::app_id.asc())
                    .limit(i64::from(self.page_size))?
                    .all::<Duty>()
                    .await?
                    .into_iter()
                    .map(|row| {
                        let app = AppId::parse(&row.app_id).map_err(|_| Error::Storage)?;
                        row.validate(&app, kind)?;
                        Ok(app)
                    })
                    .collect()
            })
            .await
    }

    /// Atomically publish a due maintenance job and record its deadline and identity.
    /// An unsettled job is returned unchanged, including after manager restart or
    /// loss of all workers. Failed publication leaves responsibility due.
    ///
    /// # Errors
    /// Refuses an unregistered scope, exhausted time range and storage failures.
    pub async fn dispatch(&self, app: &AppId, kind: DutyKind) -> Result<Option<JobSpec>, Error> {
        self.queue
            .transact(|tx| async move {
                queue::lock_scope(&tx, app).await?;
                let provenance = load(&tx, app).await?.ok_or(Error::Denied)?;
                validate_provenance(&provenance)?;
                let stored = duties::load(&tx, app, kind).await?.ok_or(Error::Storage)?;
                if let Some(pending) = stored.pending(&tx, app, kind).await? {
                    return Ok(Some(pending));
                }
                let now = self.queue.clock.now().await?;
                if stored.next_due_at > now {
                    return Ok(None);
                }
                let next_due_at = now.checked_add(self.interval_ms).ok_or(Error::Capacity)?;
                let spec = JobSpec {
                    id: JobId::mint(),
                    app_id: app.clone(),
                    operation: kind.operation(),
                    available_at: now.try_into().map_err(|_| Error::Storage)?,
                };
                self.queue.insert(&tx, &spec, now).await?;
                let updated = tx
                    .entity::<recovery_duties::Entity>()?
                    .update_many(
                        recovery_duties::id.eq(stored.id.as_str())?,
                        recovery_duties::next_due_at
                            .set(next_due_at)?
                            .and(recovery_duties::pending_job_id.set(Some(spec.id.as_str()))?)?,
                    )
                    .await?;
                if updated != 1 {
                    return Err(Error::Storage);
                }
                Ok(Some(spec))
            })
            .await
    }
}

/// The caller holds the queue's app lock in the activation transaction.
pub(crate) async fn ensure_in(
    tx: &Database,
    app: &AppId,
    deployment: &DeploymentId,
    activation_revision: Revision,
    now: i64,
) -> Result<(), Error> {
    let scopes = tx.entity::<recovery_scopes::Entity>()?;
    if let Some(stored) = load(tx, app).await? {
        let revision = validate_revision(&stored, deployment, activation_revision)?;
        duties::validate_pair(tx, app).await?;
        if activation_revision > revision {
            let changed = scopes
                .update_many(
                    recovery_scopes::id
                        .eq(app.as_str())?
                        .and(recovery_scopes::activation_revision.eq(revision.get())?),
                    recovery_scopes::deployment_id
                        .set(deployment.as_str())?
                        .and(
                            recovery_scopes::activation_revision.set(activation_revision.get())?,
                        )?,
                )
                .await?;
            if changed != 1 {
                return Err(Error::Storage);
            }
        }
    } else {
        let created = scopes
            .insert::<_, Scope>(NewScope {
                id: app.as_str(),
                deployment_id: deployment.as_str(),
                activation_revision: activation_revision.get(),
            })
            .await?;
        if created.id != app.as_str() {
            return Err(Error::Storage);
        }
        duties::create_pair(tx, app, now).await?;
    }
    Ok(())
}

fn validate_provenance(stored: &Stored) -> Result<Revision, Error> {
    DeploymentId::parse(&stored.deployment_id).map_err(|_| Error::Storage)?;
    Revision::try_from(stored.activation_revision).map_err(|_| Error::Storage)
}

fn validate_revision(
    stored: &Stored,
    deployment: &DeploymentId,
    activation_revision: Revision,
) -> Result<Revision, Error> {
    let revision = validate_provenance(stored)?;
    if activation_revision < revision
        || (activation_revision == revision && stored.deployment_id != deployment.as_str())
    {
        return Err(Error::Conflict);
    }
    Ok(revision)
}

async fn load(tx: &Database, app: &AppId) -> Result<Option<Stored>, Error> {
    Ok(tx
        .entity::<recovery_scopes::Entity>()?
        .query()
        .filter(recovery_scopes::id.eq(app.as_str())?)
        .first::<Stored>()
        .await?)
}

/// The queue calls this only for a fresh settlement under its app lock. A page
/// with more work advances the manager's deadline without retiring responsibility.
/// Stale, unrelated, and already-due pages leave the deadline unchanged.
pub(crate) async fn settled_page(
    tx: &Database,
    job: &JobSpec,
    outcome: JobOutcome,
    now: i64,
) -> Result<(), Error> {
    if !matches!(outcome, JobOutcome::Waiting {}) {
        return Ok(());
    }
    let kind = match job.operation {
        JobOperation::Reconcile {} => DutyKind::Reconcile,
        JobOperation::Collect {} => DutyKind::Collect,
        _ => return Ok(()),
    };
    let Some(duty) = duties::load(tx, &job.app_id, kind).await? else {
        return Ok(());
    };
    if duty.pending_job_id.as_deref() != Some(job.id.as_str()) || duty.next_due_at <= now {
        return Ok(());
    }
    let changed = tx
        .entity::<recovery_duties::Entity>()?
        .update_many(
            recovery_duties::id.eq(duty.id.as_str())?,
            recovery_duties::next_due_at.set(now)?,
        )
        .await?;
    if changed != 1 {
        return Err(Error::Storage);
    }
    Ok(())
}
