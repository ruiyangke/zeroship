//! Creator-owned, immutable queue publication intents.
//!
//! Journal transitions write intents before COMMIT. Network publication happens
//! after it; only a matching manager receipt confirms an intent. Neither history
//! collection nor an unknown remote outcome retires this deduplication state.
//!
//! # The key carries the identity
//!
//! A publishable job's id is [`publication_id`] of the work it names, not a
//! minted value, so the intent table's PRIMARY KEY is its deduplication key.
//! Recording an intent is a lookup by that id followed by an insert: two
//! transactions observing the same frontier, broadcast page or propagation page
//! compute the same id, both find nothing, and the second insert collides on the
//! key rather than adding a second job for the same work. Every read re-derives
//! the id from the specification it decoded, so a row whose content does not
//! produce the key it is stored under is a damaged journal.

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
    workflow_coordination::{AssignedScope, FailureCode, Revision, RunId, UnixMillis},
    workflow_jobs::{
        publication_id, BroadcastId, DeploymentId, JobId, JobOperation, JobSpec, PropagationId,
        SubmitJob,
    },
};
use zeroship_data_orm::{
    orm::{
        Entity, EntityAlias, EntityProjection, FindOptions, FromRow, Insertable, Operation, Output,
        ReadBuilder,
    },
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

/// One intent row: its identity, the specification that identity is derived
/// from, and whether a manager receipt confirmed it.
#[derive(FromRow, Insertable)]
#[orm(entity = publications)]
struct Record {
    id: String,
    app_id: String,
    specification: String,
    created_at: i64,
    confirmed_at: Option<i64>,
}

impl Record {
    fn new(job: &JobSpec, now: i64) -> Result<Self, WorkflowServiceError> {
        Ok(Self {
            id: job.id.as_str().to_owned(),
            app_id: job.app_id.as_str().to_owned(),
            specification: encode(job)?,
            created_at: now,
            confirmed_at: None,
        })
    }

    /// Decode the specification and check it against the key it is stored
    /// under.
    ///
    /// [`JobSpec::publication_id`] is `None` for the seven operations a creator
    /// journal never publishes, so an intent wearing one of them is refused
    /// here; for the three it does publish, the derived id must be the row's
    /// own id. That single equality replaces every column-by-column comparison
    /// a second copy of the tuple would need, and it covers the whole operation
    /// rather than the part a projection happened to carry.
    fn job(&self, app: &AppId) -> Result<JobSpec, WorkflowServiceError> {
        let job: JobSpec = decode(&self.specification)?;
        if job.app_id != *app
            || self.app_id != app.as_str()
            || job.id.as_str() != self.id
            || job.publication_id() != Some(job.id.clone())
        {
            return Err(invalid());
        }
        Ok(job)
    }
}

/// The intent table under its own alias, so predicates and projections name one
/// source rather than repeating the entity lookup.
struct Source {
    intents: EntityAlias<publications::Entity>,
}

impl Source {
    fn new(tx: &Transaction) -> Result<Self, WorkflowServiceError> {
        Ok(Self {
            intents: tx.database().entity::<publications::Entity>()?.alias("p")?,
        })
    }

    fn scan(&self, tx: &Transaction) -> ReadBuilder {
        tx.database().from(&self.intents)
    }

    fn projection(&self) -> EntityProjection<publications::Entity, Record, false> {
        self.intents.row::<Record>()
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
                let source = Source::new(&tx)?;
                let mut predicate = source
                    .intents
                    .column(publications::app_id)
                    .eq(self.app.as_str())?
                    .and(
                        source
                            .intents
                            .column(publications::confirmed_at)
                            .eq(None::<i64>)?,
                    );
                if let Some(after) = after {
                    predicate =
                        predicate.and(source.intents.column(publications::id).gt(after.as_str())?);
                }
                let rows: Vec<Record> = source
                    .scan(&tx)
                    .filter(predicate)
                    .order_by(source.intents.column(publications::id).asc())
                    .select(source.projection())?
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
/// The derived identity coalesces repeated observations without changing a
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
    Box::pin(publish(
        tx,
        app,
        operation,
        due.try_into().map_err(|_| invalid())?,
        now,
    ))
    .await
    .map(Some)
}

async fn read(tx: &Transaction, app: &AppId, id: &JobId) -> Result<Record, WorkflowServiceError> {
    existing(tx, app, id)
        .await?
        .ok_or_else(|| not_found("workflow job publication"))
}

/// The intent stored under one derived id, if the journal already holds it.
async fn existing(
    tx: &Transaction,
    app: &AppId,
    id: &JobId,
) -> Result<Option<Record>, WorkflowServiceError> {
    let source = Source::new(tx)?;
    Ok(source
        .scan(tx)
        .filter(
            source
                .intents
                .column(publications::app_id)
                .eq(app.as_str())?
                .and(source.intents.column(publications::id).eq(id.as_str())?),
        )
        .select(source.projection())?
        .limit(1)?
        .all()
        .await?
        .into_iter()
        .next())
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

/// Record one publishable operation, returning the immutable job that now owns
/// its identity.
///
/// The id is derived first, so an equal observation reads the intent it already
/// wrote instead of writing a second one, and two transactions racing the same
/// observation compute the same key: the loser's insert collides on the primary
/// key rather than committing a duplicate job. The seven operations that are
/// never published derive no identity and are refused here.
async fn publish(
    tx: &Transaction,
    app: &AppId,
    operation: JobOperation,
    available_at: UnixMillis,
    now: i64,
) -> Result<JobSpec, WorkflowServiceError> {
    let job = JobSpec {
        id: publication_id(app, &operation, available_at).ok_or_else(invalid)?,
        app_id: app.clone(),
        operation,
        available_at,
    };
    if let Some(stored) = existing(tx, app, &job.id).await? {
        // The recorded specification wins, not the one just built. A fanout or
        // propagation page is identified by its obligation and revision alone,
        // so an equal observation at a later moment carries a later
        // `available_at` and must still resolve to the job already published.
        let recorded = stored.job(app)?;
        if recorded.operation != job.operation {
            return Err(invalid());
        }
        return Ok(recorded);
    }
    let saved = Box::pin(
        tx.database()
            .entity::<publications::Entity>()?
            .insert::<_, Record>(Record::new(&job, now)?),
    )
    .await?;
    if saved.job(&job.app_id)? != job {
        return Err(invalid());
    }
    Ok(job)
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
    Box::pin(publish(
        tx,
        app,
        JobOperation::Fanout {
            broadcast_id: broadcast.clone(),
            revision,
        },
        now.try_into().map_err(|_| invalid())?,
        now,
    ))
    .await
}

/// Record one propagation page intent. The derived identity returns the same
/// immutable job when an equal page was already recorded.
pub(super) async fn propagate(
    tx: &Transaction,
    app: &AppId,
    propagation: &PropagationId,
    revision: Revision,
    now: i64,
) -> Result<JobSpec, WorkflowServiceError> {
    Box::pin(publish(
        tx,
        app,
        JobOperation::Propagate {
            propagation_id: propagation.clone(),
            revision,
        },
        now.try_into().map_err(|_| invalid())?,
        now,
    ))
    .await
}

/// The app lock prevents new intents while every pending specification is checked.
/// Reading the specification, rather than a denormalized deployment column, is
/// what keeps a damaged intent from concealing a code-dependent publication from
/// release.
pub(super) async fn retains_deployment(
    tx: &Transaction,
    app: &AppId,
    deployment: &str,
) -> Result<bool, WorkflowServiceError> {
    let source = Source::new(tx)?;
    let mut after: Option<String> = None;
    loop {
        let mut predicate = source
            .intents
            .column(publications::app_id)
            .eq(app.as_str())?
            .and(
                source
                    .intents
                    .column(publications::confirmed_at)
                    .eq(None::<i64>)?,
            );
        if let Some(after) = after.as_deref() {
            predicate = predicate.and(source.intents.column(publications::id).gt(after)?);
        }
        let rows: Vec<Record> = source
            .scan(tx)
            .filter(predicate)
            .order_by(source.intents.column(publications::id).asc())
            .select(source.projection())?
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
