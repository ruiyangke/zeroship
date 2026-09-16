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
    models::{
        advance_publications, fanout_publications, job_publications as publications,
        propagation_publications, runs,
    },
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

/// What a reader that has not yet decoded the specification needs. The
/// operation's own projection lives in that operation's table.
#[derive(FromRow, Insertable)]
#[orm(entity = publications)]
struct Intent {
    id: String,
    app_id: String,
    specification: String,
    created_at: i64,
    confirmed_at: Option<i64>,
}

impl Intent {
    fn new(job: &JobSpec, now: i64) -> Result<Self, WorkflowServiceError> {
        Ok(Self {
            id: job.id.as_str().to_owned(),
            app_id: job.app_id.as_str().to_owned(),
            specification: encode(job)?,
            created_at: now,
            confirmed_at: None,
        })
    }
}

/// The frontier an advance intent names. Its due time is part of the identity
/// the scoped unique key deduplicates on, so it is stored here rather than
/// beside the specification.
#[derive(FromRow, Insertable)]
#[orm(entity = advance_publications)]
struct Advance {
    id: String,
    app_id: String,
    deploy_id: String,
    run_id: String,
    generation: i64,
    frontier_revision: i64,
    available_at: i64,
}

#[derive(FromRow, Insertable)]
#[orm(entity = fanout_publications)]
struct Fanout {
    id: String,
    app_id: String,
    broadcast_id: String,
    revision: i64,
}

#[derive(FromRow, Insertable)]
#[orm(entity = propagation_publications)]
struct Propagation {
    id: String,
    app_id: String,
    propagation_id: String,
    revision: i64,
}

/// An intent and the projection of the one operation it publishes. Every column
/// of that projection is required, and its foreign key ties it to this intent,
/// so the only thing left to check is that the row belongs to the operation the
/// specification names.
struct Record {
    intent: Intent,
    advance: Option<Advance>,
    fanout: Option<Fanout>,
    propagation: Option<Propagation>,
}

impl Record {
    fn job(&self, app: &AppId) -> Result<JobSpec, WorkflowServiceError> {
        let job: JobSpec = decode(&self.intent.specification)?;
        if job.app_id != *app
            || self.intent.app_id != app.as_str()
            || job.id.as_str() != self.intent.id
        {
            return Err(invalid());
        }
        let projections = usize::from(self.advance.is_some())
            + usize::from(self.fanout.is_some())
            + usize::from(self.propagation.is_some());
        let valid = projections == 1
            && match (
                &job.operation,
                &self.advance,
                &self.fanout,
                &self.propagation,
            ) {
                (
                    JobOperation::Advance {
                        deployment_id,
                        run_id,
                        generation,
                        revision,
                    },
                    Some(frontier),
                    ..,
                ) => {
                    frontier.deploy_id == deployment_id.as_str()
                        && frontier.run_id == run_id.as_str()
                        && frontier.generation == i64::from(*generation)
                        && frontier.frontier_revision == revision.get()
                        && frontier.available_at == job.available_at.get()
                }
                (
                    JobOperation::Fanout {
                        broadcast_id,
                        revision,
                    },
                    _,
                    Some(page),
                    _,
                ) => page.broadcast_id == broadcast_id.as_str() && page.revision == revision.get(),
                (
                    JobOperation::Propagate {
                        propagation_id,
                        revision,
                    },
                    ..,
                    Some(page),
                ) => {
                    page.propagation_id == propagation_id.as_str()
                        && page.revision == revision.get()
                }
                _ => false,
            };
        if !valid {
            return Err(invalid());
        }
        Ok(job)
    }
}

/// One intent row beside each projection the outer join may or may not find.
type Row = (Intent, Option<Advance>, Option<Fanout>, Option<Propagation>);
type Projection = (
    EntityProjection<publications::Entity, Intent, false>,
    EntityProjection<advance_publications::Entity, Advance, true>,
    EntityProjection<fanout_publications::Entity, Fanout, true>,
    EntityProjection<propagation_publications::Entity, Propagation, true>,
);

/// The intent table outer-joined to all three projections. A read that already
/// knows its operation still joins all three, because reading exactly one of
/// them is what proves the intent publishes exactly one.
struct Sources {
    intents: EntityAlias<publications::Entity>,
    advance: EntityAlias<advance_publications::Entity>,
    fanout: EntityAlias<fanout_publications::Entity>,
    propagation: EntityAlias<propagation_publications::Entity>,
}

impl Sources {
    fn new(tx: &Transaction) -> Result<Self, WorkflowServiceError> {
        Ok(Self {
            intents: tx.database().entity::<publications::Entity>()?.alias("p")?,
            advance: tx
                .database()
                .entity::<advance_publications::Entity>()?
                .alias("a")?,
            fanout: tx
                .database()
                .entity::<fanout_publications::Entity>()?
                .alias("f")?,
            propagation: tx
                .database()
                .entity::<propagation_publications::Entity>()?
                .alias("g")?,
        })
    }

    fn scan(&self, tx: &Transaction) -> Result<ReadBuilder, WorkflowServiceError> {
        Ok(tx
            .database()
            .from(&self.intents)
            .left_join(
                &self.advance,
                self.advance
                    .column(advance_publications::app_id)
                    .eq(self.intents.column(publications::app_id))?
                    .and(
                        self.advance
                            .column(advance_publications::id)
                            .eq(self.intents.column(publications::id))?,
                    ),
            )?
            .left_join(
                &self.fanout,
                self.fanout
                    .column(fanout_publications::app_id)
                    .eq(self.intents.column(publications::app_id))?
                    .and(
                        self.fanout
                            .column(fanout_publications::id)
                            .eq(self.intents.column(publications::id))?,
                    ),
            )?
            .left_join(
                &self.propagation,
                self.propagation
                    .column(propagation_publications::app_id)
                    .eq(self.intents.column(publications::app_id))?
                    .and(
                        self.propagation
                            .column(propagation_publications::id)
                            .eq(self.intents.column(publications::id))?,
                    ),
            )?)
    }

    fn projection(&self) -> Projection {
        (
            self.intents.row::<Intent>(),
            self.advance.optional_row::<Advance>(),
            self.fanout.optional_row::<Fanout>(),
            self.propagation.optional_row::<Propagation>(),
        )
    }
}

fn records(rows: Vec<Row>) -> Vec<Record> {
    rows.into_iter()
        .map(|(intent, advance, fanout, propagation)| Record {
            intent,
            advance,
            fanout,
            propagation,
        })
        .collect()
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
                let sources = Sources::new(&tx)?;
                let mut predicate = sources
                    .intents
                    .column(publications::app_id)
                    .eq(self.app.as_str())?
                    .and(
                        sources
                            .intents
                            .column(publications::confirmed_at)
                            .eq(None::<i64>)?,
                    );
                if let Some(after) = after {
                    predicate = predicate.and(
                        sources
                            .intents
                            .column(publications::id)
                            .gt(after.as_str())?,
                    );
                }
                let rows = sources
                    .scan(&tx)?
                    .filter(predicate)
                    .order_by(sources.intents.column(publications::id).asc())
                    .select(sources.projection())?
                    .limit(i64::from(limit))?
                    .all()
                    .await?;
                let jobs = records(rows)
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
                if intent.intent.confirmed_at.is_some() {
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
                if current.intent.confirmed_at.is_none() {
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
    let sources = Sources::new(tx)?;
    let existing = records(
        sources
            .scan(tx)?
            .filter(
                sources
                    .intents
                    .column(publications::app_id)
                    .eq(app.as_str())?
                    .and(
                        sources
                            .advance
                            .column(advance_publications::run_id)
                            .eq(run)?,
                    )
                    .and(
                        sources
                            .advance
                            .column(advance_publications::generation)
                            .eq(frontier.generation)?,
                    )
                    .and(
                        sources
                            .advance
                            .column(advance_publications::frontier_revision)
                            .eq(frontier.frontier_revision)?,
                    )
                    .and(
                        sources
                            .advance
                            .column(advance_publications::available_at)
                            .eq(due)?,
                    ),
            )
            .select(sources.projection())?
            .limit(1)?
            .all()
            .await?,
    );
    let job = JobSpec {
        id: existing
            .first()
            .map(|row| JobId::parse(&row.intent.id).map_err(|_| invalid()))
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

async fn read(tx: &Transaction, app: &AppId, id: &JobId) -> Result<Record, WorkflowServiceError> {
    let sources = Sources::new(tx)?;
    let rows = sources
        .scan(tx)?
        .filter(
            sources
                .intents
                .column(publications::app_id)
                .eq(app.as_str())?
                .and(sources.intents.column(publications::id).eq(id.as_str())?),
        )
        .select(sources.projection())?
        .limit(1)?
        .all()
        .await?;
    records(rows)
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

/// Write the intent and the one projection its operation owns. The projection's
/// foreign key needs its intent first, and the seven operations that are never
/// published have no projection to write, so they are refused here.
async fn insert(tx: &Transaction, job: &JobSpec, now: i64) -> Result<(), WorkflowServiceError> {
    let id = job.id.as_str().to_owned();
    let app_id = job.app_id.as_str().to_owned();
    let intent = Box::pin(
        tx.database()
            .entity::<publications::Entity>()?
            .insert::<_, Intent>(Intent::new(job, now)?),
    )
    .await?;
    let mut saved = Record {
        intent,
        advance: None,
        fanout: None,
        propagation: None,
    };
    match &job.operation {
        JobOperation::Advance {
            deployment_id,
            run_id,
            generation,
            revision,
        } => {
            saved.advance = Some(
                Box::pin(
                    tx.database()
                        .entity::<advance_publications::Entity>()?
                        .insert::<_, Advance>(Advance {
                            id,
                            app_id,
                            deploy_id: deployment_id.as_str().to_owned(),
                            run_id: run_id.as_str().to_owned(),
                            generation: i64::from(*generation),
                            frontier_revision: revision.get(),
                            available_at: job.available_at.get(),
                        }),
                )
                .await?,
            );
        }
        JobOperation::Fanout {
            broadcast_id,
            revision,
        } => {
            saved.fanout = Some(
                Box::pin(
                    tx.database()
                        .entity::<fanout_publications::Entity>()?
                        .insert::<_, Fanout>(Fanout {
                            id,
                            app_id,
                            broadcast_id: broadcast_id.as_str().to_owned(),
                            revision: revision.get(),
                        }),
                )
                .await?,
            );
        }
        JobOperation::Propagate {
            propagation_id,
            revision,
        } => {
            saved.propagation = Some(
                Box::pin(
                    tx.database()
                        .entity::<propagation_publications::Entity>()?
                        .insert::<_, Propagation>(Propagation {
                            id,
                            app_id,
                            propagation_id: propagation_id.as_str().to_owned(),
                            revision: revision.get(),
                        }),
                )
                .await?,
            );
        }
        _ => return Err(invalid()),
    }
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
    let sources = Sources::new(tx)?;
    let existing = records(
        sources
            .scan(tx)?
            .filter(
                sources
                    .intents
                    .column(publications::app_id)
                    .eq(app.as_str())?
                    .and(
                        sources
                            .fanout
                            .column(fanout_publications::broadcast_id)
                            .eq(broadcast.as_str())?,
                    )
                    .and(
                        sources
                            .fanout
                            .column(fanout_publications::revision)
                            .eq(revision.get())?,
                    ),
            )
            .select(sources.projection())?
            .limit(1)?
            .all()
            .await?,
    );
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
    let sources = Sources::new(tx)?;
    let existing = records(
        sources
            .scan(tx)?
            .filter(
                sources
                    .intents
                    .column(publications::app_id)
                    .eq(app.as_str())?
                    .and(
                        sources
                            .propagation
                            .column(propagation_publications::propagation_id)
                            .eq(propagation.as_str())?,
                    )
                    .and(
                        sources
                            .propagation
                            .column(propagation_publications::revision)
                            .eq(revision.get())?,
                    ),
            )
            .select(sources.projection())?
            .limit(1)?
            .all()
            .await?,
    );
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
/// Reading the specification, rather than the advance projection's deployment,
/// is what keeps a damaged projection from concealing a code-dependent
/// publication from release.
pub(super) async fn retains_deployment(
    tx: &Transaction,
    app: &AppId,
    deployment: &str,
) -> Result<bool, WorkflowServiceError> {
    let sources = Sources::new(tx)?;
    let mut after: Option<String> = None;
    loop {
        let mut predicate = sources
            .intents
            .column(publications::app_id)
            .eq(app.as_str())?
            .and(
                sources
                    .intents
                    .column(publications::confirmed_at)
                    .eq(None::<i64>)?,
            );
        if let Some(after) = after.as_deref() {
            predicate = predicate.and(sources.intents.column(publications::id).gt(after)?);
        }
        let rows = records(
            sources
                .scan(tx)?
                .filter(predicate)
                .order_by(sources.intents.column(publications::id).asc())
                .select(sources.projection())?
                .limit(128)?
                .all()
                .await?,
        );
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
        after = rows.last().map(|row| row.intent.id.clone());
    }
}
