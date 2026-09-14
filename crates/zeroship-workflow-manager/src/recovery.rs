//! Durable reconciliation and collection duties, independent of worker liveness.
//!
//! A scope's responsibility carries a monotonic ingress epoch and a state: open,
//! closing or retired. Closing records the app's dispatch cursor as a watermark
//! and publishes a manager-origin Close job for the current epoch. Settlement of
//! that job retires responsibility only when the creator reported drained
//! evidence and no other job was published or claimed above the watermark.
//! Retirement deletes the duty pair and keeps the scope row as the epoch's
//! tombstone, so a reopened scope always continues at the next epoch.
#![expect(
    clippy::future_not_send,
    reason = "recovery shares the manager's owning compio runtime"
)]

use crate::{
    models::{jobs, queue_scopes, recovery_duties, recovery_scopes, Scope},
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
use zeroship_data_orm::orm::{Database, FindOptions, FromRow, Insertable};

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

/// Where a scope's responsibility stands. Only an open scope dispatches its
/// periodic duties; a retired scope has none until an establishment, an
/// intent-producing claim or a worker publication reopens it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeState {
    Open,
    Closing,
    Retired,
}

impl ScopeState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closing => "closing",
            Self::Retired => "retired",
        }
    }

    fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "open" => Ok(Self::Open),
            "closing" => Ok(Self::Closing),
            "retired" => Ok(Self::Retired),
            _ => Err(Error::Storage),
        }
    }
}

/// A scope's current recovery responsibility.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Responsibility {
    pub state: ScopeState,
    pub ingress_epoch: Revision,
    pub closing_watermark: Option<i64>,
    pub close_job: Option<JobId>,
    pub last_ingress_at: Option<i64>,
}

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub interval: Duration,
    pub page_size: u32,
    /// A closing attempt whose Close job has not settled within this bound
    /// returns the scope to open, so its periodic duties resume.
    pub closing_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(30),
            page_size: 128,
            closing_timeout: Duration::from_secs(300),
        }
    }
}

/// The trusted platform host registers responsibility before enabling ingress.
///
/// Worker registration, placement and heartbeat operations cannot postpone it.
/// Registration expiry, release, empty polling and heartbeats never retire it;
/// only a settled Close with drained evidence below its watermark does.
#[derive(Debug, Clone)]
pub struct Recovery {
    queue: Queue,
    interval_ms: i64,
    closing_timeout_ms: i64,
    page_size: u32,
}

#[derive(FromRow)]
#[orm(entity = recovery_scopes)]
struct Stored {
    deployment_id: String,
    activation_revision: i64,
    ingress_epoch: i64,
    state: String,
    closing_watermark: Option<i64>,
    close_job_id: Option<String>,
    last_ingress_at: Option<i64>,
}

impl Stored {
    fn responsibility(&self) -> Result<Responsibility, Error> {
        validate_provenance(self)?;
        let state = ScopeState::parse(&self.state)?;
        let ingress_epoch = Revision::try_from(self.ingress_epoch).map_err(|_| Error::Storage)?;
        let close_job = self
            .close_job_id
            .as_deref()
            .map(JobId::parse)
            .transpose()
            .map_err(|_| Error::Storage)?;
        let consistent = match state {
            ScopeState::Closing => {
                self.closing_watermark.is_some_and(|watermark| watermark >= 0)
                    && close_job.is_some()
            }
            ScopeState::Open | ScopeState::Retired => {
                self.closing_watermark.is_none() && close_job.is_none()
            }
        };
        if !consistent || self.last_ingress_at.is_some_and(|at| at < 0) {
            return Err(Error::Storage);
        }
        Ok(Responsibility {
            state,
            ingress_epoch,
            closing_watermark: self.closing_watermark,
            close_job,
            last_ingress_at: self.last_ingress_at,
        })
    }
}

#[derive(FromRow)]
#[orm(entity = recovery_scopes)]
struct ScopeId {
    id: String,
}

#[derive(Insertable)]
#[orm(entity = recovery_scopes)]
struct NewScope<'a> {
    id: &'a str,
    deployment_id: &'a str,
    activation_revision: i64,
    ingress_epoch: i64,
    state: &'a str,
    closing_watermark: Option<i64>,
    close_job_id: Option<&'a str>,
    last_ingress_at: Option<i64>,
}

impl Recovery {
    /// # Errors
    /// Rejects unrepresentable intervals and pages outside the ORM row bound.
    pub fn new(queue: Queue, options: Options) -> Result<Self, Error> {
        let millis = |value: Duration| {
            i64::try_from(value.as_millis())
                .ok()
                .filter(|value| *value > 0)
                .ok_or(Error::Invalid)
        };
        let interval_ms = millis(options.interval)?;
        let closing_timeout_ms = millis(options.closing_timeout)?;
        if options.page_size == 0
            || i64::from(options.page_size) > zeroship_data_orm::sql::MAX_ROW_LIMIT
        {
            return Err(Error::Invalid);
        }
        Ok(Self {
            queue,
            interval_ms,
            closing_timeout_ms,
            page_size: options.page_size,
        })
    }

    /// Persist responsibility using a platform-authorized deployment activation.
    /// Repeated registration preserves the deadline. A newer activation changes
    /// only activation provenance; pending duties keep their identities. An
    /// activation of a retired scope reopens it at the next ingress epoch.
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
                let now = self.queue.clock.now().await?;
                Box::pin(ensure_in(&tx, app, deployment, activation_revision, now)).await
            })
            .await
    }

    /// Read the current responsibility without changing it.
    ///
    /// # Errors
    /// Reports unavailable or malformed platform storage.
    pub async fn responsibility(&self, app: &AppId) -> Result<Option<Responsibility>, Error> {
        self.queue
            .transact(|tx| async move {
                load(&tx, app)
                    .await?
                    .map(|stored| stored.responsibility())
                    .transpose()
            })
            .await
    }

    /// Page due obligations of open scopes by app identity, including apps with
    /// healthy owners. Continue after the last returned app and restart a sweep
    /// from the beginning after an empty page. Work that becomes due behind the
    /// cursor joins that sweep. Closing and retired scopes dispatch no duty.
    ///
    /// # Errors
    /// Reports unavailable or malformed platform storage.
    pub async fn due(&self, kind: DutyKind, after: Option<&AppId>) -> Result<Vec<AppId>, Error> {
        self.queue
            .transact(|tx| async move {
                let duty = tx.entity::<recovery_duties::Entity>()?.alias("duty")?;
                let scope = tx.entity::<recovery_scopes::Entity>()?.alias("scope")?;
                let mut filter = duty
                    .column(recovery_duties::kind)
                    .eq(kind.as_str())?
                    .and(
                        duty.column(recovery_duties::next_due_at)
                            .lte(self.queue.clock.now().await?)?,
                    )
                    .and(
                        scope
                            .column(recovery_scopes::state)
                            .eq(ScopeState::Open.as_str())?,
                    );
                if let Some(after) = after {
                    filter = filter.and(duty.column(recovery_duties::app_id).gt(after.as_str())?);
                }
                tx.from(&duty)
                    .inner_join(
                        &scope,
                        duty.column(recovery_duties::app_id)
                            .eq(scope.column(recovery_scopes::id))?,
                    )?
                    .filter(filter)
                    .order_by(duty.column(recovery_duties::app_id).asc())
                    .select(duty.row::<Duty>())?
                    .limit(i64::from(self.page_size))?
                    .all()
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
    /// loss of all workers. Failed publication leaves responsibility due. A
    /// closing or retired scope dispatches nothing: the Close job's drain
    /// predicates subsume its duties for the attempt.
    ///
    /// # Errors
    /// Refuses an unregistered scope, exhausted time range and storage failures.
    pub async fn dispatch(&self, app: &AppId, kind: DutyKind) -> Result<Option<JobSpec>, Error> {
        self.queue
            .transact(|tx| async move {
                queue::lock_scope(&tx, app).await?;
                let stored = load(&tx, app).await?.ok_or(Error::Denied)?;
                if stored.responsibility()?.state != ScopeState::Open {
                    return Ok(None);
                }
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

    /// Begin closing an open scope. The closure trigger (archive or idleness) is
    /// the caller's decision; this operation owns the safety preconditions: no
    /// job of the app holds a live lease and no maintenance job is pending. It
    /// records the dispatch cursor as the closing watermark and publishes Close
    /// for the current epoch under the app lock. A closing scope returns its
    /// existing Close job, so replicas converge on one attempt.
    ///
    /// # Errors
    /// Refuses an unregistered scope and reports unavailable storage.
    pub async fn begin_close(&self, app: &AppId) -> Result<Option<JobSpec>, Error> {
        self.queue
            .transact(|tx| async move {
                queue::lock_scope(&tx, app).await?;
                let current = load(&tx, app)
                    .await?
                    .ok_or(Error::Denied)?
                    .responsibility()?;
                match current.state {
                    ScopeState::Retired => return Ok(None),
                    ScopeState::Closing => {
                        let id = current.close_job.ok_or(Error::Storage)?;
                        let job = queue::load(&tx, app, id.as_str())
                            .await?
                            .ok_or(Error::Storage)?;
                        return Ok(Some(job.spec()?));
                    }
                    ScopeState::Open => {}
                }
                let now = self.queue.clock.now().await?;
                if leased(&tx, app, now, None).await? || pending_maintenance(&tx, app).await? {
                    return Ok(None);
                }
                let watermark = dispatch_cursor(&tx, app).await?;
                let spec = JobSpec {
                    id: JobId::mint(),
                    app_id: app.clone(),
                    operation: JobOperation::Close {
                        epoch: current.ingress_epoch,
                    },
                    available_at: now.try_into().map_err(|_| Error::Storage)?,
                };
                self.queue.insert(&tx, &spec, now).await?;
                transition(
                    &tx,
                    app,
                    &current,
                    recovery_scopes::state
                        .set(ScopeState::Closing.as_str())?
                        .and(recovery_scopes::closing_watermark.set(Some(watermark))?)?
                        .and(recovery_scopes::close_job_id.set(Some(spec.id.as_str()))?)?,
                )
                .await?;
                Ok(Some(spec))
            })
            .await
    }

    /// Return a closing attempt whose Close job has not settled within the
    /// closing timeout to open, resuming its duties. The unsettled Close keeps
    /// its identity; its later settlement no longer matches the scope.
    ///
    /// # Errors
    /// Refuses an unregistered scope and reports unavailable storage.
    pub async fn expire_close(&self, app: &AppId) -> Result<bool, Error> {
        self.queue
            .transact(|tx| async move {
                queue::lock_scope(&tx, app).await?;
                let current = load(&tx, app)
                    .await?
                    .ok_or(Error::Denied)?
                    .responsibility()?;
                if current.state != ScopeState::Closing {
                    return Ok(false);
                }
                let id = current.close_job.as_ref().ok_or(Error::Storage)?;
                let job = queue::load(&tx, app, id.as_str())
                    .await?
                    .ok_or(Error::Storage)?;
                let expires = job
                    .created_at
                    .checked_add(self.closing_timeout_ms)
                    .ok_or(Error::Storage)?;
                if expires > self.queue.clock.now().await? {
                    return Ok(false);
                }
                transition(&tx, app, &current, reopened_attempt()?).await?;
                Ok(true)
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
        let current = stored.responsibility()?;
        if current.state == ScopeState::Retired {
            Box::pin(reopen(tx, app, &current, now)).await?;
        } else {
            duties::validate_pair(tx, app).await?;
        }
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
            .insert::<_, ScopeId>(NewScope {
                id: app.as_str(),
                deployment_id: deployment.as_str(),
                activation_revision: activation_revision.get(),
                ingress_epoch: 1,
                state: ScopeState::Open.as_str(),
                closing_watermark: None,
                close_job_id: None,
                last_ingress_at: None,
            })
            .await?;
        if created.id != app.as_str() {
            return Err(Error::Storage);
        }
        duties::create_pair(tx, app, now).await?;
    }
    Ok(())
}

/// Policy issuance calls this under the app lock, after the placement and
/// enrollment re-checks and before commit, so responsibility is durable before
/// the lease reaches the worker.
///
/// A plain refresh never reopens responsibility: it reports the current epoch
/// while the scope is open or closing and none once it retired. `establish_after`
/// guarantees an open epoch above the named one. It reopens a retired scope,
/// cancels a closing one, or advances an open epoch the creator already fenced,
/// and only while the observed policy admits work. Retries after a lost reply
/// find the epoch already above the named one and return it unchanged.
pub(crate) async fn lease_epoch_in(
    tx: &Database,
    app: &AppId,
    establish_after: Option<Revision>,
    ingress_used: bool,
    admission: bool,
    now: i64,
) -> Result<Option<Revision>, Error> {
    let Some(stored) = load(tx, app).await? else {
        // Nothing to establish before activation creates the scope.
        return if establish_after.is_some() {
            Err(Error::Conflict)
        } else {
            Ok(None)
        };
    };
    let current = stored.responsibility()?;
    if ingress_used {
        let changed = tx
            .entity::<recovery_scopes::Entity>()?
            .update_many(
                recovery_scopes::id.eq(app.as_str())?,
                recovery_scopes::last_ingress_at.set(Some(now))?,
            )
            .await?;
        if changed != 1 {
            return Err(Error::Storage);
        }
    }
    let Some(after) = establish_after else {
        return Ok((current.state != ScopeState::Retired).then_some(current.ingress_epoch));
    };
    // The manager never issued a later epoch; refuse rather than inflate it.
    if after > current.ingress_epoch {
        return Err(Error::Conflict);
    }
    if current.state == ScopeState::Open && current.ingress_epoch > after {
        return Ok(Some(current.ingress_epoch));
    }
    if !admission {
        return Err(Error::Denied);
    }
    Box::pin(reopen(tx, app, &current, now)).await.map(Some)
}

/// Claim-time re-arm, under the claim transaction's app lock. A claimed
/// intent-producing job reopens a retired scope before the worker executes it.
/// A closing scope needs no change here: the claim moved the job above the
/// closing watermark, which cancels that attempt's retirement at settlement.
pub(crate) async fn claimed_in(tx: &Database, job: &JobSpec, now: i64) -> Result<(), Error> {
    if job.produces_intents() {
        Box::pin(rearm_retired(tx, &job.app_id, now)).await?;
    }
    Ok(())
}

/// Publication re-arm, under the submission's app lock. A worker publication to
/// a retired scope indicates stale evidence or a restored creator journal.
pub(crate) async fn published_in(tx: &Database, app: &AppId, now: i64) -> Result<(), Error> {
    Box::pin(rearm_retired(tx, app, now)).await
}

async fn rearm_retired(tx: &Database, app: &AppId, now: i64) -> Result<(), Error> {
    let Some(stored) = load(tx, app).await? else {
        return Ok(());
    };
    let current = stored.responsibility()?;
    if current.state == ScopeState::Retired {
        Box::pin(reopen(tx, app, &current, now)).await?;
    }
    Ok(())
}

/// The queue calls this only for a fresh Close settlement under its app lock,
/// after marking the job settled. Retirement needs the matching closing
/// attempt at the job's epoch, drained creator evidence, no other job published
/// or claimed above the closing watermark, no other live lease and no pending
/// maintenance job. Any other fresh settlement of the current attempt returns
/// the scope to open. A stale attempt's settlement changes nothing.
pub(crate) async fn settled_close(
    tx: &Database,
    job: &JobSpec,
    outcome: JobOutcome,
    now: i64,
) -> Result<(), Error> {
    let JobOperation::Close { epoch } = job.operation else {
        return Ok(());
    };
    let JobOutcome::Closed { drained } = outcome else {
        return Err(Error::Invalid);
    };
    let current = load(tx, &job.app_id)
        .await?
        .ok_or(Error::Storage)?
        .responsibility()?;
    if current.state != ScopeState::Closing || current.close_job.as_ref() != Some(&job.id) {
        return Ok(());
    }
    let watermark = current.closing_watermark.ok_or(Error::Storage)?;
    let retire = drained
        && current.ingress_epoch == epoch
        && !claimed_after(tx, &job.app_id, watermark, &job.id).await?
        && !leased(tx, &job.app_id, now, Some(&job.id)).await?
        && !pending_maintenance(tx, &job.app_id).await?;
    if !retire {
        return transition(tx, &job.app_id, &current, reopened_attempt()?).await;
    }
    tx.entity::<recovery_duties::Entity>()?
        .delete_many(recovery_duties::app_id.eq(job.app_id.as_str())?)
        .await?;
    transition(
        tx,
        &job.app_id,
        &current,
        recovery_scopes::state
            .set(ScopeState::Retired.as_str())?
            .and(recovery_scopes::closing_watermark.set(None::<i64>)?)?
            .and(recovery_scopes::close_job_id.set(None::<&str>)?)?,
    )
    .await
}

/// The caller holds the queue app lock. Open the epoch after the current one,
/// cancelling a closing attempt; a retired scope recreates its duty pair.
async fn reopen(
    tx: &Database,
    app: &AppId,
    current: &Responsibility,
    now: i64,
) -> Result<Revision, Error> {
    let next = current
        .ingress_epoch
        .get()
        .checked_add(1)
        .ok_or(Error::Capacity)?;
    transition(
        tx,
        app,
        current,
        recovery_scopes::ingress_epoch
            .set(next)?
            .and(recovery_scopes::state.set(ScopeState::Open.as_str())?)?
            .and(recovery_scopes::closing_watermark.set(None::<i64>)?)?
            .and(recovery_scopes::close_job_id.set(None::<&str>)?)?,
    )
    .await?;
    if current.state == ScopeState::Retired {
        duties::create_pair(tx, app, now).await?;
    }
    Revision::try_from(next).map_err(|_| Error::Storage)
}

fn reopened_attempt(
) -> Result<zeroship_data_orm::orm::Patch<recovery_scopes::Entity>, Error> {
    Ok(recovery_scopes::state
        .set(ScopeState::Open.as_str())?
        .and(recovery_scopes::closing_watermark.set(None::<i64>)?)?
        .and(recovery_scopes::close_job_id.set(None::<&str>)?)?)
}

/// Apply a change to exactly the observed state and epoch.
async fn transition(
    tx: &Database,
    app: &AppId,
    current: &Responsibility,
    patch: zeroship_data_orm::orm::Patch<recovery_scopes::Entity>,
) -> Result<(), Error> {
    let changed = tx
        .entity::<recovery_scopes::Entity>()?
        .update_many(
            recovery_scopes::id
                .eq(app.as_str())?
                .and(recovery_scopes::ingress_epoch.eq(current.ingress_epoch.get())?)
                .and(recovery_scopes::state.eq(current.state.as_str())?),
            patch,
        )
        .await?;
    if changed != 1 {
        return Err(Error::Storage);
    }
    Ok(())
}

/// A job published or claimed after closing began holds a dispatch ticket
/// above the watermark, even after it settles.
async fn claimed_after(
    tx: &Database,
    app: &AppId,
    watermark: i64,
    close: &JobId,
) -> Result<bool, Error> {
    Ok(tx
        .entity::<jobs::Entity>()?
        .exists(
            jobs::app_id
                .eq(app.as_str())?
                .and(jobs::dispatch_order.gt(watermark)?)
                .and(jobs::id.ne(close.as_str())?),
        )
        .await?)
}

async fn leased(
    tx: &Database,
    app: &AppId,
    now: i64,
    except: Option<&JobId>,
) -> Result<bool, Error> {
    let mut filter = jobs::app_id
        .eq(app.as_str())?
        .and(jobs::state.eq("leased")?)
        .and(jobs::lease_deadline.gt(Some(now))?);
    if let Some(except) = except {
        filter = filter.and(jobs::id.ne(except.as_str())?);
    }
    Ok(tx.entity::<jobs::Entity>()?.exists(filter).await?)
}

/// A pending duty job, or an unsettled Close from an earlier attempt.
async fn pending_maintenance(tx: &Database, app: &AppId) -> Result<bool, Error> {
    for kind in [DutyKind::Reconcile, DutyKind::Collect] {
        if let Some(duty) = duties::load(tx, app, kind).await? {
            if duty.pending(tx, app, kind).await?.is_some() {
                return Ok(true);
            }
        }
    }
    Ok(tx
        .entity::<jobs::Entity>()?
        .exists(
            jobs::app_id
                .eq(app.as_str())?
                .and(jobs::operation_kind.eq("close")?)
                .and(jobs::state.ne("settled")?),
        )
        .await?)
}

async fn dispatch_cursor(tx: &Database, app: &AppId) -> Result<i64, Error> {
    let scope = tx
        .entity::<queue_scopes::Entity>()?
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
    if scope.dispatch_cursor < 0 {
        return Err(Error::Storage);
    }
    Ok(scope.dispatch_cursor)
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
