//! Creator-owned, immutable queue publication intents.
//!
//! Journal transitions write intents before COMMIT. Network publication happens
//! after it; only a matching manager receipt confirms an intent. Neither history
//! collection nor an unknown remote outcome retires this deduplication state.

#![expect(
    clippy::future_not_send,
    reason = "creator journal and metadata transport use their owning compio thread"
)]

use super::{
    app::{decode, encode, lock_app, not_found, parse_state},
    models::{job_publications as publications, runs},
    store::Transaction,
    AppWorkflows,
};
use crate::WorkflowServiceError;
use std::future::Future;
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{AssignedScope, FailureCode, Revision, RunId},
    workflow_jobs::{
        BroadcastId, DeploymentId, JobId, JobOperation, JobSpec, PropagationId, SubmitJob,
    },
};
use zeroship_data_orm::{
    orm::{Entity, FindOptions, FromRow, Insertable, Operation, Output},
    sql::MAX_ROW_LIMIT,
    value,
};
use zeroship_workflow_client::WorkerCoordinator;

/// Host-bound metadata publisher, shared by remote and local compositions.
/// Implementations must durably submit the immutable job before acknowledging it.
pub trait JobPublisher {
    fn app_id(&self) -> &AppId;
    fn submit(&self, job: &JobSpec) -> impl Future<Output = Result<JobSpec, WorkflowServiceError>>;
}

/// Authenticated publication under the worker's current app placement.
#[derive(Debug)]
pub struct AssignedPublisher<'a> {
    client: &'a WorkerCoordinator,
    scope: AssignedScope,
}

impl<'a> AssignedPublisher<'a> {
    #[must_use]
    pub const fn new(client: &'a WorkerCoordinator, scope: AssignedScope) -> Self {
        Self { client, scope }
    }
}

impl JobPublisher for AssignedPublisher<'_> {
    fn app_id(&self) -> &AppId {
        &self.scope.app_id
    }

    async fn submit(&self, job: &JobSpec) -> Result<JobSpec, WorkflowServiceError> {
        self.client
            .submit_job(&SubmitJob {
                scope: self.scope.clone(),
                job: job.clone(),
            })
            .await
            .map_err(|error| match error {
                zeroship_workflow_client::Error::Refused(FailureCode::Denied) => {
                    WorkflowServiceError::PermissionDenied
                }
                zeroship_workflow_client::Error::Refused(FailureCode::Conflict) => {
                    WorkflowServiceError::Conflict("workflow job publication conflicts".into())
                }
                zeroship_workflow_client::Error::Refused(FailureCode::Capacity) => {
                    WorkflowServiceError::ResourceExhausted("workflow queue is full".into())
                }
                zeroship_workflow_client::Error::Timeout => WorkflowServiceError::Timeout,
                _ => WorkflowServiceError::Unavailable("workflow job publication failed".into()),
            })
    }
}

#[derive(FromRow, Insertable)]
#[orm(entity = publications)]
struct Intent {
    id: String,
    app_id: String,
    deploy_id: Option<String>,
    run_id: Option<String>,
    generation: Option<i64>,
    frontier_revision: Option<i64>,
    broadcast_id: Option<String>,
    broadcast_revision: Option<i64>,
    propagation_id: Option<String>,
    propagation_revision: Option<i64>,
    available_at: i64,
    specification: String,
    created_at: i64,
    confirmed_at: Option<i64>,
}

impl Intent {
    const fn frontier_absent(&self) -> bool {
        self.deploy_id.is_none()
            && self.run_id.is_none()
            && self.generation.is_none()
            && self.frontier_revision.is_none()
    }
    const fn broadcast_absent(&self) -> bool {
        self.broadcast_id.is_none() && self.broadcast_revision.is_none()
    }
    const fn propagation_absent(&self) -> bool {
        self.propagation_id.is_none() && self.propagation_revision.is_none()
    }

    fn job(&self, app: &AppId) -> Result<JobSpec, WorkflowServiceError> {
        let job: JobSpec = decode(&self.specification)?;
        if job.app_id != *app
            || self.app_id != app.as_str()
            || job.id.as_str() != self.id
            || job.available_at.get() != self.available_at
        {
            return Err(invalid());
        }
        let valid = match &job.operation {
            JobOperation::Advance {
                deployment_id,
                run_id,
                generation,
                revision,
            } => {
                self.deploy_id.as_deref() == Some(deployment_id.as_str())
                    && self.run_id.as_deref() == Some(run_id.as_str())
                    && self.generation == Some(i64::from(*generation))
                    && self.frontier_revision == Some(revision.get())
                    && self.broadcast_absent()
                    && self.propagation_absent()
            }
            JobOperation::Fanout {
                broadcast_id,
                revision,
            } => {
                self.broadcast_id.as_deref() == Some(broadcast_id.as_str())
                    && self.broadcast_revision == Some(revision.get())
                    && self.frontier_absent()
                    && self.propagation_absent()
            }
            JobOperation::Propagate {
                propagation_id,
                revision,
            } => {
                self.propagation_id.as_deref() == Some(propagation_id.as_str())
                    && self.propagation_revision == Some(revision.get())
                    && self.frontier_absent()
                    && self.broadcast_absent()
            }
            _ => false,
        };
        if !valid {
            return Err(invalid());
        }
        Ok(job)
    }

    fn new(job: &JobSpec, now: i64) -> Result<Self, WorkflowServiceError> {
        let mut intent = Self {
            id: job.id.as_str().to_owned(),
            app_id: job.app_id.as_str().to_owned(),
            deploy_id: None,
            run_id: None,
            generation: None,
            frontier_revision: None,
            broadcast_id: None,
            broadcast_revision: None,
            propagation_id: None,
            propagation_revision: None,
            available_at: job.available_at.get(),
            specification: encode(job)?,
            created_at: now,
            confirmed_at: None,
        };
        match &job.operation {
            JobOperation::Advance {
                deployment_id,
                run_id,
                generation,
                revision,
            } => {
                intent.deploy_id = Some(deployment_id.as_str().to_owned());
                intent.run_id = Some(run_id.as_str().to_owned());
                intent.generation = Some(i64::from(*generation));
                intent.frontier_revision = Some(revision.get());
            }
            JobOperation::Fanout {
                broadcast_id,
                revision,
            } => {
                intent.broadcast_id = Some(broadcast_id.as_str().to_owned());
                intent.broadcast_revision = Some(revision.get());
            }
            JobOperation::Propagate {
                propagation_id,
                revision,
            } => {
                intent.propagation_id = Some(propagation_id.as_str().to_owned());
                intent.propagation_revision = Some(revision.get());
            }
            _ => return Err(invalid()),
        }
        Ok(intent)
    }
}

impl AppWorkflows {
    /// Read a bounded page of pending metadata, including future timers.
    /// Restart the sweep after an empty page so earlier insertions join the next pass.
    ///
    /// # Errors
    /// Rejects invalid bounds, unavailable host policy and malformed journal state.
    pub async fn pending_jobs(
        &self,
        after: Option<&JobId>,
        limit: u32,
    ) -> Result<Vec<JobSpec>, WorkflowServiceError> {
        if limit == 0 || i64::from(limit) > MAX_ROW_LIMIT {
            return Err(WorkflowServiceError::InvalidRequest(
                "invalid workflow publication page size".into(),
            ));
        }
        let captured = self.capture_policy();
        captured
            .run(async {
                captured.check()?;
                let tx = self.service.begin().await?;
                let source = tx.database().entity::<publications::Entity>()?.alias("p")?;
                let mut predicate = source
                    .column(publications::app_id)
                    .eq(self.app.as_str())?
                    .and(source.column(publications::confirmed_at).eq(None::<i64>)?);
                if let Some(after) = after {
                    predicate = predicate.and(source.column(publications::id).gt(after.as_str())?);
                }
                let rows = tx
                    .database()
                    .from(&source)
                    .filter(predicate)
                    .order_by(source.column(publications::id).asc())
                    .select(source.row::<Intent>())?
                    .limit(i64::from(limit))?
                    .all()
                    .await?;
                let jobs = rows
                    .iter()
                    .map(|row| row.job(&self.app))
                    .collect::<Result<_, _>>()?;
                captured.check()?;
                tx.commit().await?;
                Ok(jobs)
            })
            .await
    }

    /// Publish a persisted intent and confirm only the matching receipt.
    /// No creator transaction is held across manager I/O. Lost replies and failed
    /// confirmation writes leave the same immutable intent available for retry.
    ///
    /// # Errors
    /// Refuses foreign scope, missing intents and changed acknowledgements;
    /// propagates journal and publication failures without discarding the intent.
    pub async fn publish_job(
        &self,
        id: &JobId,
        publisher: &impl JobPublisher,
    ) -> Result<JobSpec, WorkflowServiceError> {
        self.publish_job_authorized(id, publisher, None).await
    }

    pub(super) async fn publish_job_authorized(
        &self,
        id: &JobId,
        publisher: &impl JobPublisher,
        authority: Option<&super::delivery::CapturedLease>,
    ) -> Result<JobSpec, WorkflowServiceError> {
        if publisher.app_id() != &self.app {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        let captured = self.capture_policy();
        captured
            .run(async {
                if let Some(authority) = authority {
                    authority.check(self)?;
                }
                let tx = self.service.begin().await?;
                let intent = read(&tx, &self.app, id).await?;
                let job = intent.job(&self.app)?;
                captured.recheck()?;
                tx.commit().await?;
                if intent.confirmed_at.is_some() {
                    return Ok(job);
                }
                captured.check()?;
                if let Some(authority) = authority {
                    authority.check(self)?;
                }
                if publisher.submit(&job).await? != job {
                    return Err(WorkflowServiceError::Conflict(
                        "workflow job acknowledgement does not match its intent".into(),
                    ));
                }
                captured.check()?;
                let mut tx = self.service.begin().await?;
                lock_app(&mut tx, &self.app).await?;
                captured.check()?;
                if let Some(authority) = authority {
                    authority.check(self)?;
                }
                let current = read(&tx, &self.app, id).await?;
                if current.job(&self.app)? != job {
                    return Err(invalid());
                }
                if current.confirmed_at.is_none() {
                    let now = tx.now().await?;
                    captured.check()?;
                    if let Some(authority) = authority {
                        authority.check(self)?;
                    }
                    tx.database()
                        .collection(publications::Entity::COLLECTION)?
                        .update(
                            value!({"app_id":self.app.as_str(), "id":id.as_str()}),
                            value!({"confirmed_at":now}),
                        )
                        .await?;
                }
                captured.check()?;
                if let Some(authority) = authority {
                    authority.check(self)?;
                }
                tx.commit().await?;
                Ok(job)
            })
            .await
    }
}

#[derive(FromRow)]
#[orm(entity = runs)]
struct RunFrontier {
    deploy_id: String,
    generation: i64,
    frontier_revision: i64,
    due_at: Option<i64>,
    task_id: Option<String>,
    state: String,
}

/// The caller holds the app lock. Task delivery attempts do not advance this
/// revision; only a committed creator transition supersedes its old frontier.
pub(super) async fn advance(
    tx: &Transaction,
    app: &AppId,
    run: &str,
    now: i64,
) -> Result<(), WorkflowServiceError> {
    Box::pin(advance_job(tx, app, run, now)).await.map(|_| ())
}

/// [`advance`] that also returns the recorded runnable intent, if any, so a
/// delivered page can retain its exact successor specification.
pub(super) async fn advance_job(
    tx: &Transaction,
    app: &AppId,
    run: &str,
    now: i64,
) -> Result<Option<JobSpec>, WorkflowServiceError> {
    let changed = tx
        .database()
        .collection(runs::Entity::COLLECTION)?
        .execute(Operation::Update {
            filter: value!({"app_id":app.as_str(), "id":run, "frontier_revision":{"$lt":i64::MAX}}),
            patch: value!({"$inc":{"frontier_revision":1}}),
            many: true,
        })
        .await?;
    if !matches!(changed, Output::Count(1)) {
        return Err(WorkflowServiceError::ResourceExhausted(
            "workflow frontier revision exhausted".into(),
        ));
    }
    Box::pin(record_job(tx, app, run, now)).await
}

/// Record the final runnable frontier in the same transaction as its cause.
/// The scoped unique key coalesces repeated observations without changing a
/// previously published job's due time or immutable identity.
pub(super) async fn record(
    tx: &Transaction,
    app: &AppId,
    run: &str,
    now: i64,
) -> Result<(), WorkflowServiceError> {
    record_job(tx, app, run, now).await.map(|_| ())
}

pub(super) async fn record_job(
    tx: &Transaction,
    app: &AppId,
    run: &str,
    now: i64,
) -> Result<Option<JobSpec>, WorkflowServiceError> {
    let frontier = tx
        .database()
        .entity::<runs::Entity>()?
        .find::<RunFrontier>(runs::app_id.eq(app.as_str())?.and(runs::id.eq(run)?), one())
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| not_found("workflow run"))?;
    let Some(due) = frontier.due_at else {
        return Ok(None);
    };
    if frontier.task_id.is_some() || parse_state(&frontier.state)?.is_terminal() {
        return Ok(None);
    }
    let operation = JobOperation::Advance {
        deployment_id: DeploymentId::parse(&frontier.deploy_id).map_err(|_| invalid())?,
        run_id: RunId::parse(run).map_err(|_| invalid())?,
        generation: frontier.generation.try_into().map_err(|_| invalid())?,
        revision: Revision::try_from(frontier.frontier_revision).map_err(|_| invalid())?,
    };
    let existing = tx
        .database()
        .entity::<publications::Entity>()?
        .find::<Intent>(
            publications::app_id
                .eq(app.as_str())?
                .and(publications::run_id.eq(Some(run))?)
                .and(publications::generation.eq(Some(frontier.generation))?)
                .and(publications::frontier_revision.eq(Some(frontier.frontier_revision))?)
                .and(publications::available_at.eq(due)?),
            one(),
        )
        .await?;
    let job = JobSpec {
        id: existing
            .first()
            .map(|row| JobId::parse(&row.id).map_err(|_| invalid()))
            .transpose()?
            .unwrap_or_else(JobId::mint),
        app_id: app.clone(),
        operation,
        available_at: due.try_into().map_err(|_| invalid())?,
    };
    if let Some(existing) = existing.first() {
        if existing.job(app)? != job {
            return Err(invalid());
        }
        return Ok(Some(job));
    }
    insert(tx, &job, now).await?;
    Ok(Some(job))
}

async fn read(tx: &Transaction, app: &AppId, id: &JobId) -> Result<Intent, WorkflowServiceError> {
    tx.database()
        .entity::<publications::Entity>()?
        .find::<Intent>(
            publications::app_id
                .eq(app.as_str())?
                .and(publications::id.eq(id.as_str())?),
            one(),
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| not_found("workflow job publication"))
}

fn one() -> FindOptions {
    FindOptions {
        limit: Some(1),
        ..Default::default()
    }
}

fn invalid() -> WorkflowServiceError {
    WorkflowServiceError::Internal("invalid workflow job publication journal".into())
}

async fn insert(tx: &Transaction, job: &JobSpec, now: i64) -> Result<(), WorkflowServiceError> {
    let saved = tx
        .database()
        .entity::<publications::Entity>()?
        .insert::<_, Intent>(Intent::new(job, now)?)
        .await?;
    if saved.job(&job.app_id)? != *job {
        return Err(invalid());
    }
    Ok(())
}

pub(super) async fn exact(tx: &Transaction, job: &JobSpec) -> Result<(), WorkflowServiceError> {
    if read(tx, &job.app_id, &job.id).await?.job(&job.app_id)? != *job {
        return Err(invalid());
    }
    Ok(())
}

pub(super) async fn intent_job(
    tx: &Transaction,
    app: &AppId,
    id: &JobId,
) -> Result<JobSpec, WorkflowServiceError> {
    read(tx, app, id).await?.job(app)
}

pub(super) async fn fanout(
    tx: &Transaction,
    app: &AppId,
    broadcast: &BroadcastId,
    revision: Revision,
    now: i64,
) -> Result<JobSpec, WorkflowServiceError> {
    let existing = tx
        .database()
        .entity::<publications::Entity>()?
        .find::<Intent>(
            publications::app_id
                .eq(app.as_str())?
                .and(publications::broadcast_id.eq(Some(broadcast.as_str()))?)
                .and(publications::broadcast_revision.eq(Some(revision.get()))?),
            one(),
        )
        .await?;
    let operation = JobOperation::Fanout {
        broadcast_id: broadcast.clone(),
        revision,
    };
    if let Some(existing) = existing.first() {
        let job = existing.job(app)?;
        if job.operation != operation {
            return Err(invalid());
        }
        return Ok(job);
    }
    let job = JobSpec {
        id: JobId::mint(),
        app_id: app.clone(),
        operation,
        available_at: now.try_into().map_err(|_| invalid())?,
    };
    insert(tx, &job, now).await?;
    Ok(job)
}

/// Record one propagation page intent. The scoped unique projection returns the
/// same immutable job when an equal page was already recorded.
pub(super) async fn propagate(
    tx: &Transaction,
    app: &AppId,
    propagation: &PropagationId,
    revision: Revision,
    now: i64,
) -> Result<JobSpec, WorkflowServiceError> {
    let existing = tx
        .database()
        .entity::<publications::Entity>()?
        .find::<Intent>(
            publications::app_id
                .eq(app.as_str())?
                .and(publications::propagation_id.eq(Some(propagation.as_str()))?)
                .and(publications::propagation_revision.eq(Some(revision.get()))?),
            one(),
        )
        .await?;
    let operation = JobOperation::Propagate {
        propagation_id: propagation.clone(),
        revision,
    };
    if let Some(existing) = existing.first() {
        let job = existing.job(app)?;
        if job.operation != operation {
            return Err(invalid());
        }
        return Ok(job);
    }
    let job = JobSpec {
        id: JobId::mint(),
        app_id: app.clone(),
        operation,
        available_at: now.try_into().map_err(|_| invalid())?,
    };
    insert(tx, &job, now).await?;
    Ok(job)
}

/// The app lock prevents new intents while every pending specification is checked.
/// Nullable projections cannot conceal a code-dependent publication from release.
pub(super) async fn retains_deployment(
    tx: &Transaction,
    app: &AppId,
    deployment: &str,
) -> Result<bool, WorkflowServiceError> {
    let source = tx.database().entity::<publications::Entity>()?.alias("p")?;
    let mut after: Option<String> = None;
    loop {
        let mut predicate = source
            .column(publications::app_id)
            .eq(app.as_str())?
            .and(source.column(publications::confirmed_at).eq(None::<i64>)?);
        if let Some(after) = after.as_deref() {
            predicate = predicate.and(source.column(publications::id).gt(after)?);
        }
        let rows = tx
            .database()
            .from(&source)
            .filter(predicate)
            .order_by(source.column(publications::id).asc())
            .select(source.row::<Intent>())?
            .limit(128)?
            .all()
            .await?;
        if rows.is_empty() {
            return Ok(false);
        }
        for row in &rows {
            let job = row.job(app)?;
            if job
                .deployment_id()
                .is_some_and(|id| id.as_str() == deployment)
            {
                return Ok(true);
            }
        }
        after = rows.last().map(|row| row.id.clone());
    }
}
